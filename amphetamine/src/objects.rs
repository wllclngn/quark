// LEG 3: Object Management
//
// Handle tables, process/thread state, window objects.
// Read-heavy, write-rare -- the handle table is consulted on every
// CloseHandle, DuplicateHandle, and implicitly by every request that
// takes an obj_handle_t.
//
// Design:
//   - Per-process handle tables (no global lock for same-process lookups)
//   - Handles are 32-bit indices shifted left by 2 (low bits reserved)
//   - Thread state indexed by thread_id_t
//   - Thread queues live in shared memory (managed by ShmManager),
//     not owned by Thread

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use crate::protocol::*;

pub struct HandleEntry {
    pub object_id: object_id_t,
}

pub struct Process {
    pub handles: HandleTable,
    pub threads: Vec<thread_id_t>,
    pub startup_info: Option<Vec<u8>>,  // raw VARARG bytes (startup_info + env)
    pub info_size: u32,                  // size of startup_info portion
    pub machine: u16,                    // architecture (IMAGE_FILE_MACHINE_*)
    pub startup_done: bool,              // set by init_process_done
    pub exit_code: i32,
    pub socket_fd: Option<RawFd>,        // process socket from new_process
    pub peb: u64,                        // from init_process_done
}

impl Process {
    pub fn new(_pid: process_id_t) -> Self {
        Self {
            handles: HandleTable::new(),
            threads: Vec::new(),
            startup_info: None,
            info_size: 0,
            machine: 0x8664, // IMAGE_FILE_MACHINE_AMD64
            startup_done: false,
            exit_code: 0,
            socket_fd: None,
            peb: 0,
        }
    }
}

// Tracks a new_process -> get_new_process_info correlation
pub struct ProcessInfoHandle {
    pub target_pid: process_id_t,
}

// Per-thread state.
// The message queue lives in shared memory (ShmManager), not here.
pub struct Thread {
    pub pid: process_id_t,
}

impl Thread {
    pub fn new(pid: process_id_t) -> Self {
        Self { pid }
    }
}

// Handle table: dense array with free list.
// Handles in Windows are multiples of 4 (low 2 bits reserved).
// We store the raw index and shift on input/output.
pub struct HandleTable {
    entries: Vec<Option<HandleEntry>>,
    free_list: Vec<u32>,
    next_id: u32,
}

impl HandleTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(64),
            free_list: Vec::new(),
            next_id: 1, // handle 0 is invalid
        }
    }

    pub fn allocate(&mut self, object_id: object_id_t) -> obj_handle_t {
        let idx = if let Some(idx) = self.free_list.pop() {
            idx
        } else {
            let idx = self.next_id;
            self.next_id += 1;
            if self.entries.len() <= idx as usize {
                self.entries.resize_with((idx as usize) + 1, || None);
            }
            idx
        };

        self.entries[idx as usize] = Some(HandleEntry { object_id });

        // Windows handles are index * 4 (low 2 bits reserved)
        idx << 2
    }

    pub fn close(&mut self, handle: obj_handle_t) -> Option<HandleEntry> {
        let idx = (handle >> 2) as usize;
        if idx < self.entries.len() {
            let entry = self.entries[idx].take();
            if entry.is_some() {
                self.free_list.push(idx as u32);
            }
            entry
        } else {
            None
        }
    }

    pub fn get(&self, handle: obj_handle_t) -> Option<&HandleEntry> {
        let idx = (handle >> 2) as usize;
        self.entries.get(idx).and_then(|e| e.as_ref())
    }

}

// Global server state
pub struct ServerState {
    pub processes: HashMap<process_id_t, Process>,
    pub threads: HashMap<thread_id_t, Thread>,
    pub process_info_handles: HashMap<u32, ProcessInfoHandle>,
    next_pid: process_id_t,
    next_tid: thread_id_t,
    next_info_handle: u32,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            processes: HashMap::new(),
            threads: HashMap::new(),
            process_info_handles: HashMap::new(),
            next_pid: 1,
            next_tid: 1,
            next_info_handle: 1,
        }
    }

    pub fn alloc_info_handle(&mut self, target_pid: process_id_t) -> u32 {
        let h = self.next_info_handle;
        self.next_info_handle += 1;
        self.process_info_handles.insert(h, ProcessInfoHandle { target_pid });
        h
    }

    pub fn create_process(&mut self) -> process_id_t {
        let pid = self.next_pid;
        self.next_pid += 1;
        self.processes.insert(pid, Process::new(pid));
        pid
    }

    pub fn create_thread(&mut self, pid: process_id_t) -> thread_id_t {
        let tid = self.next_tid;
        self.next_tid += 1;
        self.threads.insert(tid, Thread::new(pid));
        if let Some(process) = self.processes.get_mut(&pid) {
            process.threads.push(tid);
        }
        tid
    }

}
