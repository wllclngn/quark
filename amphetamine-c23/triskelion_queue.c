// SPDX-License-Identifier: GPL-2.0
/*
 * triskelion kernel module — message queues.
 *
 * Per-thread ring buffers for PostMessage/GetMessage. Uses the kernel's
 * hlist hash table for O(1) lookup by TID with proper collision chaining.
 * 256-slot ring per thread, spinlock-protected, with wait queues for
 * blocking GetMessage.
 */

#include <linux/slab.h>
#include <linux/hashtable.h>

#include "triskelion_internal.h"

void triskelion_queues_init(struct triskelion_queue_table *qt)
{
	hash_init(qt->queues);
	spin_lock_init(&qt->lock);
}

void triskelion_queues_destroy(struct triskelion_queue_table *qt)
{
	struct triskelion_msg_queue *q;
	struct hlist_node *tmp;
	int bkt;

	hash_for_each_safe(qt->queues, bkt, tmp, q, node) {
		wake_up_all(&q->wq);
		hash_del(&q->node);
		kfree(q);
	}
}

struct triskelion_msg_queue *triskelion_queue_get_or_create(
	struct triskelion_queue_table *qt, u32 tid)
{
	struct triskelion_msg_queue *q;
	unsigned long flags;

	/* Fast path: find existing queue for this TID */
	rcu_read_lock();
	hash_for_each_possible_rcu(qt->queues, q, node, tid) {
		if (q->tid == tid) {
			rcu_read_unlock();
			return q;
		}
	}
	rcu_read_unlock();

	/* Slow path: allocate new queue */
	q = kzalloc(sizeof(*q), GFP_KERNEL);
	if (!q)
		return ERR_PTR(-ENOMEM);

	q->tid = tid;
	spin_lock_init(&q->lock);
	init_waitqueue_head(&q->wq);

	spin_lock_irqsave(&qt->lock, flags);

	/* Double-check under lock — another thread may have created it */
	{
		struct triskelion_msg_queue *existing;

		hash_for_each_possible(qt->queues, existing, node, tid) {
			if (existing->tid == tid) {
				spin_unlock_irqrestore(&qt->lock, flags);
				kfree(q);
				return existing;
			}
		}
	}

	hash_add_rcu(qt->queues, &q->node, tid);
	spin_unlock_irqrestore(&qt->lock, flags);

	return q;
}

int triskelion_queue_post(struct triskelion_msg_queue *q,
			  const struct triskelion_msg *msg)
{
	u32 next;
	unsigned long flags;

	spin_lock_irqsave(&q->lock, flags);

	next = (q->write_pos + 1) & (TRISKELION_QUEUE_SIZE - 1);
	if (next == q->read_pos) {
		spin_unlock_irqrestore(&q->lock, flags);
		return -ENOSPC;  /* queue full */
	}

	q->ring[q->write_pos] = *msg;
	WRITE_ONCE(q->write_pos, next);

	spin_unlock_irqrestore(&q->lock, flags);

	wake_up_interruptible(&q->wq);
	return 0;
}

int triskelion_queue_get(struct triskelion_msg_queue *q,
			 struct triskelion_msg *msg)
{
	unsigned long flags;

	spin_lock_irqsave(&q->lock, flags);

	if (q->read_pos == q->write_pos) {
		spin_unlock_irqrestore(&q->lock, flags);
		return -EAGAIN;  /* empty */
	}

	*msg = q->ring[q->read_pos];
	WRITE_ONCE(q->read_pos,
		   (q->read_pos + 1) & (TRISKELION_QUEUE_SIZE - 1));

	spin_unlock_irqrestore(&q->lock, flags);

	return 0;
}
