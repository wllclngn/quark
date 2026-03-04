// Client IPC -- Unix domain socket listener and per-client connection state
//
// Wine clients (ntdll/unix/server.c) connect via a Unix domain socket at
// $WINEPREFIX/server-<hostname>-<hash>/socket. Each connection is a
// Wine thread. Messages are fixed-size request/reply pairs.

use std::collections::VecDeque;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::Path;

pub struct Listener {
    inner: UnixListener,
}

impl Listener {
    pub fn fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }

    pub fn accept(&self) -> Option<Client> {
        match self.inner.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(true).ok();
                Some(Client::new(stream.into_raw_fd()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(e) => {
                eprintln!("[triskelion] accept error: {e}");
                None
            }
        }
    }
}

impl AsRawFd for Listener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

pub struct Client {
    pub fd: RawFd,
    pub process_id: u32,
    pub thread_id: u32,
    // Accumulation buffer for partial reads. Wine requests can exceed a
    // single read() call, especially for registry ops and window property
    // sets. We append to this buffer until a complete request is present.
    pub recv_buf: Vec<u8>,
    // File descriptors received via SCM_RIGHTS ancillary data.
    // Wine sends fds for process sockets, request/reply/wait pipes.
    pub inflight_fds: VecDeque<RawFd>,
}

impl Client {
    pub fn new(fd: RawFd) -> Self {
        Self {
            fd,
            process_id: 0,
            thread_id: 0,
            recv_buf: Vec::with_capacity(256),
            inflight_fds: VecDeque::with_capacity(4),
        }
    }

    // Read from socket into internal buffer via recvmsg.
    // Extracts any SCM_RIGHTS file descriptors from ancillary data.
    // Returns bytes read, 0 on disconnect, -1 on EAGAIN.
    pub fn read_into_buf(&mut self) -> isize {
        let mut tmp = [0u8; 4096];
        // cmsg buffer: room for up to 16 fds (matches Wine's MAX_INFLIGHT_FDS)
        const MAX_FDS: usize = 16;
        let cmsg_space = unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) } as usize;
        let mut cmsg_buf = vec![0u8; cmsg_space];

        let mut iov = libc::iovec {
            iov_base: tmp.as_mut_ptr() as *mut _,
            iov_len: tmp.len(),
        };

        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsg_buf.len() as _;

        let n = unsafe { libc::recvmsg(self.fd, &mut msg, 0) };

        if n > 0 {
            self.recv_buf.extend_from_slice(&tmp[..n as usize]);

            // Extract file descriptors from SCM_RIGHTS ancillary messages
            let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
            while !cmsg.is_null() {
                let hdr = unsafe { &*cmsg };
                if hdr.cmsg_level == libc::SOL_SOCKET && hdr.cmsg_type == libc::SCM_RIGHTS {
                    let data_ptr = unsafe { libc::CMSG_DATA(cmsg) };
                    let data_len = hdr.cmsg_len as usize
                        - unsafe { libc::CMSG_LEN(0) } as usize;
                    let fd_count = data_len / std::mem::size_of::<RawFd>();
                    for i in 0..fd_count {
                        let fd = unsafe {
                            std::ptr::read_unaligned(data_ptr.add(i * std::mem::size_of::<RawFd>()) as *const RawFd)
                        };
                        self.inflight_fds.push_back(fd);
                    }
                }
                cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
            }
        }

        n as isize
    }

    // Take the first inflight fd (FIFO order). Used by handlers that
    // expect fds sent via SCM_RIGHTS (new_process, new_thread, init_thread).
    pub fn take_inflight_fd(&mut self) -> Option<RawFd> {
        self.inflight_fds.pop_front()
    }

    // Check if a complete request is buffered. A complete request has
    // at least a RequestHeader (12 bytes), and the total size is
    // header_size (12) + request_size.
    pub fn has_complete_request(&self) -> bool {
        if self.recv_buf.len() < HEADER_SIZE {
            return false;
        }
        let request_size = u32::from_le_bytes([
            self.recv_buf[4], self.recv_buf[5],
            self.recv_buf[6], self.recv_buf[7],
        ]) as usize;
        self.recv_buf.len() >= HEADER_SIZE + request_size
    }

    // Take a complete request from the buffer into a reusable output buffer.
    // After warmup the output Vec never reallocates.
    pub fn take_request(&mut self, out: &mut Vec<u8>) {
        let request_size = u32::from_le_bytes([
            self.recv_buf[4], self.recv_buf[5],
            self.recv_buf[6], self.recv_buf[7],
        ]) as usize;
        let total = HEADER_SIZE + request_size;
        out.clear();
        out.extend(self.recv_buf.drain(..total));
    }

    // Write a reply to the client socket.
    pub fn write_reply(&self, reply: &crate::event_loop::Reply) -> isize {
        match reply {
            crate::event_loop::Reply::Fixed { buf, len } => unsafe {
                libc::write(self.fd, buf.as_ptr() as *const _, *len) as isize
            },
            crate::event_loop::Reply::Vararg(vec) => unsafe {
                libc::write(self.fd, vec.as_ptr() as *const _, vec.len()) as isize
            },
            crate::event_loop::Reply::Deferred => 0,
        }
    }
}

// RequestHeader is 12 bytes: req (i32) + request_size (u32) + reply_size (u32)
const HEADER_SIZE: usize = 12;

impl Drop for Client {
    fn drop(&mut self) {
        for &fd in &self.inflight_fds {
            unsafe { libc::close(fd); }
        }
        unsafe { libc::close(self.fd); }
    }
}

use std::os::unix::net::UnixStream;

trait IntoRawFdExt {
    fn into_raw_fd(self) -> RawFd;
}

impl IntoRawFdExt for UnixStream {
    fn into_raw_fd(self) -> RawFd {
        use std::os::unix::io::IntoRawFd;
        IntoRawFd::into_raw_fd(self)
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
