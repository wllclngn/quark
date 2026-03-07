// Event loop -- the hub that spins the three legs
//
// Uses epoll for fd readiness notification.
// Accepts client connections, reads requests, dispatches to the appropriate
// leg, writes replies.
//
// Single-threaded for protocol correctness first. Partitioned multithreading
// comes later -- the three legs are designed for it (per-thread queues,
// per-process handles, per-object sync state).

use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::time::Instant;

use crate::ipc::{Client, Listener};
use crate::objects::ServerState;
use crate::protocol::*;
use crate::registry::Registry;
use crate::shm::ShmManager;
use crate::SHUTDOWN;

const MAX_EVENTS: usize = 64;

const MAX_OPCODES: usize = 306;

struct PendingWait {
    client_fd: RawFd,
    deadline: Instant,
    // ntsync wait context (empty = timeout-only wait, no kernel objects)
    ntsync_fds: Vec<RawFd>,
    wait_all: bool,
    owner: u32,
}

pub struct EventLoop {
    epoll_fd: RawFd,
    listener_fd: RawFd,
    signal_fd: RawFd,
    timer_fd: RawFd,
    clients: HashMap<RawFd, Client>,
    listener: Listener,
    state: ServerState,
    registry: Registry,
    shm: ShmManager,
    opcode_counts: [u64; MAX_OPCODES],
    total_requests: u64,
    pending_waits: Vec<PendingWait>,
    request_buf: Vec<u8>,
    start_time: Instant,
    peak_clients: usize,
    process_init_count: u64,
    thread_init_count: u64,
    // ntsync: kernel-native NT sync (None = older kernel, fallback to stubs)
    ntsync: Option<crate::ntsync::NtsyncDevice>,
    ntsync_objects: HashMap<obj_handle_t, (crate::ntsync::NtsyncObj, u32)>,
    ntsync_objects_created: u64,
}

impl EventLoop {
    pub fn new(listener: Listener, signal_fd: RawFd, shm: ShmManager) -> Self {
        let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        assert!(epoll_fd >= 0, "epoll_create1 failed");

        let listener_fd = listener.fd();

        let timer_fd = unsafe {
            libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC)
        };
        assert!(timer_fd >= 0, "timerfd_create failed");

        epoll_add(epoll_fd, listener_fd, libc::EPOLLIN as u32);
        epoll_add(epoll_fd, signal_fd, libc::EPOLLIN as u32);
        epoll_add(epoll_fd, timer_fd, libc::EPOLLIN as u32);

        let ntsync = crate::ntsync::NtsyncDevice::open();
        if ntsync.is_some() {
            eprintln!("[triskelion] ntsync: /dev/ntsync available, kernel-native sync enabled");
        } else {
            eprintln!("[triskelion] ntsync: /dev/ntsync not available, using stub sync");
        }

