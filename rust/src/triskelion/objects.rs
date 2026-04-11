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
use rustc_hash::FxHashMap;
use std::os::unix::io::RawFd;
use crate::protocol::*;

// Wine FD_TYPE_* constants (from include/wine/server_protocol.h)
pub const FD_TYPE_FILE: u32 = 1;
pub const FD_TYPE_DIR: u32 = 2;
pub const FD_TYPE_SOCKET: u32 = 3;
pub const FD_TYPE_PIPE: u32 = 5;
pub const FD_TYPE_CHAR: u32 = 7;
pub const FD_TYPE_DEVICE: u32 = 8;

pub struct HandleEntry {
    pub object_id: object_id_t,
    pub fd: Option<RawFd>,    // Unix fd (for get_handle_fd)
    pub obj_type: u32,        // FD_TYPE_*
    pub access: u32,          // Access mask
    pub options: u32,         // Create options
}

impl HandleEntry {
    pub fn new(object_id: object_id_t) -> Self {
        Self { object_id, fd: None, obj_type: 0, access: 0, options: 0 }
    }

    pub fn with_fd(object_id: object_id_t, fd: RawFd, obj_type: u32, access: u32, options: u32) -> Self {
        Self { object_id, fd: Some(fd), obj_type, access, options }
    }
}

// Per-mapping metadata (memfd or dup'd file fd, size, flags)
pub struct MappingInfo {
    pub fd: RawFd,
    pub size: u64,
    pub flags: u32,    // SEC_* flags
    pub pe_image_info: Option<Vec<u8>>,  // Serialized pe_image_info for SEC_IMAGE
    pub nt_name: Option<Vec<u8>>,        // UTF-16LE encoded NT path (for find_builtin_dll)
    pub shared_fd: Option<RawFd>,        // backing fd for shared writable PE sections
}

impl Drop for MappingInfo {
    fn drop(&mut self) {
        if self.fd >= 0 { unsafe { libc::close(self.fd); } }
        if let Some(sfd) = self.shared_fd {
            if sfd >= 0 { unsafe { libc::close(sfd); } }
        }
    }
}

// Named object entry (e.g. USD section)
pub struct NamedObjectEntry {
    pub object_id: u64,
    pub fd: RawFd,
}

pub struct Process {
    pub handles: HandleTable,
    pub threads: Vec<thread_id_t>,
    pub startup_info: Option<Vec<u8>>,  // startup_info struct + variable strings (info_size bytes)
    pub startup_env: Option<Vec<u8>>,   // environment block (after startup_info, no VARARG padding)
    pub info_size: u32,                  // size of startup_info portion
    pub machine: u16,                    // architecture (IMAGE_FILE_MACHINE_*)
    pub startup_done: bool,              // set by init_process_done
    pub claimed: bool,                   // true after init_first_thread claims this process
    pub exit_code: i32,
    pub socket_fd: Option<RawFd>,        // process socket from new_process
    pub peb: u64,                        // from init_process_done
    pub exe_image_info: Option<Vec<u8>>, // pe_image_info of main executable
    pub idle_signaled: bool,             // true after first blocking Select (process is "idle")
    pub parent_pid: u32,                 // 0 for the initial wine process
    pub session_id: u32,                 // Windows session ID (1 = interactive, 0 = services)
}

impl Process {
    pub fn new(_pid: process_id_t) -> Self {
        Self {
            handles: HandleTable::new(),
            threads: Vec::new(),
            startup_info: None,
            startup_env: None,
            info_size: 0,
            machine: 0x8664, // IMAGE_FILE_MACHINE_AMD64
            startup_done: false,
            claimed: false,
            exit_code: 259, // STILL_ACTIVE (0x103) — must match stock until process exits
            socket_fd: None,
            peb: 0,
            exe_image_info: None,
            idle_signaled: false,
            parent_pid: 0,
            session_id: 1, // Default: interactive session
        }
    }
}

// Tracks a new_process -> get_new_process_info correlation
pub struct ProcessInfoHandle {
    pub target_pid: process_id_t,
    pub parent_pid: process_id_t,
    /// Dup'd ntsync event fd — survives close_handle on the parent's copy.
    /// Signaled by init_process_done when the child completes init.
    pub ntsync_obj_fd: i32,
}

pub struct Job {
    pub processes: Vec<u32>,
    pub num_processes: u32,
    pub total_processes: u32,
    pub limit_flags: u32,
    pub completion_port_handle: Option<obj_handle_t>,
    pub completion_key: u64,
}

// Per-thread state.
// The message queue lives in shared memory (ShmManager), not here.
pub struct Thread {
    pub pid: process_id_t,
    pub suspend_count: i32,
}

impl Thread {
    pub fn new(pid: process_id_t) -> Self {
        Self { pid, suspend_count: 0 }
    }
}

// Generational handle table backed by HeapSlab.
//
// Handles in Windows are multiples of 4 (low 2 bits reserved).
// Index = handle >> 2. HeapSlab provides O(1) alloc/free with LIFO
// free list and generation counters for ABA prevention.

pub struct HandleTable {
    slab: crate::slab::HeapSlab<HandleEntry>,
    fd_refcounts: FxHashMap<RawFd, u32>,
}

impl HandleTable {
    pub fn new() -> Self {
        let mut slab = crate::slab::HeapSlab::with_capacity(64);
        // Skip index 0 so handle values start at 0x4 (index 1 << 2).
        // Handle 0 is invalid in Wine's protocol.
        slab.skip_index_zero();
        Self { slab, fd_refcounts: FxHashMap::default() }
    }

