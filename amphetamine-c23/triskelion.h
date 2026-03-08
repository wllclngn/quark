/* SPDX-License-Identifier: GPL-2.0 */
/*
 * triskelion kernel module — wineserver in kernel space.
 *
 * Wine processes open /dev/triskelion and issue ioctls for every wineserver
 * call. Handle tables, sync primitives, message queues, and thread state
 * live in kernel memory. No context switch to a userspace daemon.
 *
 * Quocunque Jeceris Stabit.
 */
#ifndef TRISKELION_H
#define TRISKELION_H

#include <linux/types.h>
#include <linux/ioctl.h>

#define TRISKELION_DEVICE_NAME  "triskelion"
#define TRISKELION_IOCTL_MAGIC  'T'

/* ── Handle system ──────────────────────────────────────────────────── */

typedef __u32 triskelion_handle_t;

#define TRISKELION_INVALID_HANDLE  ((triskelion_handle_t)0)

enum triskelion_obj_type {
	TRISKELION_OBJ_PROCESS,
	TRISKELION_OBJ_THREAD,
	TRISKELION_OBJ_SEMAPHORE,
	TRISKELION_OBJ_MUTEX,
	TRISKELION_OBJ_EVENT,
	TRISKELION_OBJ_TIMER,
	TRISKELION_OBJ_KEY,         /* registry key */
	TRISKELION_OBJ_COUNT,
};

/* ── Sync primitives (extends ntsync pattern) ───────────────────────── */

struct triskelion_sem_args {
	triskelion_handle_t handle;
	__u32 count;
	__u32 max_count;
	__u32 prev_count;       /* out */
};

struct triskelion_mutex_args {
	triskelion_handle_t handle;
	__u32 owner_tid;
	__u32 count;
	__u32 prev_count;       /* out */
};

struct triskelion_event_args {
	triskelion_handle_t handle;
	__u32 manual_reset;
	__u32 initial_state;
	__u32 prev_state;       /* out */
};

/* ── Wait ───────────────────────────────────────────────────────────── */

struct triskelion_wait_args {
	const triskelion_handle_t *handles;
	__u32 count;
	__u32 wait_all;         /* 0 = WaitAny, 1 = WaitAll */
	__s64 timeout_ns;       /* negative = relative, 0 = poll, positive = absolute */
	__u32 signaled_index;   /* out */
};

/* ── Message queue ──────────────────────────────────────────────────── */

struct triskelion_msg {
	__u32 msg;
	__u32 wparam_lo;
	__u32 wparam_hi;
	__u32 lparam_lo;
	__u32 lparam_hi;
	__u32 time;
	__u32 info;
};

struct triskelion_post_msg_args {
	__u32 target_tid;
	struct triskelion_msg msg;
};

struct triskelion_get_msg_args {
	struct triskelion_msg msg;  /* out */
	__u32 has_message;          /* out */
};

/* ── Process / thread ───────────────────────────────────────────────── */

struct triskelion_new_process_args {
	triskelion_handle_t process_handle;  /* out */
	triskelion_handle_t thread_handle;   /* out */
	__u32 pid;
	__u32 tid;
};

struct triskelion_new_thread_args {
	triskelion_handle_t handle;  /* out */
	__u32 tid;
	__u32 process_handle;
};

/* ── ioctls ─────────────────────────────────────────────────────────── */

/* Process/thread lifecycle */
#define TRISKELION_IOC_NEW_PROCESS   _IOWR(TRISKELION_IOCTL_MAGIC, 0x00, struct triskelion_new_process_args)
#define TRISKELION_IOC_NEW_THREAD    _IOWR(TRISKELION_IOCTL_MAGIC, 0x01, struct triskelion_new_thread_args)

/* Sync object creation */
#define TRISKELION_IOC_CREATE_SEM    _IOWR(TRISKELION_IOCTL_MAGIC, 0x10, struct triskelion_sem_args)
#define TRISKELION_IOC_CREATE_MUTEX  _IOWR(TRISKELION_IOCTL_MAGIC, 0x11, struct triskelion_mutex_args)
#define TRISKELION_IOC_CREATE_EVENT  _IOWR(TRISKELION_IOCTL_MAGIC, 0x12, struct triskelion_event_args)

/* Sync operations */
#define TRISKELION_IOC_RELEASE_SEM   _IOWR(TRISKELION_IOCTL_MAGIC, 0x18, struct triskelion_sem_args)
#define TRISKELION_IOC_RELEASE_MUTEX _IOWR(TRISKELION_IOCTL_MAGIC, 0x19, struct triskelion_mutex_args)
#define TRISKELION_IOC_SET_EVENT     _IOWR(TRISKELION_IOCTL_MAGIC, 0x1A, struct triskelion_event_args)
#define TRISKELION_IOC_RESET_EVENT   _IOWR(TRISKELION_IOCTL_MAGIC, 0x1B, struct triskelion_event_args)
#define TRISKELION_IOC_PULSE_EVENT   _IOWR(TRISKELION_IOCTL_MAGIC, 0x1C, struct triskelion_event_args)

/* Wait */
#define TRISKELION_IOC_WAIT_ANY      _IOWR(TRISKELION_IOCTL_MAGIC, 0x20, struct triskelion_wait_args)
#define TRISKELION_IOC_WAIT_ALL      _IOWR(TRISKELION_IOCTL_MAGIC, 0x21, struct triskelion_wait_args)

/* Message queue */
#define TRISKELION_IOC_POST_MSG      _IOW (TRISKELION_IOCTL_MAGIC, 0x30, struct triskelion_post_msg_args)
#define TRISKELION_IOC_GET_MSG       _IOR (TRISKELION_IOCTL_MAGIC, 0x31, struct triskelion_get_msg_args)

/* Handle ops */
#define TRISKELION_IOC_CLOSE         _IOW (TRISKELION_IOCTL_MAGIC, 0x40, triskelion_handle_t)
#define TRISKELION_IOC_DUP           _IOWR(TRISKELION_IOCTL_MAGIC, 0x41, triskelion_handle_t)

#endif /* TRISKELION_H */
