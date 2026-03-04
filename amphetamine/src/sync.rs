// LEG 2: Sync Arbitration
//
// Implements Windows sync primitives (mutexes, semaphores, events)
// using Linux futexes and atomics. No mutexes in OUR code -- we use
// lock-free state tracking to implement the game's sync objects.
//
// This leg is largely bypassed at runtime by Valve's fsync/esync/ntsync,
// which move sync primitives into shared memory between ntdll and the
// kernel directly. Triskelion handles the fallback path and the
// creation/destruction lifecycle.
//
// Design follows PANDEMONIUM's atomic discipline:
//   Rule 1: No mixed atomic/plain access
//   Rule 3: CAS for state transitions
//   Rule 7: Per-object state (no global lock)

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use crate::protocol::*;
use crate::queue::{futex_wake, futex_wake_i32};

// A sync object tracked by the server.
// Each has an atomic state that clients can futex-wait on.
pub enum SyncObject {
    Semaphore(Semaphore),
    Event(Event),
    Mutex(Mutex),
}

pub struct Semaphore {
    pub handle: obj_handle_t,
    pub count: AtomicI32,
    pub max_count: i32,
}

impl Semaphore {
    pub fn new(handle: obj_handle_t, initial: i32, max: i32) -> Self {
        Self {
            handle,
            count: AtomicI32::new(initial),
            max_count: max,
        }
    }

    // Release: increment count, wake waiters.
    // Returns previous count, or error if would exceed max.
    pub fn release(&self, count: i32) -> Result<i32, ()> {
        loop {
            let current = self.count.load(Ordering::Acquire);
            let new = current + count;
            if new > self.max_count {
                return Err(());
            }
            match self.count.compare_exchange(
                current, new, Ordering::AcqRel, Ordering::Acquire
            ) {
                Ok(prev) => {
                    futex_wake_i32(&self.count, count);
                    return Ok(prev);
                }
                Err(_) => continue, // retry CAS
            }
        }
    }

    // Try-acquire: decrement if count > 0.
    pub fn try_acquire(&self) -> bool {
        loop {
            let current = self.count.load(Ordering::Acquire);
            if current <= 0 {
                return false;
            }
            match self.count.compare_exchange(
                current, current - 1, Ordering::AcqRel, Ordering::Acquire
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }
}

pub struct Event {
    pub handle: obj_handle_t,
    pub signaled: AtomicU32,
    pub manual_reset: bool,
}

impl Event {
    pub fn new(handle: obj_handle_t, manual_reset: bool, initial_state: bool) -> Self {
        Self {
            handle,
            signaled: AtomicU32::new(initial_state as u32),
            manual_reset,
        }
    }

    pub fn set(&self) {
        self.signaled.store(1, Ordering::Release);
        futex_wake(&self.signaled, i32::MAX);
    }

    pub fn reset(&self) {
        self.signaled.store(0, Ordering::Release);
    }

    pub fn pulse(&self) {
        // Set then immediately reset -- wake waiting threads but don't
        // leave the event signaled.
        self.signaled.store(1, Ordering::Release);
        futex_wake(&self.signaled, i32::MAX);
        self.signaled.store(0, Ordering::Release);
    }

    // Try-acquire for auto-reset events: atomically check and clear.
    pub fn try_acquire(&self) -> bool {
        if self.manual_reset {
            self.signaled.load(Ordering::Acquire) != 0
        } else {
            // Auto-reset: CAS 1 -> 0
            self.signaled.compare_exchange(
                1, 0, Ordering::AcqRel, Ordering::Acquire
            ).is_ok()
        }
    }
}

pub struct Mutex {
    pub handle: obj_handle_t,
    // Owner thread ID, or 0 if unowned
    pub owner: AtomicU32,
    // Recursion count (Windows mutexes are recursive)
    pub count: AtomicU32,
}

impl Mutex {
    pub fn new(handle: obj_handle_t, owned_by: Option<thread_id_t>) -> Self {
        Self {
            handle,
            owner: AtomicU32::new(owned_by.unwrap_or(0)),
            count: AtomicU32::new(if owned_by.is_some() { 1 } else { 0 }),
        }
    }

    pub fn try_acquire(&self, tid: thread_id_t) -> bool {
        let current_owner = self.owner.load(Ordering::Acquire);

        if current_owner == 0 {
            // Unowned -- try to take it
            match self.owner.compare_exchange(
                0, tid, Ordering::AcqRel, Ordering::Acquire
            ) {
                Ok(_) => {
                    self.count.store(1, Ordering::Release);
                    true
                }
                Err(_) => false,
            }
        } else if current_owner == tid {
            // Recursive acquisition
            self.count.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn release(&self, tid: thread_id_t) -> Result<u32, ()> {
        let current_owner = self.owner.load(Ordering::Acquire);
        if current_owner != tid {
            return Err(()); // not the owner
        }

        let prev = self.count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            // Last release -- clear ownership
            self.owner.store(0, Ordering::Release);
            futex_wake(&self.owner, 1);
        }
        Ok(prev - 1)
    }
}
