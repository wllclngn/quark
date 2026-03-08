// SPDX-License-Identifier: GPL-2.0
/*
 * triskelion kernel module — ioctl dispatch.
 *
 * Maps ioctl commands to handler functions. Each handler copies args
 * from userspace, operates on the server context, and copies results back.
 */

#include <linux/uaccess.h>
#include <linux/slab.h>

#include "triskelion.h"
#include "triskelion_internal.h"

/* ── Sync object creation ───────────────────────────────────────────── */

static long do_create_sem(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_sem_args args;
	struct triskelion_semaphore *sem;
	triskelion_handle_t handle;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	sem = triskelion_sem_create(args.count, args.max_count);
	if (IS_ERR(sem))
		return PTR_ERR(sem);

	handle = triskelion_handle_alloc(&ctx->handles,
					 TRISKELION_OBJ_SEMAPHORE, sem);
	if (handle == TRISKELION_INVALID_HANDLE) {
		triskelion_sem_destroy(sem);
		return -ENOMEM;
	}

	args.handle = handle;
	if (copy_to_user(uarg, &args, sizeof(args))) {
		triskelion_handle_close(&ctx->handles, handle);
		return -EFAULT;
	}

	return 0;
}

static long do_create_mutex(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_mutex_args args;
	struct triskelion_mutex *mtx;
	triskelion_handle_t handle;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	mtx = triskelion_mutex_create(args.owner_tid);
	if (IS_ERR(mtx))
		return PTR_ERR(mtx);

	handle = triskelion_handle_alloc(&ctx->handles,
					 TRISKELION_OBJ_MUTEX, mtx);
	if (handle == TRISKELION_INVALID_HANDLE) {
		triskelion_mutex_destroy(mtx);
		return -ENOMEM;
	}

	args.handle = handle;
	if (copy_to_user(uarg, &args, sizeof(args))) {
		triskelion_handle_close(&ctx->handles, handle);
		return -EFAULT;
	}

	return 0;
}

static long do_create_event(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_event_args args;
	struct triskelion_event *evt;
	triskelion_handle_t handle;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	evt = triskelion_event_create(args.manual_reset, args.initial_state);
	if (IS_ERR(evt))
		return PTR_ERR(evt);

	handle = triskelion_handle_alloc(&ctx->handles,
					 TRISKELION_OBJ_EVENT, evt);
	if (handle == TRISKELION_INVALID_HANDLE) {
		triskelion_event_destroy(evt);
		return -ENOMEM;
	}

	args.handle = handle;
	if (copy_to_user(uarg, &args, sizeof(args))) {
		triskelion_handle_close(&ctx->handles, handle);
		return -EFAULT;
	}

	return 0;
}

/* ── Sync operations ────────────────────────────────────────────────── */

static long do_release_sem(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_sem_args args;
	struct triskelion_object *obj;
	int ret;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	obj = triskelion_handle_get(&ctx->handles, args.handle);
	if (!obj || obj->type != TRISKELION_OBJ_SEMAPHORE)
		return -EINVAL;

	ret = triskelion_sem_release(obj->data, args.count, &args.prev_count);
	if (ret)
		return ret;

	if (copy_to_user(uarg, &args, sizeof(args)))
		return -EFAULT;

	return 0;
}

static long do_release_mutex(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_mutex_args args;
	struct triskelion_object *obj;
	int ret;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	obj = triskelion_handle_get(&ctx->handles, args.handle);
	if (!obj || obj->type != TRISKELION_OBJ_MUTEX)
		return -EINVAL;

	ret = triskelion_mutex_release(obj->data, args.owner_tid,
				       &args.prev_count);
	if (ret)
		return ret;

	if (copy_to_user(uarg, &args, sizeof(args)))
		return -EFAULT;

	return 0;
}

