// Client IPC -- Unix domain socket listener and per-client connection state
//
// Wine's server protocol uses THREE separate channels per thread:
//   1. msg_fd (Unix socket) -- the connection from accept(). Used for
//      passing file descriptors via SCM_RIGHTS in both directions.
//   2. request_fd (pipe) -- server creates on accept, sends write-end to
//      client. Client writes protocol requests here. Server reads.
//   3. reply_fd (pipe) -- client creates, sends write-end to server via
//      SCM_RIGHTS on msg_fd. Server writes replies here. Client reads.
//
// On accept, the server MUST immediately send:
//   - The write-end of request_pipe via SCM_RIGHTS
//   - The client's expected protocol version as the data payload
// Wine blocks in wine_server_receive_fd() waiting for this.

use std::collections::VecDeque;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::Path;

/// Compile-time protocol version. Re-exported from the build.rs-generated
/// protocol module, which reads SERVER_PROTOCOL_VERSION from the Wine source
/// at build time. Always matches the wineserver protocol version this build
/// was compiled against.
pub use crate::protocol::COMPILED_PROTOCOL_VERSION;

/// Runtime protocol version — set by detect_and_remap() at startup.
/// Defaults to COMPILED_PROTOCOL_VERSION; updated before accepting connections.
static RUNTIME_PROTOCOL_VERSION: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(COMPILED_PROTOCOL_VERSION);

pub fn set_runtime_protocol_version(ver: u32) {
    RUNTIME_PROTOCOL_VERSION.store(ver, std::sync::atomic::Ordering::Relaxed);
}

pub fn runtime_protocol_version() -> u32 {
    RUNTIME_PROTOCOL_VERSION.load(std::sync::atomic::Ordering::Relaxed)
}

pub struct Listener {
    inner: UnixListener,
}

impl Listener {
    pub fn fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }

}

