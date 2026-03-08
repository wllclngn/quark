// SPDX-License-Identifier: GPL-2.0
/*
 * triskelion kernel module — handle table.
 *
 * Dense array + free list. Handles are 1-indexed (0 = invalid).
 * Same pattern as the Rust triskelion, but in kernel memory.
 */

#include <linux/slab.h>

#include "triskelion_internal.h"

int triskelion_handles_init(struct triskelion_handle_table *ht, u32 capacity)
{
	u32 i;

	ht->entries = kcalloc(capacity, sizeof(*ht->entries), GFP_KERNEL);
	if (!ht->entries)
		return -ENOMEM;

	ht->free_list = kmalloc_array(capacity, sizeof(*ht->free_list),
				      GFP_KERNEL);
	if (!ht->free_list) {
		kfree(ht->entries);
		return -ENOMEM;
	}

	/* Build free list: [capacity-1, capacity-2, ..., 1, 0] */
	for (i = 0; i < capacity; i++)
		ht->free_list[i] = capacity - 1 - i;

	ht->capacity = capacity;
	ht->free_head = capacity;
	ht->count = 0;
	spin_lock_init(&ht->lock);

	return 0;
}

void triskelion_handles_destroy(struct triskelion_handle_table *ht)
{
	u32 i;

	if (!ht->entries)
		return;

	/* Free all live objects */
	for (i = 0; i < ht->capacity; i++) {
		struct triskelion_object *obj = &ht->entries[i];

		if (!obj->data)
			continue;

		switch (obj->type) {
		case TRISKELION_OBJ_SEMAPHORE:
			triskelion_sem_destroy(obj->data);
			break;
		case TRISKELION_OBJ_MUTEX:
			triskelion_mutex_destroy(obj->data);
			break;
		case TRISKELION_OBJ_EVENT:
			triskelion_event_destroy(obj->data);
			break;
		default:
			kfree(obj->data);
			break;
		}
	}

	kfree(ht->entries);
	kfree(ht->free_list);
	ht->entries = NULL;
	ht->free_list = NULL;
}

triskelion_handle_t triskelion_handle_alloc(struct triskelion_handle_table *ht,
					    enum triskelion_obj_type type,
					    void *data)
{
	triskelion_handle_t handle;
	u32 idx;
	unsigned long flags;

	spin_lock_irqsave(&ht->lock, flags);

	if (ht->free_head == 0) {
		spin_unlock_irqrestore(&ht->lock, flags);
		return TRISKELION_INVALID_HANDLE;
	}

	idx = ht->free_list[--ht->free_head];
	ht->entries[idx].type = type;
	ht->entries[idx].data = data;
	refcount_set(&ht->entries[idx].refcnt, 1);
	ht->count++;

	spin_unlock_irqrestore(&ht->lock, flags);

	handle = idx + 1;  /* 1-indexed */
	return handle;
}

struct triskelion_object *triskelion_handle_get(struct triskelion_handle_table *ht,
					       triskelion_handle_t handle)
{
	u32 idx;

	if (handle == TRISKELION_INVALID_HANDLE || handle > ht->capacity)
		return NULL;

	idx = handle - 1;
	if (!ht->entries[idx].data)
		return NULL;

	return &ht->entries[idx];
}

int triskelion_handle_close(struct triskelion_handle_table *ht,
			    triskelion_handle_t handle)
{
	struct triskelion_object *obj;
	u32 idx;
	unsigned long flags;

	if (handle == TRISKELION_INVALID_HANDLE || handle > ht->capacity)
		return -EINVAL;

	idx = handle - 1;

	spin_lock_irqsave(&ht->lock, flags);

	obj = &ht->entries[idx];
	if (!obj->data) {
		spin_unlock_irqrestore(&ht->lock, flags);
		return -ENOENT;
	}

	/* Free the payload */
	switch (obj->type) {
	case TRISKELION_OBJ_SEMAPHORE:
		triskelion_sem_destroy(obj->data);
		break;
	case TRISKELION_OBJ_MUTEX:
		triskelion_mutex_destroy(obj->data);
		break;
	case TRISKELION_OBJ_EVENT:
		triskelion_event_destroy(obj->data);
		break;
	default:
		kfree(obj->data);
		break;
	}

	obj->data = NULL;
	obj->type = 0;

	/* Return to free list */
	ht->free_list[ht->free_head++] = idx;
	ht->count--;

	spin_unlock_irqrestore(&ht->lock, flags);

	return 0;
}
