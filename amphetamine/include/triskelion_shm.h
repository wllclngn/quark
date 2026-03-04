// triskelion_shm.h -- Shared memory layout for Wine client bypass
//
// This header defines the exact binary layout of the shared memory
// region created by triskelion. Wine's win32u/ntdll maps this file
// and reads/writes message queues directly, bypassing socket RPC
// for hot-path GetMessage/PeekMessage/PostMessage.
//
// Layout must match triskelion's queue.rs and shm.rs exactly.
// All offsets and sizes are verified by static assertions below.
//
// Usage (Wine client side):
//   1. Check WINE_TRISKELION env var (cached, like do_fsync())
//   2. shm_open("/triskelion-<prefix-hash>", O_RDWR)
//   3. mmap the whole file (MAP_SHARED)
//   4. Read ShmHeader at offset 0 for layout params
//   5. Thread's queue is at: base + 64 + slot_index * 24896
//
// SPSC contract:
//   - Posted ring: PostMessage is producer, GetMessage is consumer
//   - Sent ring: SendMessage is producer, GetMessage is consumer
//   - Only one writer and one reader per ring at a time
//   - Atomics use acquire/release ordering (see queue.rs comments)

#ifndef TRISKELION_SHM_H
#define TRISKELION_SHM_H

#include <stdint.h>
#include <stdatomic.h>

#define TRISKELION_SHM_MAGIC   0x54524953  /* "TRIS" */
#define TRISKELION_SHM_VERSION 1
#define TRISKELION_MAX_THREADS 256
#define TRISKELION_RING_CAPACITY 256

// Shared memory header (offset 0, 64 bytes)
struct triskelion_shm_header {
    uint32_t magic;
    uint32_t version;
    uint32_t max_threads;
    uint32_t queue_size;
    _Atomic uint32_t next_slot;
    uint8_t _reserved[44];
} __attribute__((aligned(64)));

_Static_assert(sizeof(struct triskelion_shm_header) == 64, "header size");

// A queued Windows message (48 bytes)
struct triskelion_message {
    uint32_t win;
    uint32_t msg;
    uint64_t wparam;
    uint64_t lparam;
    int32_t  msg_type;
    int32_t  x;
    int32_t  y;
    uint32_t time;
    uint32_t sender_tid;
    uint32_t _pad;
};

_Static_assert(sizeof(struct triskelion_message) == 48, "message size");

// Cache-line aligned atomic u64 (64 bytes)
struct triskelion_cacheline_u64 {
    _Atomic uint64_t val;
    uint8_t _pad[56];
} __attribute__((aligned(64)));

_Static_assert(sizeof(struct triskelion_cacheline_u64) == 64, "cacheline size");

// SPSC message ring buffer (12416 bytes)
// write_pos and read_pos are on separate cache lines.
struct triskelion_ring {
    struct triskelion_cacheline_u64 write_pos;
    struct triskelion_cacheline_u64 read_pos;
    struct triskelion_message buf[TRISKELION_RING_CAPACITY];
};

_Static_assert(sizeof(struct triskelion_ring) == 128 + 48 * TRISKELION_RING_CAPACITY,
               "ring size");

// Per-thread queue (24896 bytes)
// Contains two rings (posted + sent) and atomic status bits.
struct triskelion_queue {
    struct triskelion_ring posted;
    struct triskelion_ring sent;
    _Atomic uint32_t wake_bits;
    _Atomic uint32_t changed_bits;
    _Atomic uint32_t wake_mask;
    _Atomic uint32_t changed_mask;
    uint32_t thread_id;
    uint8_t _reserved[44];
};

_Static_assert(sizeof(struct triskelion_queue) == 24896, "queue size");

#define TRISKELION_HEADER_SIZE 64
#define TRISKELION_QUEUE_SIZE  24896

// Queue wake bits (match Wine's QS_* constants)
#define TRISKELION_QS_KEY            0x0001
#define TRISKELION_QS_MOUSEMOVE      0x0002
#define TRISKELION_QS_MOUSEBUTTON    0x0004
#define TRISKELION_QS_POSTMESSAGE    0x0008
#define TRISKELION_QS_TIMER          0x0010
#define TRISKELION_QS_PAINT          0x0020
#define TRISKELION_QS_SENDMESSAGE    0x0040

// Inline helpers for Wine client-side bypass

static inline struct triskelion_queue *
triskelion_get_queue(void *shm_base, uint32_t slot)
{
    return (struct triskelion_queue *)
        ((char *)shm_base + TRISKELION_HEADER_SIZE + slot * TRISKELION_QUEUE_SIZE);
}

// Read a message from the posted ring (consumer side).
// Returns 1 if a message was read, 0 if ring is empty.
static inline int
triskelion_ring_pop(struct triskelion_ring *ring, struct triskelion_message *out)
{
    uint64_t rp = atomic_load_explicit(&ring->read_pos.val, memory_order_relaxed);
    uint64_t wp = atomic_load_explicit(&ring->write_pos.val, memory_order_acquire);

    if (rp == wp)
        return 0;

    *out = ring->buf[rp & (TRISKELION_RING_CAPACITY - 1)];
    atomic_store_explicit(&ring->read_pos.val, rp + 1, memory_order_release);
    return 1;
}

// Write a message into a ring (producer side).
// Returns 1 if written, 0 if ring is full.
static inline int
triskelion_ring_push(struct triskelion_ring *ring, const struct triskelion_message *msg)
{
    uint64_t wp = atomic_load_explicit(&ring->write_pos.val, memory_order_relaxed);
    uint64_t rp = atomic_load_explicit(&ring->read_pos.val, memory_order_acquire);

    if ((wp - rp) >= TRISKELION_RING_CAPACITY)
        return 0;

    ring->buf[wp & (TRISKELION_RING_CAPACITY - 1)] = *msg;
    atomic_store_explicit(&ring->write_pos.val, wp + 1, memory_order_release);
    return 1;
}

#endif /* TRISKELION_SHM_H */