        Self {
            epoll_fd,
            listener_fd,
            signal_fd,
            timer_fd,
            clients: HashMap::new(),
            listener,
            state: ServerState::new(),
            registry: Registry::new(),
            shm,
            opcode_counts: [0; MAX_OPCODES],
            total_requests: 0,
            pending_waits: Vec::new(),
            request_buf: Vec::with_capacity(512),
            start_time: Instant::now(),
            peak_clients: 0,
            process_init_count: 0,
            thread_init_count: 0,
            ntsync,
            ntsync_objects: HashMap::new(),
            ntsync_objects_created: 0,
        }
    }

    pub fn tick(&mut self) {
        // Check pending waits before blocking in epoll
        self.check_pending_waits();

        let mut events: [libc::epoll_event; MAX_EVENTS] = unsafe { std::mem::zeroed() };

        // When waits are pending, timerfd fires at the nearest deadline.
        // When idle, 100ms safety-net timeout for housekeeping.
        let epoll_timeout = if self.pending_waits.is_empty() { 100 } else { -1 };

        let n = unsafe {
            libc::epoll_wait(self.epoll_fd, events.as_mut_ptr(), MAX_EVENTS as i32, epoll_timeout)
        };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                eprintln!("[triskelion] epoll_wait: {err}");
            }
            return;
        }

        for i in 0..n as usize {
            let fd = events[i].u64 as RawFd;

            if fd == self.signal_fd {
                self.handle_signal();
            } else if fd == self.listener_fd {
                self.accept_clients();
            } else if fd == self.timer_fd {
                self.handle_timer();
            } else {
                self.handle_client(fd);
            }
        }
    }

    fn check_pending_waits(&mut self) {
        if self.pending_waits.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut i = 0;
        while i < self.pending_waits.len() {
            // Try ntsync poll first (if this wait has ntsync FDs)
            if !self.pending_waits[i].ntsync_fds.is_empty() {
                if let Some(ntsync) = &self.ntsync {
                    let fds = &self.pending_waits[i].ntsync_fds;
                    let wait_all = self.pending_waits[i].wait_all;
                    let owner = self.pending_waits[i].owner;
                    let result = if wait_all {
                        ntsync.wait_all(fds, 0, owner)
                    } else {
                        ntsync.wait_any(fds, 0, owner)
                    };
                    match result {
                        crate::ntsync::WaitResult::Signaled(index) => {
                            let pending = self.pending_waits.swap_remove(i);
                            let reply = SelectReply {
                                header: ReplyHeader { error: 0, reply_size: 0 },
                                apc_handle: 0,
                                signaled: index as i32,
                            };
                            if let Some(client) = self.clients.get(&pending.client_fd) {
                                client.write_reply(&reply_fixed(&reply));
                            }
                            continue; // don't increment i (swap_remove shifted)
                        }
                        _ => {} // not yet signaled, check deadline below
                    }
                }
            }

            if now >= self.pending_waits[i].deadline {
                let pending = self.pending_waits.swap_remove(i);
                // Reply with STATUS_TIMEOUT
                let reply = SelectReply {
                    header: ReplyHeader { error: 0x0000_0102, reply_size: 0 },
                    apc_handle: 0,
                    signaled: 0,
                };
                if let Some(client) = self.clients.get(&pending.client_fd) {
                    client.write_reply(&reply_fixed(&reply));
                }
            } else {
                i += 1;
            }
        }
        self.arm_timer();
    }

    fn handle_timer(&mut self) {
        // Read timerfd to acknowledge (prevents re-triggering)
        let mut val = 0u64;
        unsafe { libc::read(self.timer_fd, &mut val as *mut _ as *mut _, 8); }
        self.check_pending_waits();
    }

    // Arm timerfd to fire at the nearest pending wait deadline.
    // Disarms if no waits are pending.
    fn arm_timer(&self) {
        let spec = if let Some(nearest) = self.pending_waits.iter().map(|pw| pw.deadline).min() {
            let now = Instant::now();
            let dur = if nearest > now {
                nearest - now
            } else {
                std::time::Duration::from_nanos(1) // already expired, fire immediately
            };
            libc::itimerspec {
                it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
                it_value: libc::timespec {
                    tv_sec: dur.as_secs() as i64,
                    tv_nsec: dur.subsec_nanos() as i64,
                },
            }
        } else {
            // Disarm
            libc::itimerspec {
                it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
                it_value: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            }
        };
        unsafe { libc::timerfd_settime(self.timer_fd, 0, &spec, std::ptr::null_mut()); }
    }

    // Re-arm timer to fire immediately (1ns). Called after signal/release
    // operations that may wake pending ntsync waits.
    fn arm_timer_immediate(&self) {
        if self.pending_waits.is_empty() { return; }
        let spec = libc::itimerspec {
            it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
            it_value: libc::timespec { tv_sec: 0, tv_nsec: 1 },
        };
        unsafe { libc::timerfd_settime(self.timer_fd, 0, &spec, std::ptr::null_mut()); }
    }

    fn handle_signal(&self) {
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        unsafe {
            libc::read(
                self.signal_fd,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<libc::signalfd_siginfo>(),
            );
        }
        eprintln!("[triskelion] received signal {}", info.ssi_signo);
        SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn accept_clients(&mut self) {
        while let Some(client) = self.listener.accept() {
            let fd = client.fd;
            epoll_add(self.epoll_fd, fd, libc::EPOLLIN as u32);
            self.clients.insert(fd, client);
            if self.clients.len() > self.peak_clients {
                self.peak_clients = self.clients.len();
            }
        }
    }

    fn handle_client(&mut self, fd: RawFd) {
        let n = if let Some(client) = self.clients.get_mut(&fd) {
            client.read_into_buf()
        } else {
            return;
        };

        if n <= 0 {
            self.disconnect_client(fd);
            return;
        }

        // Extract reusable buffer to avoid borrow conflicts with self.dispatch()
        let mut req_buf = std::mem::take(&mut self.request_buf);

        // Process all complete requests in the buffer (handles pipelining)
        while self.clients.get(&fd).map_or(false, |c| c.has_complete_request()) {
            self.clients.get_mut(&fd).unwrap().take_request(&mut req_buf);

            let header: RequestHeader = unsafe {
                std::ptr::read_unaligned(req_buf.as_ptr() as *const RequestHeader)
            };

            let reply = self.dispatch(fd, &header, &req_buf);

            if !matches!(reply, Reply::Deferred) {
                if let Some(client) = self.clients.get(&fd) {
                    client.write_reply(&reply);
                }
            }
        }

        self.request_buf = req_buf;
    }

    fn dispatch(&mut self, client_fd: RawFd, header: &RequestHeader, buf: &[u8]) -> Reply {
        match RequestCode::from_i32(header.req) {
            Some(code) => {
                let idx = header.req as usize;
                if idx < MAX_OPCODES {
                    self.opcode_counts[idx] += 1;
                }
                self.total_requests += 1;
                dispatch_request(code, self, client_fd as i32, buf)
            }
            None => reply_fixed(&ReplyHeader { error: 0xC0000002, reply_size: 0 }),
        }
    }

    // Helpers

    fn client_thread_id(&self, client_fd: RawFd) -> Option<thread_id_t> {
        self.clients.get(&client_fd)
            .and_then(|c| if c.thread_id != 0 { Some(c.thread_id) } else { None })
    }

    fn disconnect_client(&mut self, fd: RawFd) {
        epoll_del(self.epoll_fd, fd);
        self.clients.remove(&fd);
        // Drop any pending waits for this client
        self.pending_waits.retain(|pw| pw.client_fd != fd);
    }
}