static long do_set_event(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_event_args args;
	struct triskelion_object *obj;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	obj = triskelion_handle_get(&ctx->handles, args.handle);
	if (!obj || obj->type != TRISKELION_OBJ_EVENT)
		return -EINVAL;

	triskelion_event_set(obj->data, &args.prev_state);

	if (copy_to_user(uarg, &args, sizeof(args)))
		return -EFAULT;

	return 0;
}

static long do_reset_event(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_event_args args;
	struct triskelion_object *obj;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	obj = triskelion_handle_get(&ctx->handles, args.handle);
	if (!obj || obj->type != TRISKELION_OBJ_EVENT)
		return -EINVAL;

	triskelion_event_reset(obj->data, &args.prev_state);

	if (copy_to_user(uarg, &args, sizeof(args)))
		return -EFAULT;

	return 0;
}

static long do_pulse_event(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_event_args args;
	struct triskelion_object *obj;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	obj = triskelion_handle_get(&ctx->handles, args.handle);
	if (!obj || obj->type != TRISKELION_OBJ_EVENT)
		return -EINVAL;

	triskelion_event_pulse(obj->data, &args.prev_state);

	if (copy_to_user(uarg, &args, sizeof(args)))
		return -EFAULT;

	return 0;
}

/* ── Message queue ──────────────────────────────────────────────────── */

static long do_post_msg(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_post_msg_args args;
	struct triskelion_msg_queue *q;

	if (copy_from_user(&args, uarg, sizeof(args)))
		return -EFAULT;

	q = triskelion_queue_get_or_create(&ctx->queues, args.target_tid);
	if (IS_ERR(q))
		return PTR_ERR(q);

	return triskelion_queue_post(q, &args.msg);
}

static long do_get_msg(struct triskelion_ctx *ctx, void __user *uarg)
{
	struct triskelion_get_msg_args args = {};
	struct triskelion_msg_queue *q;
	int ret;

	q = triskelion_queue_get_or_create(&ctx->queues, current->pid);
	if (IS_ERR(q))
		return PTR_ERR(q);

	ret = triskelion_queue_get(q, &args.msg);
	args.has_message = (ret == 0) ? 1 : 0;

	if (copy_to_user(uarg, &args, sizeof(args)))
		return -EFAULT;

	return 0;
}

/* ── Handle ops ─────────────────────────────────────────────────────── */

static long do_close(struct triskelion_ctx *ctx, void __user *uarg)
{
	triskelion_handle_t handle;

	if (copy_from_user(&handle, uarg, sizeof(handle)))
		return -EFAULT;

	return triskelion_handle_close(&ctx->handles, handle);
}

/* ── Dispatch table ─────────────────────────────────────────────────── */

long triskelion_dispatch(struct triskelion_ctx *ctx, unsigned int cmd,
			 unsigned long arg)
{
	void __user *uarg = (void __user *)arg;

	switch (cmd) {
	/* Sync creation */
	case TRISKELION_IOC_CREATE_SEM:    return do_create_sem(ctx, uarg);
	case TRISKELION_IOC_CREATE_MUTEX:  return do_create_mutex(ctx, uarg);
	case TRISKELION_IOC_CREATE_EVENT:  return do_create_event(ctx, uarg);

	/* Sync operations */
	case TRISKELION_IOC_RELEASE_SEM:   return do_release_sem(ctx, uarg);
	case TRISKELION_IOC_RELEASE_MUTEX: return do_release_mutex(ctx, uarg);
	case TRISKELION_IOC_SET_EVENT:     return do_set_event(ctx, uarg);
	case TRISKELION_IOC_RESET_EVENT:   return do_reset_event(ctx, uarg);
	case TRISKELION_IOC_PULSE_EVENT:   return do_pulse_event(ctx, uarg);

	/* Message queue */
	case TRISKELION_IOC_POST_MSG:      return do_post_msg(ctx, uarg);
	case TRISKELION_IOC_GET_MSG:       return do_get_msg(ctx, uarg);

	/* Handle ops */
	case TRISKELION_IOC_CLOSE:         return do_close(ctx, uarg);

	default:
		return -ENOTTY;
	}
}
