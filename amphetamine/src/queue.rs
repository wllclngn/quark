// LEG 1: Message Queue
//
// Per-thread message queues in shared memory. The hottest path in the
// server -- every GetMessage/PeekMessage/PostMessage/SendMessage hits this.
//
// Design:
//   - Each Wine thread owns a ThreadQueue in memfd-backed shared memory
//   - Both triskelion (Rust) and Wine clients (C) access the same bytes
//   - PostMessage: sender writes directly into receiver's ring
//   - GetMessage: receiver reads from own ring (no server round-trip
//     for same-process messages)
//   - SendMessage: write to receiver's ring + block for reply
//   - Cross-process messages still go through the server socket
//
// Layout contract:
//   - Every struct is #[repr(C)] with compile-time size assertions
//   - Changes here require matching changes in triskelion_shm.h
//   - CacheLineU64 separates producer/consumer atomics (no false sharing)
//   - Ring buffer is inline (no heap allocation, no pointers)
//
// The ring buffer uses the OUROBOROS/uEmacs pattern:
//   - Power-of-2 capacity, bitwise masking
//   - Monotonic positions (no ABA)
//   - Cache-line separated head/tail atomics
//   - Acquire/Release ordering (minimal correct for SPSC)

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use crate::protocol::*;

// Wake threads blocked in futex_wait on a 32-bit atomic word.
// Used after state changes to wake sleeping readers/waiters.
pub(crate) fn futex_wake(word: &AtomicU32, count: i32) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const AtomicU32 as *const u32,
            libc::FUTEX_WAKE,
            count,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
}

// AtomicI32 variant (for Semaphore::count).
pub(crate) fn futex_wake_i32(word: &std::sync::atomic::AtomicI32, count: i32) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const std::sync::atomic::AtomicI32 as *const u32,
            libc::FUTEX_WAKE,
            count,
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
}

// A queued Windows message
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct QueuedMessage {
    pub win: user_handle_t,
    pub msg: u32,
    pub wparam: lparam_t,
    pub lparam: lparam_t,
    pub msg_type: i32,
    pub x: i32,
    pub y: i32,
    pub time: u32,
    pub sender_tid: thread_id_t,
    pub _pad: u32,
}

const _: () = assert!(std::mem::size_of::<QueuedMessage>() == 48);

pub const RING_CAPACITY: usize = 256;
const RING_MASK: usize = RING_CAPACITY - 1;
const _: () = assert!(RING_CAPACITY.is_power_of_two());

// Cache-line aligned u64 atomic. Prevents false sharing between
// producer (write_pos) and consumer (read_pos) on separate cores.
#[repr(C, align(64))]
pub struct CacheLineU64 {
    pub val: AtomicU64,
    _pad: [u8; 56],
}

const _: () = assert!(std::mem::size_of::<CacheLineU64>() == 64);

// SPSC ring buffer for messages.
// Flat layout -- the buffer is inline, not behind a pointer.
// Lives in shared memory alongside the rest of ThreadQueue.
#[repr(C)]
pub struct MessageRing {
    // Producer writes write_pos, consumer reads it
    write_pos: CacheLineU64,
    // Consumer writes read_pos, producer reads it
    read_pos: CacheLineU64,
    // Inline message buffer. UnsafeCell is #[repr(transparent)] so the
    // layout matches a plain [QueuedMessage; 256] for C interop.
    buf: UnsafeCell<[QueuedMessage; RING_CAPACITY]>,
}

const _: () = assert!(std::mem::size_of::<MessageRing>() == 128 + 48 * RING_CAPACITY);

unsafe impl Send for MessageRing {}
unsafe impl Sync for MessageRing {}

impl MessageRing {
    pub fn push(&self, msg: QueuedMessage) -> bool {
        let wp = self.write_pos.val.load(Ordering::Relaxed);
        let rp = self.read_pos.val.load(Ordering::Acquire);

        if (wp - rp) as usize >= RING_CAPACITY {
            return false; // full
        }

        let idx = (wp as usize) & RING_MASK;
        unsafe {
            let buf = &mut *self.buf.get();
            std::ptr::write(&mut buf[idx] as *mut QueuedMessage, msg);
        }

        self.write_pos.val.store(wp + 1, Ordering::Release);
        true
    }