// RequestHandler trait impl -- override only the handlers we've implemented.
// The remaining ~300 opcodes get the default STATUS_NOT_IMPLEMENTED stub
// from the generated trait. Adding a new handler = writing one method here.
impl RequestHandler for EventLoop {
    fn handle_new_process(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<NewProcessRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const NewProcessRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Take the socket fd sent via SCM_RIGHTS
        let socket_fd = self.clients.get_mut(&(client_fd as RawFd))
            .and_then(|c| c.take_inflight_fd());

        let pid = self.state.create_process();
        self.process_init_count += 1;

        // Extract VARARG startup info: skip fixed struct, then skip
        // handles_size + jobs_size bytes to get startup_info + env
        let fixed_end = std::mem::size_of::<NewProcessRequest>();
        let vararg_start = fixed_end + req.handles_size as usize + req.jobs_size as usize;
        let startup_info = if vararg_start < buf.len() {
            Some(buf[vararg_start..].to_vec())
        } else {
            None
        };

        if let Some(process) = self.state.processes.get_mut(&pid) {
            process.startup_info = startup_info;
            process.info_size = req.info_size;
            process.machine = req.machine;
            process.socket_fd = socket_fd;
        }

        // Allocate handle in parent's handle table
        let parent_pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let handle = if let Some(ppid) = parent_pid {
            if let Some(parent) = self.state.processes.get_mut(&ppid) {
                parent.handles.allocate(pid as u64)
            } else { 0 }
        } else { 0 };

        let info = self.state.alloc_info_handle(pid);

        if crate::log::is_verbose() {
            eprintln!("[triskelion] new_process: pid={pid} handle={handle} info={info}");
        }

        let reply = NewProcessReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            info,
            pid,
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_get_new_process_info(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetNewProcessInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetNewProcessInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let (success, exit_code) = self.state.process_info_handles.get(&req.info)
            .and_then(|h| self.state.processes.get(&h.target_pid))
            .map(|p| (if p.startup_done { 1 } else { 0 }, p.exit_code))
            .unwrap_or((0, 0));

        let reply = GetNewProcessInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            success,
            exit_code,
        };
        reply_fixed(&reply)
    }

    fn handle_new_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<NewThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const NewThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Take the request fd sent via SCM_RIGHTS (consume, close)
        if let Some(fd) = self.clients.get_mut(&(client_fd as RawFd))
            .and_then(|c| c.take_inflight_fd()) {
            unsafe { libc::close(fd); }
        }

        // Resolve target process from handle
        let caller_pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let target_pid = caller_pid.and_then(|ppid| {
            self.state.processes.get(&ppid)
                .and_then(|p| p.handles.get(req.process))
                .map(|h| h.object_id as process_id_t)
        }).unwrap_or_else(|| {
            // Fallback: use any existing process
            self.state.processes.keys().next().copied().unwrap_or(0)
        });

        let tid = self.state.create_thread(target_pid);
        self.thread_init_count += 1;
        let handle = if let Some(ppid) = caller_pid {
            if let Some(parent) = self.state.processes.get_mut(&ppid) {
                parent.handles.allocate(tid as u64)
            } else { 0 }
        } else { 0 };

        if crate::log::is_verbose() {
            eprintln!("[triskelion] new_thread: tid={tid} handle={handle} target_pid={target_pid}");
        }

