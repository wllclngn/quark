// SPDX-License-Identifier: GPL-2.0
/*
 * triskelion kernel module — sync primitives.
 *
 * NT semaphore, mutex, and event implemented with kernel wait queues.
 * Same semantics as ntsync but integrated into the triskelion handle
 * table. No separate /dev/ntsync device needed.
 *
 * Events use atomic_t for signaled state — no spinlock needed.
 * Semaphores use atomic_cmpxchg for lock-free release.
 * Mutexes keep a spinlock (owner_tid + count must update together).
 * All types allocated from dedicated slab caches.
 */

#include <linux/slab.h>
#include <linux/sched.h>

#include "triskelion_internal.h"

static struct kmem_cache *sem_cache;
static struct kmem_cache *mtx_cache;
static struct kmem_cache *evt_cache;

int triskelion_sync_init(void)
{
	sem_cache = kmem_cache_create("triskelion_sem",
		sizeof(struct triskelion_semaphore), 0, 0, NULL);
	if (!sem_cache)
		return -ENOMEM;

	mtx_cache = kmem_cache_create("triskelion_mtx",
		sizeof(struct triskelion_mutex), 0, 0, NULL);
	if (!mtx_cache) {
		kmem_cache_destroy(sem_cache);
		return -ENOMEM;
	}

	evt_cache = kmem_cache_create("triskelion_evt",
		sizeof(struct triskelion_event), 0, 0, NULL);
	if (!evt_cache) {
		kmem_cache_destroy(mtx_cache);
		kmem_cache_destroy(sem_cache);
		return -ENOMEM;
	}

	return 0;
}

void triskelion_sync_exit(void)
{
	kmem_cache_destroy(evt_cache);
	kmem_cache_destroy(mtx_cache);
	kmem_cache_destroy(sem_cache);
}

/* ── Semaphore ──────────────────────────────────────────────────────── */

struct triskelion_semaphore *triskelion_sem_create(u32 initial, u32 max)
{
	struct triskelion_semaphore *sem;

	if (initial > max || max == 0)
		return ERR_PTR(-EINVAL);

	sem = kmem_cache_zalloc(sem_cache, GFP_KERNEL);
	if (!sem)
		return ERR_PTR(-ENOMEM);

	atomic_set(&sem->count, initial);
	sem->max_count = max;
	init_waitqueue_head(&sem->wq);

	return sem;
}

int triskelion_sem_release(struct triskelion_semaphore *sem, u32 count, u32 *prev)
{
	int old, new;

	do {
		old = atomic_read(&sem->count);
		new = old + count;
		if ((u32)new > sem->max_count) {
			*prev = old;
			return -EOVERFLOW;
		}
	} while (atomic_cmpxchg(&sem->count, old, new) != old);

	*prev = old;
	wake_up_interruptible(&sem->wq);
	return 0;
}

void triskelion_sem_destroy(struct triskelion_semaphore *sem)
{
	wake_up_all(&sem->wq);
	kmem_cache_free(sem_cache, sem);
}

/* ── Mutex ──────────────────────────────────────────────────────────── */

struct triskelion_mutex *triskelion_mutex_create(u32 owner_tid)
{
	struct triskelion_mutex *mtx;

	mtx = kmem_cache_zalloc(mtx_cache, GFP_KERNEL);
	if (!mtx)
		return ERR_PTR(-ENOMEM);

	spin_lock_init(&mtx->lock);
	mtx->owner_tid = owner_tid;
	mtx->count = owner_tid ? 1 : 0;
	init_waitqueue_head(&mtx->wq);

	return mtx;
}

int triskelion_mutex_release(struct triskelion_mutex *mtx, u32 tid, u32 *prev)
{
	unsigned long flags;
	bool wake = false;

	spin_lock_irqsave(&mtx->lock, flags);

	if (mtx->owner_tid != tid) {
		spin_unlock_irqrestore(&mtx->lock, flags);
		return -EPERM;
	}

	*prev = mtx->count;

	if (--mtx->count == 0) {
		mtx->owner_tid = 0;
		wake = true;
	}

	spin_unlock_irqrestore(&mtx->lock, flags);

	if (wake)
		wake_up_interruptible(&mtx->wq);

	return 0;
}

void triskelion_mutex_destroy(struct triskelion_mutex *mtx)
{
	wake_up_all(&mtx->wq);
	kmem_cache_free(mtx_cache, mtx);
}

/* ── Event ──────────────────────────────────────────────────────────── */

struct triskelion_event *triskelion_event_create(u32 manual_reset, u32 initial)
{
	struct triskelion_event *evt;

	evt = kmem_cache_zalloc(evt_cache, GFP_KERNEL);
	if (!evt)
		return ERR_PTR(-ENOMEM);

	atomic_set(&evt->signaled, initial);
	evt->manual_reset = manual_reset;
	init_waitqueue_head(&evt->wq);

	return evt;
}

int triskelion_event_set(struct triskelion_event *evt, u32 *prev)
{
	*prev = atomic_xchg(&evt->signaled, 1);

	if (evt->manual_reset)
		wake_up_all(&evt->wq);
	else
		wake_up_interruptible(&evt->wq);

	return 0;
}

int triskelion_event_reset(struct triskelion_event *evt, u32 *prev)
{
	*prev = atomic_xchg(&evt->signaled, 0);
	return 0;
}

int triskelion_event_pulse(struct triskelion_event *evt, u32 *prev)
{
	*prev = atomic_xchg(&evt->signaled, 1);

	if (evt->manual_reset)
		wake_up_all(&evt->wq);
	else
		wake_up_interruptible(&evt->wq);

	atomic_set(&evt->signaled, 0);
	return 0;
}

void triskelion_event_destroy(struct triskelion_event *evt)
{
	wake_up_all(&evt->wq);
	kmem_cache_free(evt_cache, evt);
}