    pub fn allocate(&mut self, object_id: object_id_t) -> obj_handle_t {
        // Bump-only: Wine caches handle→fd mappings. Reusing a closed slot
        // would cause stale cache entries to map to the wrong object.
        let (idx, _gen) = self.slab.insert_bump(HandleEntry::new(object_id));
        idx << 2
    }

    pub fn allocate_full(&mut self, entry: HandleEntry) -> obj_handle_t {
        if let Some(fd) = entry.fd {
            *self.fd_refcounts.entry(fd).or_insert(0) += 1;
        }
        let (idx, _gen) = self.slab.insert_bump(entry);
        idx << 2
    }

    pub fn close(&mut self, handle: obj_handle_t) -> Option<HandleEntry> {
        let idx = handle >> 2;
        let entry = self.slab.remove_unchecked(idx)?;
        if let Some(fd) = entry.fd {
            let close_fd = match self.fd_refcounts.get_mut(&fd) {
                Some(count) => {
                    *count -= 1;
                    if *count == 0 { self.fd_refcounts.remove(&fd); true } else { false }
                }
                None => {
                    // Not tracked — entry was created through a path that
                    // bypassed allocate_full. Scan remaining entries before
                    // closing to avoid invalidating shared mappings.
                    !self.slab.iter().any(|e| e.fd == Some(fd))
                }
            };
            if close_fd {
                unsafe { libc::close(fd); }
            }
        }
        Some(entry)
    }

    pub fn get(&self, handle: obj_handle_t) -> Option<&HandleEntry> {
        self.slab.get_unchecked(handle >> 2)
    }

    pub fn slot_count(&self) -> usize {
        self.slab.capacity()
    }

    pub fn get_mut(&mut self, handle: obj_handle_t) -> Option<&mut HandleEntry> {
        self.slab.get_mut_unchecked(handle >> 2)
    }

    pub fn insert_at(&mut self, handle: obj_handle_t, entry: HandleEntry) {
        let idx = handle >> 2;
        // Decrement refcount for old fd if overwriting an occupied slot
        if let Some(old) = self.slab.get_unchecked(idx) {
            if let Some(fd) = old.fd {
                match self.fd_refcounts.get_mut(&fd) {
                    Some(count) => {
                        *count -= 1;
                        if *count == 0 { self.fd_refcounts.remove(&fd); unsafe { libc::close(fd); } }
                    }
                    None => {
                        if !self.slab.iter().any(|e| e.fd == Some(fd)) {
                            unsafe { libc::close(fd); }
                        }
                    }
                }
            }
        }
        // Track new entry's fd
        if let Some(fd) = entry.fd {
            *self.fd_refcounts.entry(fd).or_insert(0) += 1;
        }
        self.slab.insert_at(idx, entry);
    }
}

// Global server state
pub struct ServerState {
    pub processes: FxHashMap<process_id_t, Process>,
    pub threads: FxHashMap<thread_id_t, Thread>,
    pub process_info_handles: FxHashMap<u32, ProcessInfoHandle>,
    // Named objects (e.g. USD section mapping)
    pub named_objects: HashMap<String, NamedObjectEntry>,
    // Per-mapping metadata keyed by object_id
    pub mappings: FxHashMap<u64, MappingInfo>,
    // Image view base addresses: (pid, mapping_object_id) → base_addr
    pub image_views: FxHashMap<(u32, u64), u64>,
    // Processes created by new_process but not yet claimed by init_first_thread.
    // Children connect via master socket (WINESERVERSOCKET=0 bug) so we match them FIFO.
    pub unclaimed_pids: std::collections::VecDeque<process_id_t>,
    next_ptid: u32,  // shared PID/TID counter (Windows shares the ID space)
    next_object_id: u64,
    // Global atom table: atom_id → (name_utf16le, ref_count)
    pub atoms: FxHashMap<u32, (Vec<u8>, i32)>,
    // Reverse lookup: name_lowercase → atom_id
    pub atom_names: FxHashMap<Vec<u16>, u32>,
    pub next_atom: u32,
}

impl ServerState {
    pub fn new() -> Self {
        Self {
            processes: FxHashMap::default(),
            threads: FxHashMap::default(),
            process_info_handles: FxHashMap::default(),
            named_objects: HashMap::new(),
            mappings: FxHashMap::default(),
            image_views: FxHashMap::default(),
            unclaimed_pids: std::collections::VecDeque::new(),
            next_ptid: 0,
            next_object_id: 0x1000,
            atoms: FxHashMap::default(),
            atom_names: FxHashMap::default(),
            next_atom: 0xC000, // Wine global atoms start at 0xC000
        }
    }

    pub fn alloc_object_id(&mut self) -> u64 {
        let id = self.next_object_id;
        self.next_object_id += 1;
        id
    }

    /// Allocate a process/thread ID in Wine's ptid format: (index + 8) * 4.
    /// This ensures `(id >> 2) - 1` (used by Wine's tid_alert and handle caches) doesn't underflow.
    fn alloc_ptid(index: &mut u32) -> u32 {
        const PTID_OFFSET: u32 = 8;
        let i = *index;
        *index += 1;
        (i + PTID_OFFSET) * 4
    }

    pub fn create_process(&mut self) -> process_id_t {
        let pid = Self::alloc_ptid(&mut self.next_ptid);
        self.processes.insert(pid, Process::new(pid));
        pid
    }

    pub fn create_thread(&mut self, pid: process_id_t) -> thread_id_t {
        let tid = Self::alloc_ptid(&mut self.next_ptid);
        self.threads.insert(tid, Thread::new(pid));
        if let Some(process) = self.processes.get_mut(&pid) {
            process.threads.push(tid);
        }
        tid
    }

}