        let reply = NewThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            tid,
            handle,
        };
        reply_fixed(&reply)
    }

    fn handle_init_first_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<InitFirstThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InitFirstThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Drain 2 inflight fds: reply_fd (close) and wait_fd (keep for fd passing)
        if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
            // First fd: reply_fd — unused in triskelion
            if let Some(fd) = client.take_inflight_fd() {
                unsafe { libc::close(fd); }
            }
            // Second fd: wait_fd — server→client fd passing channel (ntsync)
            if let Some(fd) = client.take_inflight_fd() {
                client.wait_fd = Some(fd);
            }
        }

        // Find existing process (created by new_process) or create one
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .unwrap_or_else(|| self.state.create_process());

        let slot = self.shm.alloc_slot(req.unix_tid as thread_id_t);
        let tid = self.state.create_thread(pid);
        self.thread_init_count += 1;

        if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
            client.thread_id = tid;
            client.process_id = pid;
        }

        if crate::log::is_verbose() {
            eprintln!("[triskelion] init_first_thread: pid={pid} tid={tid} unix_tid={} shm_slot={slot}",
                      req.unix_tid);
        }

        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
        let server_start = (ts.tv_sec as i64) * 10_000_000 + (ts.tv_nsec as i64) / 100;

        let reply = InitFirstThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            pid,
            tid,
            server_start,
            session_id: 0,
            info_size: 0,
        };
        reply_fixed(&reply)
    }

    fn handle_init_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<InitThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InitThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Drain 2 inflight fds: reply_fd (close) and wait_fd (keep for fd passing)
        if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
            // First fd: reply_fd — unused in triskelion
            if let Some(fd) = client.take_inflight_fd() {
                unsafe { libc::close(fd); }
            }
            // Second fd: wait_fd — server→client fd passing channel (ntsync)
            if let Some(fd) = client.take_inflight_fd() {
                client.wait_fd = Some(fd);
            }
        }

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .unwrap_or_else(|| {
                self.state.processes.keys().next().copied().unwrap_or_else(|| {
                    self.state.create_process()
                })
            });

        let slot = self.shm.alloc_slot(req.unix_tid as thread_id_t);
        let tid = self.state.create_thread(pid);

        if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
            client.thread_id = tid;
            client.process_id = pid;
        }

        if crate::log::is_verbose() {
            eprintln!("[triskelion] init_thread: pid={pid} tid={tid} unix_tid={} shm_slot={slot}",
                      req.unix_tid);
        }

        let reply = InitThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            suspend: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_get_startup_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });

        let (info_size, machine, vararg) = pid
            .and_then(|p| self.state.processes.get_mut(&p))
            .map(|process| {
                let info = process.startup_info.take().unwrap_or_default();
                (process.info_size, process.machine, info)
            })
            .unwrap_or((0, 0x8664, Vec::new()));

        let max_vararg = max_reply_vararg(buf) as usize;
        let send_len = vararg.len().min(max_vararg);
        let vararg_slice = &vararg[..send_len];

        let reply = GetStartupInfoReply {
            header: ReplyHeader { error: 0, reply_size: send_len as u32 },
            info_size,
            machine,
            _pad_0: [0; 2],
        };

        if crate::log::is_verbose() {
            eprintln!("[triskelion] get_startup_info: info_size={info_size} vararg={send_len}");
        }
        reply_vararg(&reply, vararg_slice)
    }

    fn handle_init_process_done(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<InitProcessDoneRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InitProcessDoneRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });

        if let Some(process) = pid.and_then(|p| self.state.processes.get_mut(&p)) {
            process.peb = req.peb;
            process.startup_done = true;
        }

        if crate::log::is_verbose() {
            eprintln!("[triskelion] init_process_done: pid={:?} peb=0x{:x}", pid, req.peb);
        }

        let reply = InitProcessDoneReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            suspend: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_get_message(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let tid = self.client_thread_id(client_fd as RawFd);

        if let Some(queue) = tid.and_then(|t| self.shm.get_queue(t)) {
            if let Some(msg) = queue.get() {
                let reply = GetMessageReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    win: msg.win,
                    msg: msg.msg,
                    wparam: msg.wparam,
                    lparam: msg.lparam,
                    r#type: msg.msg_type,
                    x: msg.x,
                    y: msg.y,
                    time: msg.time,
                    total: 0,
                    _pad_0: [0; 4],
                };
                return reply_fixed(&reply);
            }
        }

        reply_fixed(&ReplyHeader { error: 0x00000103, reply_size: 0 }) // STATUS_PENDING
    }

    fn handle_get_queue_status(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let tid = self.client_thread_id(client_fd as RawFd);

        if let Some(queue) = tid.and_then(|t| self.shm.get_queue(t)) {
            let wake_bits = queue.wake_bits.load(std::sync::atomic::Ordering::Acquire);
            let reply = GetQueueStatusReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                wake_bits,
                changed_bits: wake_bits, // no separate changed tracking
            };
            return reply_fixed(&reply);
        }

        reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }) // STATUS_INVALID_HANDLE
    }

    fn handle_send_message(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SendMessageRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SendMessageRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        if let Some(queue) = self.shm.get_queue(req.id) {
            let msg = crate::queue::QueuedMessage {
                win: req.win,
                msg: req.msg,
                wparam: req.wparam,
                lparam: req.lparam,
                msg_type: req.r#type,
                x: 0,
                y: 0,
                time: 0,
                _pad: [0; 2],
            };

            if req.r#type == MSG_POSTED || req.r#type == MSG_NOTIFY {
                if queue.post(msg) {
                    return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
                }
            }
            // Non-posted message types (SendMessage, etc.) fall through to stub
        }

        reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }) // STATUS_INVALID_HANDLE
    }

    fn handle_close_handle(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let tid = self.client_thread_id(client_fd as RawFd);
        let pid = tid.and_then(|t| self.state.threads.get(&t).map(|th| th.pid));

        if let Some(pid) = pid {
            if buf.len() >= std::mem::size_of::<CloseHandleRequest>() {
                let req: CloseHandleRequest = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr() as *const _)
                };
                // Clean up ntsync object (Drop closes the kernel FD)
                self.ntsync_objects.remove(&req.handle);
                if let Some(process) = self.state.processes.get_mut(&pid) {
                    process.handles.close(req.handle);
                }
            }
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    // ---- Registry handlers ----

    fn handle_create_key(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() < std::mem::size_of::<CreateKeyRequest>() {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        }

        let vararg = &buf[std::mem::size_of::<CreateKeyRequest>()..];
        let (rootdir, name) = crate::registry::parse_objattr_name(vararg);
        let parent = if rootdir != 0 { rootdir } else { 0 };
        let (hkey, _created) = self.registry.create_key(parent, name);

        let reply = CreateKeyReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            hkey,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_open_key(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<OpenKeyRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const OpenKeyRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let vararg = &buf[std::mem::size_of::<OpenKeyRequest>()..];
        // open_key VARARG is just unicode_str (no objattr wrapper)
        if let Some(hkey) = self.registry.open_key(req.parent, vararg) {
            let reply = OpenKeyReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                hkey,
                _pad_0: [0; 4],
            };
            reply_fixed(&reply)
        } else {
            reply_fixed(&ReplyHeader { error: 0xC0000034, reply_size: 0 }) // STATUS_OBJECT_NAME_NOT_FOUND
        }
    }

    fn handle_get_key_value(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetKeyValueRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetKeyValueRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let vararg = &buf[std::mem::size_of::<GetKeyValueRequest>()..];
        if let Some((data_type, data)) = self.registry.get_value(req.hkey, vararg) {
            let max = max_reply_vararg(buf) as usize;
            let send_len = data.len().min(max);
            let reply = GetKeyValueReply {
                header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                r#type: data_type as i32,
                total: data.len() as u32,
            };
            reply_vararg(&reply, &data[..send_len])
        } else {
            reply_fixed(&ReplyHeader { error: 0xC0000034, reply_size: 0 }) // STATUS_OBJECT_NAME_NOT_FOUND
        }
    }

    fn handle_set_key_value(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetKeyValueRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetKeyValueRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let vararg = &buf[std::mem::size_of::<SetKeyValueRequest>()..];
        let namelen = req.namelen as usize;
        if vararg.len() >= namelen {
            let name = &vararg[..namelen];
            let data = &vararg[namelen..];
            self.registry.set_value(req.hkey, name, req.r#type as u32, data);
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    fn handle_enum_key_value(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<EnumKeyValueRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const EnumKeyValueRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        if let Some((name_bytes, data_type, data)) = self.registry.enum_value(req.hkey, req.index as usize) {
            let max = max_reply_vararg(buf) as usize;
            let mut vararg = name_bytes.clone();
            vararg.extend_from_slice(data);
            let send_len = vararg.len().min(max);

            let reply = EnumKeyValueReply {
                header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                r#type: data_type as i32,
                total: data.len() as u32,
                namelen: name_bytes.len() as u32,
                _pad_0: [0; 4],
            };
            reply_vararg(&reply, &vararg[..send_len])
        } else {
            reply_fixed(&ReplyHeader { error: 0x8000001A, reply_size: 0 }) // STATUS_NO_MORE_ENTRIES
        }
    }

    // ---- Startup stubs (no-op success) ----

    fn handle_load_registry(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    fn handle_set_handle_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = SetHandleInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            old_flags: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_get_process_info(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .unwrap_or(0);

        let reply = GetProcessInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            pid,
            ppid: 0,
            affinity: u64::MAX,
            peb: 0,
            start_time: 0,
            end_time: 0,
            session_id: 0,
            exit_code: 0,
            priority: 8, // NORMAL_PRIORITY_CLASS
            machine: 0x8664,
            _pad_0: [0; 2],
        };
        reply_fixed(&reply)
    }

    fn handle_get_thread_info(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .unwrap_or(0);

        let reply = GetThreadInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            pid,
            tid,
            teb: 0,
            entry_point: 0,
            affinity: u64::MAX,
            exit_code: 0,
            priority: 0,
            last: 0,
            suspend_count: 0,
            flags: 0,
            desc_len: 0,
        };
        reply_fixed(&reply)
    }

    fn handle_set_queue_fd(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    fn handle_set_queue_mask(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = SetQueueMaskReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            wake_bits: 0,
            changed_bits: 0,
        };
        reply_fixed(&reply)
    }

    fn handle_terminate_process(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = TerminateProcessReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            is_self: 1,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_terminate_thread(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = TerminateThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            is_self: 1,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_flush_key(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = FlushKeyReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            timestamp_counter: 0,
            total: 0,
            branch_count: 0,
        };
        reply_fixed(&reply)
    }

    fn handle_flush_key_done(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    // ---- Sync primitives (critical -- NOT_IMPLEMENTED here = system freeze) ----

    fn handle_select(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        // select is Wine's universal wait/sleep mechanism.
        // Returning immediately causes a CPU spin. We must defer the reply
        // for timed waits, and handle polls (timeout=0) immediately.
        let req = if buf.len() >= std::mem::size_of::<SelectRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SelectRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // timeout: 0 = poll (return immediately)
        //          negative = relative timeout in 100ns units
        //          positive = absolute time (Windows FILETIME)
        let has_objects = req.size > 0;

        // Parse select_op to extract wait handles (if ntsync available)
        // VARARG layout: [apc_result(40 bytes)] [select_op(req.size bytes)] [contexts...]
        // select_op: [opcode(u32)] [handles(u32 each)...]
        const APC_RESULT_SIZE: usize = 40;
        const SELECT_WAIT: u32 = 1;
        const SELECT_WAIT_ALL: u32 = 2;

        let mut ntsync_fds: Vec<RawFd> = Vec::new();
        let mut wait_all = false;
        let mut owner: u32 = 0;

        if has_objects && self.ntsync.is_some() {
            let select_op_offset = std::mem::size_of::<SelectRequest>() + APC_RESULT_SIZE;
            if buf.len() >= select_op_offset + 4 && req.size >= 4 {
                let opcode = u32::from_le_bytes([
                    buf[select_op_offset], buf[select_op_offset + 1],
                    buf[select_op_offset + 2], buf[select_op_offset + 3],
                ]);
                wait_all = opcode == SELECT_WAIT_ALL;

                if opcode == SELECT_WAIT || opcode == SELECT_WAIT_ALL {
                    let handle_count = ((req.size as usize) - 4) / 4;
                    let handles_start = select_op_offset + 4;
                    let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
                    owner = tid;

                    let mut all_have_ntsync = true;
                    for h_idx in 0..handle_count {
                        let off = handles_start + h_idx * 4;
                        if off + 4 > buf.len() { all_have_ntsync = false; break; }
                        let handle = u32::from_le_bytes([
                            buf[off], buf[off + 1], buf[off + 2], buf[off + 3],
                        ]);
                        if let Some((obj, _)) = self.ntsync_objects.get(&handle) {
                            ntsync_fds.push(obj.fd());
                        } else {
                            all_have_ntsync = false;
                            break;
                        }
                    }
                    if !all_have_ntsync {
                        ntsync_fds.clear(); // fall back to legacy for mixed waits
                    }
                }
            }
        }

        // Try immediate ntsync poll for object waits
        if !ntsync_fds.is_empty() {
            if let Some(ntsync) = &self.ntsync {
                let result = if wait_all {
                    ntsync.wait_all(&ntsync_fds, 0, owner)
                } else {
                    ntsync.wait_any(&ntsync_fds, 0, owner)
                };
                match result {
                    crate::ntsync::WaitResult::Signaled(index) => {
                        let reply = SelectReply {
                            header: ReplyHeader { error: 0, reply_size: 0 },
                            apc_handle: 0,
                            signaled: index as i32,
                        };
                        return reply_fixed(&reply);
                    }
                    _ => {
                        // Not signaled yet -- if poll mode, return timeout
                        if req.timeout == 0 {
                            let reply = SelectReply {
                                header: ReplyHeader { error: 0x0000_0102, reply_size: 0 },
                                apc_handle: 0,
                                signaled: 0,
                            };
                            return reply_fixed(&reply);
                        }
                        // Otherwise fall through to defer with ntsync_fds
                    }
                }
            }
        } else if req.timeout == 0 {
            // Poll without ntsync: return immediately
            let reply = SelectReply {
                header: ReplyHeader { error: 0x0000_0102, reply_size: 0 }, // STATUS_TIMEOUT
                apc_handle: 0,
                signaled: 0,
            };
            return reply_fixed(&reply);
        }

        // Compute wait duration
        let duration_ns = if has_objects && req.timeout < 0 {
            (-req.timeout as u64) * 100
        } else if has_objects {
            // With ntsync: still defer, but we'll poll objects on timer
            1_000_000 // 1ms retry interval
        } else if req.timeout < 0 {
            (-req.timeout as u64) * 100
        } else {
            10_000_000 // 10ms fallback
        };

        let duration_ns = duration_ns.min(5_000_000_000);
        let deadline = Instant::now() + std::time::Duration::from_nanos(duration_ns);

        self.pending_waits.push(PendingWait {
            client_fd: client_fd as RawFd,
            deadline,
            ntsync_fds,
            wait_all,
            owner,
        });
        self.arm_timer();

        Reply::Deferred
    }

    fn handle_create_event(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        // Parse request fields for ntsync
        let (manual_reset, initial_state) = if buf.len() >= std::mem::size_of::<CreateEventRequest>() {
            let req: CreateEventRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            (req.manual_reset != 0, req.initial_state != 0)
        } else {
            (false, false)
        };

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let handle = if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.handles.allocate(0)
            } else { 0 }
        } else { 0 };

        // Create kernel ntsync object if available
        if handle != 0 {
            if let Some(ntsync) = &self.ntsync {
                if let Some(obj) = ntsync.create_event(manual_reset, initial_state) {
                    let sync_type = if manual_reset { 4u32 } else { 3u32 }; // MANUAL_EVENT=4, AUTO_EVENT=3
                    self.ntsync_objects.insert(handle, (obj, sync_type));
                    self.ntsync_objects_created += 1;
                }
            }
        }

        let reply = CreateEventReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_event_op(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() >= std::mem::size_of::<EventOpRequest>() {
            let req: EventOpRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            if let Some((obj, _)) = self.ntsync_objects.get(&req.handle) {
                // Wine event_op codes: PULSE_EVENT=0, SET_EVENT=1, RESET_EVENT=2
                let prev = match req.op {
                    1 => obj.event_set().unwrap_or(0),
                    2 => obj.event_reset().unwrap_or(0),
                    0 => obj.event_pulse().unwrap_or(0),
                    _ => 0,
                };
                // Signal operation may wake pending waits -- re-arm timer immediately
                if req.op == 1 || req.op == 0 {
                    self.arm_timer_immediate();
                }
                let reply = EventOpReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    state: prev as i32,
                    _pad_0: [0; 4],
                };
                return reply_fixed(&reply);
            }
        }
        // Fallback: stub success
        let reply = EventOpReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            state: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_create_esync(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Return NOT_IMPLEMENTED so Wine falls back to server-based sync.
        // This is safe because select (above) handles the server path.
        reply_fixed(&ReplyHeader { error: 0xC000_0002, reply_size: 0 })
    }

    fn handle_get_esync_fd(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0xC000_0002, reply_size: 0 })
    }

    fn handle_get_esync_apc_fd(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0xC000_0002, reply_size: 0 })
    }

    fn handle_create_fsync(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Return NOT_IMPLEMENTED so Wine disables fsync and uses server sync
        reply_fixed(&ReplyHeader { error: 0xC000_0002, reply_size: 0 })
    }

    // ---- Additional critical stubs to prevent hangs ----

    fn handle_create_mutex(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        // Parse request: if owned=1, create with owner=client's thread ID
        let owned = if buf.len() >= std::mem::size_of::<CreateMutexRequest>() {
            let req: CreateMutexRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            req.owned != 0
        } else {
            false
        };

        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let handle = if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.handles.allocate(0)
            } else { 0 }
        } else { 0 };

        // Create kernel ntsync mutex
        if handle != 0 {
            if let Some(ntsync) = &self.ntsync {
                let (owner, count) = if owned { (tid, 1) } else { (0, 0) };
                if let Some(obj) = ntsync.create_mutex(owner, count) {
                    self.ntsync_objects.insert(handle, (obj, 2)); // MUTEX=2
                    self.ntsync_objects_created += 1;
                }
            }
        }

        let reply = CreateMutexReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_create_semaphore(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let (initial, max) = if buf.len() >= std::mem::size_of::<CreateSemaphoreRequest>() {
            let req: CreateSemaphoreRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            (req.initial, req.max)
        } else {
            (0, 1)
        };

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let handle = if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.handles.allocate(0)
            } else { 0 }
        } else { 0 };

        // Create kernel ntsync semaphore
        if handle != 0 {
            if let Some(ntsync) = &self.ntsync {
                if let Some(obj) = ntsync.create_sem(initial, max) {
                    self.ntsync_objects.insert(handle, (obj, 1)); // SEMAPHORE=1
                    self.ntsync_objects_created += 1;
                }
            }
        }

        let reply = CreateSemaphoreReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_release_semaphore(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() >= std::mem::size_of::<ReleaseSemaphoreRequest>() {
            let req: ReleaseSemaphoreRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            if let Some((obj, _)) = self.ntsync_objects.get(&req.handle) {
                match obj.sem_release(req.count) {
                    Ok(prev) => {
                        self.arm_timer_immediate();
                        let reply = ReleaseSemaphoreReply {
                            header: ReplyHeader { error: 0, reply_size: 0 },
                            prev_count: prev,
                            _pad_0: [0; 4],
                        };
                        return reply_fixed(&reply);
                    }
                    Err(_) => {} // fall through to stub
                }
            }
        }
        let reply = ReleaseSemaphoreReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            prev_count: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_release_mutex(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() >= std::mem::size_of::<ReleaseMutexRequest>() {
            let req: ReleaseMutexRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            if let Some((obj, _)) = self.ntsync_objects.get(&req.handle) {
                let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
                match obj.mutex_unlock(tid) {
                    Ok(prev) => {
                        self.arm_timer_immediate();
                        let reply = ReleaseMutexReply {
                            header: ReplyHeader { error: 0, reply_size: 0 },
                            prev_count: prev,
                            _pad_0: [0; 4],
                        };
                        return reply_fixed(&reply);
                    }
                    Err(_) => {} // fall through to stub
                }
            }
        }
        let reply = ReleaseMutexReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            prev_count: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    fn handle_open_event(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let handle = if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.handles.allocate(0)
            } else { 0 }
        } else { 0 };

        let reply = OpenEventReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

}