/// Set up a client connection on a pre-connected socket_fd (from NewProcess socketpair).
/// Same handshake as accept(): create request pipe, send write end to client.
pub fn setup_client_on_socket(socket_fd: RawFd) -> Option<(Client, RawFd)> {
    // Keep socket BLOCKING for the initial handshake.
    // The child's Wine loader does a blocking recvmsg to receive the request pipe.
    // Setting O_NONBLOCK here causes EAGAIN and the child dies silently.
    // We set non-blocking AFTER sending the pipe fd.

    // Create request pipe: [0]=read (server), [1]=write (client)
    let mut request_pipe = [0i32; 2];
    if unsafe { libc::pipe2(request_pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        log_error!("pipe2 failed for socketpair client: {}", std::io::Error::last_os_error());
        return None;
    }

    // Send request_pipe[1] to client via SCM_RIGHTS with runtime_protocol_version()
    let send_result = send_fd(socket_fd, request_pipe[1], runtime_protocol_version());
    if send_result < 0 {
        let err = std::io::Error::last_os_error();
        log_error!("failed to send request_fd to socketpair client: {err}");
        unsafe {
            libc::close(request_pipe[0]);
            libc::close(request_pipe[1]);
        }
        return None;
    }

    // Server keeps read end, closes write end
    unsafe { libc::close(request_pipe[1]); }

    // NOW set socket non-blocking (after handshake send completed)
    unsafe {
        let flags = libc::fcntl(socket_fd, libc::F_GETFL);
        libc::fcntl(socket_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Dup pipe read fd to a fresh number to avoid zombie worker fd reuse race.
    let safe_fd = unsafe { libc::dup(request_pipe[0]) };
    unsafe { libc::close(request_pipe[0]); }
    if safe_fd < 0 {
        log_error!("setup_client_on_socket: dup failed");
        return None;
    }

    unsafe {
        let flags = libc::fcntl(safe_fd, libc::F_GETFL);
        libc::fcntl(safe_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    log_info!("socketpair client: request_fd={safe_fd} msg_fd={socket_fd}");
    Some((Client::new(safe_fd, socket_fd), socket_fd))
}

impl AsRawFd for Listener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

pub struct Client {
    /// Pipe read end -- server reads protocol requests from here.
    pub fd: RawFd,
    /// Connection socket -- used for SCM_RIGHTS fd passing.
    pub msg_fd: RawFd,
    /// Pipe write end -- server writes protocol replies here.
    /// Set during init_first_thread/init_thread when client sends its reply pipe.
    pub reply_fd: Option<RawFd>,
    pub process_id: u32,
    pub thread_id: u32,
    pub suspend_cookie: u64,
    /// Cookie from the current blocking Select (set before spawning ntsync worker).
    /// Used by try_connect_named_pipe to write STATUS_KERNEL_APC to wait_fd
    /// when the thread is in a deferred wait and the PipeListenAsync cookie is 0.
    pub current_wait_cookie: u64,
    /// Pending fd to send to the client after the next reply (for init_first_thread).
    /// The worker sends this via SCM_RIGHTS on msg_fd right before writing the reply.
    pub pending_fd: Option<(RawFd, u32)>,  // (fd, tag)
    // File descriptors received via SCM_RIGHTS ancillary data on msg_fd.
    // Wine sends fds for process sockets, request/reply/wait pipes.
    // Each entry is (thread_id, client_fd_number, actual_fd).
    // thread_id comes from the data payload of wine_server_send_fd and is
    // used to route the fd to the correct thread's handler.
    pub inflight_fds: VecDeque<(u32, i32, RawFd)>,
    // Server→client fd passing channel. Wine creates a socketpair during
    // init_first_thread/init_thread and sends one end to the server.
    // Used by send_fd() to push fds (e.g. ntsync device/object fds) to
    // the client, which receives them via receive_fd() on its end.
    pub wait_fd: Option<RawFd>,
    // Per-thread shared object locators for queue and input (allocated in session memfd)
    pub queue_locator: [u8; 16],
    pub input_locator: [u8; 16],
    // Cached message queue handle for get_msg_queue_handle (0 = not yet created)
    pub queue_handle: u32,
    // Thread metadata from init_thread/init_first_thread (for get_thread_info/get_thread_times)
    pub teb: u64,
    pub entry_point: u64,
    pub unix_pid: i32,
    pub unix_tid: i32,
    // Display driver event fd (X11 display connection). Set by set_queue_fd.
    // When this fd has data, X11 events are available and the message queue
    // should be woken (QS_INPUT). The fd is dup'd from the handle table.
    pub queue_fd: Option<RawFd>,
    // Phantom clients are broker-side mirrors of worker-owned clients.
    // They share fd values but do NOT own them — only the dup'd msg_fd is owned.
    pub is_phantom: bool,
    pub is_msg_primary: bool,
}

impl Client {
    pub fn new(request_fd: RawFd, msg_fd: RawFd) -> Self {
        Self {
            fd: request_fd,
            msg_fd,
            reply_fd: None,
            process_id: 0,
            thread_id: 0,
            suspend_cookie: 0,
            current_wait_cookie: 0,
            inflight_fds: VecDeque::with_capacity(4),
            wait_fd: None,
            queue_locator: [0u8; 16],
            input_locator: [0u8; 16],
            queue_handle: 0,
            teb: 0,
            entry_point: 0,
            unix_pid: 0,
            unix_tid: 0,
            queue_fd: None,
            is_phantom: false,
            is_msg_primary: true,
            pending_fd: None,
        }
    }

    // Take the first inflight fd (FIFO order). Used by handlers that
    // expect fds sent via SCM_RIGHTS (init_first_thread, init_thread).
    pub fn take_inflight_fd(&mut self) -> Option<RawFd> {
        self.inflight_fds.pop_front().map(|(_, _, fd)| fd)
    }

    // Take an inflight fd by its client fd number (matching Wine's
    // thread_get_inflight_fd). Used by new_thread, get_handle_fd, etc.
    pub fn take_inflight_fd_by_number(&mut self, fd_number: i32) -> Option<RawFd> {
        if let Some(pos) = self.inflight_fds.iter().position(|(_, n, _)| *n == fd_number) {
            self.inflight_fds.remove(pos).map(|(_, _, fd)| fd)
        } else {
            None
        }
    }

}

impl Drop for Client {
    fn drop(&mut self) {
        // reply_fd and wait_fd are owned by the Authority thread (received via
        // inflight fds during init_first_thread/init_thread). Close them here.
        if let Some(rfd) = self.reply_fd {
            unsafe { libc::close(rfd); }
        }
        if let Some(wfd) = self.wait_fd {
            unsafe { libc::close(wfd); }
        }
        if let Some(qfd) = self.queue_fd {
            unsafe { libc::close(qfd); }
        }
        for &(_, _, fd) in &self.inflight_fds {
            unsafe { libc::close(fd); }
        }
        // fd (request_fd) and msg_fd are owned by the I/O thread.
        // They are closed in disconnect_io_client() in csp_loop.rs.
        // Closing them here would double-close, corrupting newly-allocated fds.
    }
}

pub fn create_listener(socket_path: &Path) -> Listener {
    // Remove stale socket if it exists
    if socket_path.exists() {
        std::fs::remove_file(socket_path).ok();
    }

    let listener = UnixListener::bind(socket_path)
        .unwrap_or_else(|e| panic!("bind {}: {e}", socket_path.display()));

    listener.set_nonblocking(true)
        .expect("set_nonblocking on listener");

    Listener { inner: listener }
}

/// Send a file descriptor + handle value to a client via SCM_RIGHTS.
/// Used for the initial request_fd handshake and for send_client_fd.
pub fn send_fd(socket_fd: RawFd, fd: RawFd, handle: u32) -> isize {
    let mut handle_buf = handle.to_le_bytes();
    let mut iov = libc::iovec {
        iov_base: handle_buf.as_mut_ptr() as *mut _,
        iov_len: std::mem::size_of::<u32>(),
    };

    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd);
    }

    let n = unsafe { libc::sendmsg(socket_fd, &msg, 0) };
    n as isize
}
