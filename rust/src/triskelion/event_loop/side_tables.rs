// Per-handle side tables.
//
// All state keyed by (pid, handle) that needs to be cleaned up when a handle
// is closed or when a process disconnects. Lives in one struct so the cleanup
// contract is enforced in one file: every map is registered with both purge
// and purge_pid, and an agent adding a new field has to confront both methods.
//
// This is what stops the phantom-limb pattern: a writer that has no consumer
// (or a consumer with no writer) becomes obvious because all per-handle state
// passes through the same gate.

use rustc_hash::{FxHashMap, FxHashSet};
use std::os::unix::io::RawFd;

/// Per-pipe-handle state. Created by handle_create_named_pipe and the pipe
/// path of handle_open_file. Presence in pipe_handles means "this is a pipe."
pub(crate) struct PipeHandle {
    /// Pipe data fd. Closed when the entry is removed (purge or close_handle).
    pub data_fd: RawFd,
}

/// Owns all per-handle side tables on EventLoop. Cleanup goes through purge
/// (single handle) or purge_pid (all handles for a process).
pub(crate) struct HandleSideTables {
    /// Per-pipe-handle state. (pid, handle) → PipeHandle.
    pub pipe_handles: FxHashMap<(u32, u32), PipeHandle>,

    /// Cached I/O completion wait handles. Misnamed in history as
    /// pipe_io_wait_handles — actually used for pipes, devices, AND sockets.
    /// Keyed by (pid, handle), value is a wait event handle in the same process's
    /// handle table. Lazy-allocated by file_io::get_pipe_wait_handle.
    pub io_wait_handles: FxHashMap<(u32, u32), u32>,

    /// (pid, handle) pairs whose fd has been sent to the client via get_handle_fd.
    /// Prevents Wine's add_fd_to_cache from asserting on duplicate fd sends.
    pub fd_sent: FxHashSet<(u32, u32)>,

    /// Completion port bindings: (pid, object_handle) → (port_handle, ckey).
    /// When async I/O completes on the bound object, post to its completion port.
    pub completion_bindings: FxHashMap<(u32, u32), (u32, u64)>,
}

impl HandleSideTables {
    pub fn new() -> Self {
        Self {
            pipe_handles: FxHashMap::default(),
            io_wait_handles: FxHashMap::default(),
            fd_sent: FxHashSet::default(),
            completion_bindings: FxHashMap::default(),
        }
    }

    /// Remove all per-handle side state for a single (pid, handle).
    /// Closes the pipe data fd if present. Called by close_handle.
    /// Returns the io_wait_handle if one was cached, so the caller can
    /// release the corresponding ntsync object.
    pub fn purge(&mut self, pid: u32, handle: u32) -> Option<u32> {
        if let Some(ph) = self.pipe_handles.remove(&(pid, handle)) {
            unsafe { libc::close(ph.data_fd); }
        }
        let io_wh = self.io_wait_handles.remove(&(pid, handle));
        self.fd_sent.remove(&(pid, handle));
        self.completion_bindings.remove(&(pid, handle));
        io_wh
    }

    /// Remove all per-handle side state for an entire pid. Closes all pipe
    /// data fds owned by that pid. Called by disconnect_client.
    pub fn purge_pid(&mut self, pid: u32) {
        let dead_pipe_keys: Vec<(u32, u32)> = self.pipe_handles.keys()
            .filter(|(p, _)| *p == pid)
            .copied()
            .collect();
        for key in dead_pipe_keys {
            if let Some(ph) = self.pipe_handles.remove(&key) {
                unsafe { libc::close(ph.data_fd); }
            }
        }
        self.io_wait_handles.retain(|(p, _), _| *p != pid);
        self.fd_sent.retain(|(p, _)| *p != pid);
        self.completion_bindings.retain(|(p, _), _| *p != pid);
    }
}