impl Drop for EventLoop {
    fn drop(&mut self) {
        if self.total_requests > 0 && crate::log::is_verbose() {
            self.dump_opcode_stats();
            self.write_session_prom();
        }
        unsafe {
            libc::close(self.timer_fd);
            libc::close(self.epoll_fd);
        }
    }
}

impl EventLoop {
    fn dump_opcode_stats(&self) {
        eprintln!("[triskelion] opcode stats ({} total requests):", self.total_requests);

        let mut sorted: Vec<(usize, u64)> = self.opcode_counts.iter()
            .enumerate()
            .filter(|(_, c)| **c > 0)
            .map(|(i, c)| (i, *c))
            .collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));

        for (idx, count) in &sorted {
            let name = RequestCode::from_i32(*idx as i32)
                .map(|c| c.as_str())
                .unwrap_or("unknown");
            let pct = *count as f64 / self.total_requests as f64 * 100.0;
            eprintln!("  {count:>8}  {pct:>5.1}%  {name}");
        }

        // Write to file for later analysis
        let log_dir = "/tmp/amphetamine";
        let _ = std::fs::create_dir_all(log_dir);
        let log_path = format!("{log_dir}/triskelion_opcode_stats.txt");
        if let Ok(mut f) = std::fs::File::create(&log_path) {
            use std::io::Write;
            let _ = writeln!(f, "triskelion opcode stats ({} total)", self.total_requests);
            for (idx, count) in &sorted {
                let name = RequestCode::from_i32(*idx as i32)
                    .map(|c| c.as_str())
                    .unwrap_or("unknown");
                let _ = writeln!(f, "{count:>8}  {name}");
            }
            eprintln!("[triskelion] stats written to {log_path}");
        }
    }

    fn write_session_prom(&self) {
        use crate::log::{PromWriter, filename_timestamp};

        let mut w = PromWriter::new();
        w.timestamp_header();

        // ---- Session overview ----
        let uptime = self.start_time.elapsed();
        w.separator();
        w.header("amphetamine_session_uptime_seconds", "Wineserver session duration", "gauge");
        w.gauge("amphetamine_session_uptime_seconds", uptime.as_secs());

        w.header("amphetamine_session_total_requests", "Total protocol requests processed", "gauge");
        w.gauge("amphetamine_session_total_requests", self.total_requests);

        w.header("amphetamine_session_peak_clients", "Peak concurrent client connections", "gauge");
        w.gauge("amphetamine_session_peak_clients", self.peak_clients as u64);

        w.header("amphetamine_session_process_inits", "Total process initializations", "gauge");
        w.gauge("amphetamine_session_process_inits", self.process_init_count);

        w.header("amphetamine_session_thread_inits", "Total thread initializations", "gauge");
        w.gauge("amphetamine_session_thread_inits", self.thread_init_count);

        // ---- ntsync ----
        w.separator();
        w.header("amphetamine_ntsync_available", "Kernel ntsync driver available (1=yes)", "gauge");
        w.gauge("amphetamine_ntsync_available", if self.ntsync.is_some() { 1u64 } else { 0 });

        w.header("amphetamine_ntsync_objects_created", "Total ntsync objects created", "gauge");
        w.gauge("amphetamine_ntsync_objects_created", self.ntsync_objects_created);

        // ---- Per-opcode counts ----
        w.separator();
        w.header("amphetamine_opcode_count", "Per-opcode request count", "gauge");
        for (idx, &count) in self.opcode_counts.iter().enumerate() {
            if count > 0 {
                let name = RequestCode::from_i32(idx as i32)
                    .map(|c| c.as_str())
                    .unwrap_or("unknown");
                w.gauge_labeled("amphetamine_opcode_count", "opcode", name, count);
            }
        }

        let log_dir = crate::log::log_dir();
        let ts = filename_timestamp();
        let filename = format!("session-{ts}.prom");

        match w.write_to(&log_dir, &filename, "session-latest.prom") {
            Ok(p) => eprintln!("[triskelion] session diagnostics: {}", p.display()),
            Err(e) => eprintln!("[triskelion] cannot write session diagnostics: {e}"),
        }
    }
}