    pub fn pop(&self) -> Option<QueuedMessage> {
        let rp = self.read_pos.val.load(Ordering::Relaxed);
        let wp = self.write_pos.val.load(Ordering::Acquire);

        if rp == wp {
            return None; // empty
        }

        let idx = (rp as usize) & RING_MASK;
        let msg = unsafe {
            let buf = &*self.buf.get();
            std::ptr::read(&buf[idx])
        };

        self.read_pos.val.store(rp + 1, Ordering::Release);
        Some(msg)
    }

    pub fn is_empty(&self) -> bool {
        let rp = self.read_pos.val.load(Ordering::Relaxed);
        let wp = self.write_pos.val.load(Ordering::Acquire);
        rp == wp
    }
}

// Per-thread message queue state.
//
// Lives in memfd-backed shared memory -- both triskelion (Rust) and
// Wine clients (C) read/write this struct directly.
//
// Layout must match triskelion_shm.h exactly.
// Alignment is 64 bytes (inherited from CacheLineU64 in MessageRing).
#[repr(C)]
pub struct ThreadQueue {
    // Posted messages (SPSC ring)
    pub posted: MessageRing,

    // Sent messages waiting for reply (separate ring)
    pub sent: MessageRing,

    // Wake/changed bits (atomic -- set by poster, read by receiver)
    pub wake_bits: AtomicU32,
    pub changed_bits: AtomicU32,

    // Queue mask set by SetQueueMask
    pub wake_mask: AtomicU32,
    pub changed_mask: AtomicU32,

    // Owning thread
    pub thread_id: thread_id_t,

    // Pad to 64-byte boundary for clean alignment in slot arrays
    _reserved: [u8; 44],
}

// 2 * 12416 (rings) + 5 * 4 (u32 fields) + 44 (reserved) = 24896
const _: () = assert!(std::mem::size_of::<ThreadQueue>() == 24896);
// Alignment is 64 from CacheLineU64
const _: () = assert!(std::mem::align_of::<ThreadQueue>() == 64);

unsafe impl Send for ThreadQueue {}
unsafe impl Sync for ThreadQueue {}

pub const THREAD_QUEUE_SIZE: usize = std::mem::size_of::<ThreadQueue>();

impl ThreadQueue {
    // Initialize a ThreadQueue in-place in shared memory.
    // Safety: ptr must point to at least THREAD_QUEUE_SIZE bytes of
    // writable memory.
    pub unsafe fn init_at(ptr: *mut Self, thread_id: thread_id_t) {
        unsafe {
            std::ptr::write_bytes(ptr, 0, 1);
            (*ptr).thread_id = thread_id;
        }
    }

    // Post a message into this thread's queue.
    pub fn post(&self, msg: QueuedMessage) -> bool {
        let ok = self.posted.push(msg);
        if ok {
            self.wake_bits.fetch_or(QS_POSTMESSAGE, Ordering::Release);
            self.changed_bits.fetch_or(QS_POSTMESSAGE, Ordering::Release);
            futex_wake(&self.wake_bits, 1);
        }
        ok
    }

    // Send a message (cross-thread SendMessage).
    pub fn send(&self, msg: QueuedMessage) -> bool {
        let ok = self.sent.push(msg);
        if ok {
            self.wake_bits.fetch_or(QS_SENDMESSAGE, Ordering::Release);
            self.changed_bits.fetch_or(QS_SENDMESSAGE, Ordering::Release);
            futex_wake(&self.wake_bits, 1);
        }
        ok
    }

    // Get the next message (sent messages have priority per Win32 semantics).
    pub fn get(&self) -> Option<QueuedMessage> {
        if let Some(msg) = self.sent.pop() {
            if self.sent.is_empty() {
                self.wake_bits.fetch_and(!QS_SENDMESSAGE, Ordering::Release);
            }
            return Some(msg);
        }

        if let Some(msg) = self.posted.pop() {
            if self.posted.is_empty() {
                self.wake_bits.fetch_and(!QS_POSTMESSAGE, Ordering::Release);
            }
            return Some(msg);
        }

        None
    }

    pub fn has_messages(&self, mask: u32) -> bool {
        self.wake_bits.load(Ordering::Acquire) & mask != 0
    }

    pub fn get_status(&self, clear_bits: u32) -> (u32, u32) {
        let wake = self.wake_bits.load(Ordering::Acquire);
        let changed = self.changed_bits.fetch_and(!clear_bits, Ordering::AcqRel);
        (wake, changed)
    }
}