fn epoll_add(epoll_fd: RawFd, fd: RawFd, events: u32) {
    let mut ev = libc::epoll_event {
        events,
        u64: fd as u64,
    };
    unsafe {
        libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev);
    }
}

fn epoll_del(epoll_fd: RawFd, fd: RawFd) {
    unsafe {
        libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut());
    }
}

/// Reply data: stack buffer for fixed-size replies, heap only for VARARG.
pub enum Reply {
    /// Fixed-size reply on the stack. Max 64 bytes covers all reply structs.
    Fixed { buf: [u8; 64], len: usize },
    /// Variable-length reply (VARARG) — registry ops, startup info.
    Vararg(Vec<u8>),
    /// Deferred reply (select with timeout).
    Deferred,
}

fn reply_fixed<T>(reply: &T) -> Reply {
    let size = std::mem::size_of::<T>();
    debug_assert!(size <= 64, "reply struct exceeds 64-byte stack buffer");
    let mut buf = [0u8; 64];
    unsafe {
        std::ptr::copy_nonoverlapping(
            reply as *const T as *const u8,
            buf.as_mut_ptr(),
            size,
        );
    }
    Reply::Fixed { buf, len: size }
}

// Serialize a fixed reply struct + variable-length data (VARARG).
// The caller must set header.reply_size = vararg.len() before calling.
fn reply_vararg<T>(reply: &T, vararg: &[u8]) -> Reply {
    let fixed_size = std::mem::size_of::<T>();
    let mut out = Vec::with_capacity(fixed_size + vararg.len());
    out.extend_from_slice(unsafe {
        std::slice::from_raw_parts(reply as *const T as *const u8, fixed_size)
    });
    out.extend_from_slice(vararg);
    Reply::Vararg(out)
}

// Read the client's max accepted VARARG reply size from the request header.
// RequestHeader layout: req (i32) + request_size (u32) + reply_size (u32)
fn max_reply_vararg(buf: &[u8]) -> u32 {
    if buf.len() >= 12 {
        u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]])
    } else {
        0
    }
}
