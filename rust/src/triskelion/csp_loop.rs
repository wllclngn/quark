// CSP event loop — Communicating Sequential Processes architecture
//
// Two threads, communicating exclusively via lock-free channels:
//
//   I/O Thread (owns all file descriptors, epoll)
//     - epoll_wait for ready fds
//     - read requests from client pipes
//     - recv SCM_RIGHTS fds from msg_fd sockets
//     - send parsed requests to Authority via flume channel
//     - receive replies + effects from Authority, write them to clients
//     - execute effects: register new client fds, close dead ones, send fds
//
//   Authority Thread (owns EventLoop, all mutable state)
//     - drain request channel
//     - dispatch each request via ev.dispatch()
//     - push replies + effects back via reply channel
//     - run housekeeping ticks
//     - NEVER touches a file descriptor. NEVER blocks on I/O.
//
// ntsync wait threads (unchanged):
//     - blocking ioctl on /dev/ntsync
//     - write WakeUpReply directly to dup'd wait_fd (bypasses both threads)
//
// This architecture eliminates:
//   - The msg_fd race (I/O thread drains fds synchronously before forwarding)
//   - The secondary thread init bug (I/O thread registers pipes immediately)
//   - All shared mutable state between threads (CSP: share by communicating)

use std::os::unix::io::RawFd;
use std::collections::VecDeque;
use std::time::Instant;
use rustc_hash::FxHashMap;

use crate::event_loop::{EventLoop, Reply};
use crate::ipc::Client;
use crate::protocol::*;
use crate::SHUTDOWN;

// I/O thread → Authority: parsed request ready for dispatch
struct RequestMsg {
    client_fd: RawFd,
    header: RequestHeader,
    buf: Vec<u8>,
    // Inflight fds extracted from SCM_RIGHTS, to deposit before dispatch
    inflight_fds: Vec<(u32, i32, RawFd)>,
    // For registration (req=-1): msg_fd to store in Client
    msg_fd: RawFd,
}

// Authority → I/O thread: reply + side effects from handler
struct ReplyMsg {
    client_fd: RawFd,
    reply: SerializedReply,
    // Where to write the reply (reply_fd from init handshake, or None for request_fd)
    reply_fd: Option<RawFd>,
    // Effects the I/O thread must execute
    effects: Vec<Effect>,
}

// Serialized reply data (Reply enum can't cross thread boundary safely)
enum SerializedReply {
    Data(Vec<u8>),
}

// Side effects that handlers produce, executed by I/O thread
enum Effect {
    // Register a new client pipe (from new_thread/new_process handler)
    RegisterClient {
        request_fd: RawFd,
        msg_fd: RawFd,
        is_msg_primary: bool,
    },
    // Send an fd to a client via SCM_RIGHTS (pending_fd on reply)
    SendFd {
        target_fd: RawFd,
        fd_to_send: RawFd,
        protocol_version: u32,
    },
    // Monitor a pipe data fd for readability. When POLLIN fires,
    // signal the associated ntsync event to wake the client.
    WatchPipeFd {
        pipe_fd: RawFd,
        ntsync_fd: RawFd,
    },
    // Register a display driver fd (X11 connection) for polling.
    // When readable, sets QS_DRIVER in internal_bits to trigger ProcessEvents.
    WatchQueueFd {
        queue_fd: RawFd,
        shm_ptr: usize,   // pointer to queue shared_object_t base
        _queue_handle: u32,
        ntsync_event_fd: RawFd, // dup'd ntsync event fd to signal
    },
    // Re-arm polling on a queue fd after set_queue_mask(poll_events=1).
    RearmQueueFd {
        queue_fd: RawFd,
    },
}

// Per-client I/O state (owned exclusively by I/O thread)
struct IoClient {
    msg_fd: RawFd,
    recv_buf: Vec<u8>,
}

const MAX_EVENTS: usize = 64;
const TICK_MS: i32 = 50;

pub fn csp_main(
    ev: &mut EventLoop,
    listener_fd: RawFd,
    sigfd: RawFd,
    death_fd: RawFd,
) {
    // Channels: I/O thread ↔ Authority thread
    // Bounded to prevent runaway: if I/O produces faster than authority consumes,
    // backpressure naturally throttles accepts.
    let (req_tx, req_rx) = flume::bounded::<RequestMsg>(256);
    let (reply_tx, reply_rx) = flume::bounded::<ReplyMsg>(256);

    // Signal pipe: authority → I/O thread for shutdown
    let mut shutdown_pipe = [0i32; 2];
    unsafe { libc::pipe2(shutdown_pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK); }
    let shutdown_read = shutdown_pipe[0];
    let shutdown_write = shutdown_pipe[1];

    // Wakeup pipe: authority → I/O thread, "check reply channel"
    let mut wake_pipe = [0i32; 2];
    unsafe { libc::pipe2(wake_pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK); }
    let wake_read = wake_pipe[0];
    let wake_write = wake_pipe[1];

    let reply_tx_clone = reply_tx.clone();

    // Spawn I/O thread
    let io_handle = std::thread::Builder::new()
        .name("io".into())
        .spawn(move || {
            io_thread_main(listener_fd, sigfd, req_tx, reply_rx, shutdown_read, wake_read, death_fd);
        })
        .expect("failed to spawn I/O thread");

    // Authority runs on the main thread (owns EventLoop)
    authority_main(ev, req_rx, reply_tx_clone, shutdown_write, wake_write);

    // Wait for I/O thread to finish
    let _ = io_handle.join();

    unsafe {
        libc::close(shutdown_read);
        libc::close(shutdown_write);
        libc::close(wake_read);
        libc::close(wake_write);
    }
}

// Authority thread: owns EventLoop, dispatches requests, produces replies
fn authority_main(
    ev: &mut EventLoop,
    req_rx: flume::Receiver<RequestMsg>,
    reply_tx: flume::Sender<ReplyMsg>,
    shutdown_write: RawFd,
    wake_write: RawFd,
) {
    let wake_byte = [1u8];
    let mut tick_count: u64 = 0;
    // Track which client fds the I/O thread already knows about
    let mut known_io_fds = rustc_hash::FxHashSet::default();
    // Throttle housekeeping to TICK_MS intervals — without this, every request
    // burst causes check_win_timers to fire instantly, which restarts WM_TIMER
    // timers with their minimum rate (10ms) AFTER the just-completed iteration,
    // and the next housekeeping pass picks them up immediately. Result: ~30k
    // req/sec spin on get_message + WM_TIMER. Stock wineserver runs housekeeping
    // off poll() timeouts, not every request.
    let mut last_housekeep = Instant::now();
    let housekeep_interval = std::time::Duration::from_millis(TICK_MS as u64);

    loop {
        // Block waiting for a request (with timeout for housekeeping)
        match req_rx.recv_timeout(housekeep_interval) {
            Ok(msg) => {
                process_request(ev, msg, &reply_tx, wake_write, &wake_byte, &mut known_io_fds);

                // Drain any additional queued requests without blocking
                while let Ok(msg) = req_rx.try_recv() {
                    process_request(ev, msg, &reply_tx, wake_write, &wake_byte, &mut known_io_fds);
                }
            }
            Err(flume::RecvTimeoutError::Timeout) => {
                // Housekeeping tick
            }
            Err(flume::RecvTimeoutError::Disconnected) => {
                break;
            }
        }

        // Housekeeping — only run on TICK_MS interval, not every request iteration
        let now = Instant::now();
        if now.duration_since(last_housekeep) < housekeep_interval {
            continue;
        }
        last_housekeep = now;

        tick_count += 1;
        ev.idle_ticks = tick_count;
        ev.check_pending_waits();
        ev.check_pending_pipe_reads();
        ev.check_win_timers();
        ev.check_nt_timers();
        ev.update_usd_time();

        if tick_count % 200 == 0 && ev.total_requests > 0 {
            ev.dump_ntsync_state();
        }

        // Check linger (daemon exits when all clients gone + timeout)
        if let Some(deadline) = ev.linger_deadline {
            if Instant::now() >= deadline {
                log_info!("linger expired — shutting down");
                SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }

        if SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            // Tell I/O thread to stop
            unsafe { libc::write(shutdown_write, wake_byte.as_ptr() as *const _, 1); }
            break;
        }
    }
}

fn process_request(
    ev: &mut EventLoop,
    msg: RequestMsg,
    reply_tx: &flume::Sender<ReplyMsg>,
    wake_write: RawFd,
    wake_byte: &[u8],
    known_io_fds: &mut rustc_hash::FxHashSet<RawFd>,
) {
    let fd = msg.client_fd;

    // Special message: new client registration from I/O thread accept
    if msg.header.req == -1 {
        // I/O thread accepted a new connection and created pipe + msg_fd.
        // Register the client in EventLoop so handlers can find it.
        if !ev.clients.contains_key(&fd) {
            let c = Client::new(fd, msg.msg_fd);
            ev.clients.insert(fd, c);
            known_io_fds.insert(fd);
            ev.track_peak_clients();
        }
        return;
    }

    // Special message: client disconnected
    if msg.header.req == -2 {
        let pid = ev.clients.get(&fd).map(|c| c.process_id).unwrap_or(0);
        let tid = ev.clients.get(&fd).map(|c| c.thread_id).unwrap_or(0);
        log_info!("disconnect: fd={fd} pid={pid} tid={tid}");
        ev.disconnect_client(fd);
        known_io_fds.remove(&fd);

        // Check linger
        if ev.clients.is_empty() {
            if ev.linger_deadline.is_none() && ev.total_requests > 0 {
                let linger_secs = ev.linger_secs;
                ev.linger_deadline = Some(Instant::now() + std::time::Duration::from_secs(linger_secs));
                log_info!("disconnect: linger started ({linger_secs}s deadline)");
            }
        } else {
            ev.linger_deadline = None;
        }
        return;
    }

    // Deposit inflight fds into process pool before dispatch
    if !msg.inflight_fds.is_empty() {
        let pid = ev.clients.get(&fd).map(|c| c.process_id).unwrap_or(0);
        if pid != 0 {
            let pool = ev.process_inflight_fds.entry(pid).or_default();
            for (tid, fd_num, actual_fd) in msg.inflight_fds {
                pool.push_back((tid, fd_num, actual_fd));
            }
        } else if let Some(client) = ev.clients.get_mut(&fd) {
            for (tid, fd_num, actual_fd) in msg.inflight_fds {
                client.inflight_fds.push_back((tid, fd_num, actual_fd));
            }
        }
    }

    // Snapshot client count to detect new clients from handlers
    let clients_before = ev.clients.len();

    // Dispatch
    let reply = ev.dispatch(fd, &msg.header, &msg.buf);

    // Collect effects
    let mut effects = Vec::new();

    // Detect new clients created by handlers (new_process, new_thread)
    if ev.clients.len() > clients_before {
        for (&cfd, client) in ev.clients.iter() {
            if !client.is_phantom && !known_io_fds.contains(&cfd) {
                effects.push(Effect::RegisterClient {
                    request_fd: cfd,
                    msg_fd: client.msg_fd,
                    is_msg_primary: client.is_msg_primary,
                });
                known_io_fds.insert(cfd);
            }
        }
    }

    // Check for pending_fd (fd to send with reply via SCM_RIGHTS)
    // MUST send on msg_fd (Unix domain socket), NOT reply_fd (pipe).
    // SCM_RIGHTS only works on sockets.
    if let Some((pending_fd, version)) = ev.clients.get_mut(&fd).and_then(|c| c.pending_fd.take()) {
        let msg_target = ev.clients.get(&fd).map(|c| c.msg_fd).unwrap_or(-1);
        if msg_target >= 0 {
            effects.push(Effect::SendFd {
                target_fd: msg_target,
                fd_to_send: pending_fd,
                protocol_version: version,
            });
        } else {
            log_error!("pending_fd: no msg_fd for client fd={fd}");
            unsafe { libc::close(pending_fd); }
        }
    }

    // Drain pending pipe watch requests from handlers
    for (pipe_fd, ntsync_fd) in ev.pending_pipe_watches.drain(..) {
        effects.push(Effect::WatchPipeFd { pipe_fd, ntsync_fd });
    }

    // Drain pending queue fd watch/rearm requests
    for (queue_fd, shm_ptr, queue_handle, ntsync_event_fd) in ev.pending_queue_fd_watches.drain(..) {
        effects.push(Effect::WatchQueueFd { queue_fd, shm_ptr, _queue_handle: queue_handle, ntsync_event_fd });
    }
    for queue_fd in ev.pending_queue_fd_rearms.drain(..) {
        effects.push(Effect::RearmQueueFd { queue_fd });
    }

    // Serialize reply
    let serialized = match &reply {
        Reply::Fixed { buf, len } => SerializedReply::Data(buf[..*len].to_vec()),
        Reply::Vararg(v) => SerializedReply::Data(v.clone()),
    };

    // Get reply_fd from the client (set during init_first_thread/init_thread)
    let reply_fd = ev.clients.get(&fd).and_then(|c| c.reply_fd);

    let reply_msg = ReplyMsg {
        client_fd: fd,
        reply: serialized,
        reply_fd,
        effects,
    };

    let _ = reply_tx.send(reply_msg);
    // Wake I/O thread to process the reply
    unsafe { libc::write(wake_write, wake_byte.as_ptr() as *const _, 1); }
}

// I/O thread: owns all fds, epoll, reads/writes, executes effects
fn io_thread_main(
    listener_fd: RawFd,
    sigfd: RawFd,
    req_tx: flume::Sender<RequestMsg>,
    reply_rx: flume::Receiver<ReplyMsg>,
    shutdown_read: RawFd,
    wake_read: RawFd,
    death_fd: RawFd,
) {
    let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    assert!(epoll_fd >= 0, "epoll_create1 failed");

    // Register static fds
    epoll_add(epoll_fd, listener_fd, libc::EPOLLIN as u32);
    epoll_add(epoll_fd, sigfd, libc::EPOLLIN as u32);
    epoll_add(epoll_fd, shutdown_read, libc::EPOLLIN as u32);
    epoll_add(epoll_fd, wake_read, libc::EPOLLIN as u32);
    // Death pipe: POLLHUP when launcher exits (μEmacs ext_runner.c pattern)
    if death_fd >= 0 {
        epoll_add(epoll_fd, death_fd, libc::EPOLLIN as u32);
    }

    // Per-client I/O state
    let mut io_clients: FxHashMap<RawFd, IoClient> = FxHashMap::default();
    // msg_fd → list of request_fds sharing it (one msg_fd per process)
    let mut msg_fd_clients: FxHashMap<RawFd, Vec<RawFd>> = FxHashMap::default();
    // Pending inflight fds per process msg_fd, to attach to next request from any thread in process
    let mut pending_fds: FxHashMap<RawFd, VecDeque<(u32, i32, RawFd)>> = FxHashMap::default();
    // Track which reply_fd/wait_fd belong to which client (set by authority via init response)
    let mut client_reply_fds: FxHashMap<RawFd, RawFd> = FxHashMap::default();
    // Pipe data fd monitoring: pipe_data_fd → dup'd ntsync event fd.
    // When POLLIN fires on a pipe data fd, signal the ntsync event to wake the client.
    let mut pipe_watchers: FxHashMap<RawFd, RawFd> = FxHashMap::default();
    // Queue fd monitoring: X11 display fd → (shm_ptr, ntsync_event_fd).
    // When readable, sets QS_DRIVER (0x80000000) in internal_bits.
    let mut queue_fd_watchers: FxHashMap<RawFd, (usize, RawFd)> = FxHashMap::default();

    let mut events: Vec<libc::epoll_event> = vec![unsafe { std::mem::zeroed() }; MAX_EVENTS];

    log_info!("I/O thread started");

    loop {
        let n = unsafe {
            libc::epoll_wait(epoll_fd, events.as_mut_ptr(), MAX_EVENTS as i32, TICK_MS)
        };

        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            log_error!("epoll_wait failed: {e}");
            break;
        }

        for i in 0..n as usize {
            let fd = events[i].u64 as RawFd;

            if fd == listener_fd {
                // Accept new connection
                handle_accept(epoll_fd, listener_fd, &req_tx, &mut io_clients, &mut msg_fd_clients, &mut pending_fds);
            } else if fd == sigfd {
                // Signal received — handle all signals as shutdown triggers.
                // μEmacs pattern: handler sets flag only, real work in main loop.
                let mut buf = [0u8; 128];
                let r = unsafe { libc::read(sigfd, buf.as_mut_ptr() as *mut _, 128) };
                if r > 0 {
                    let info = unsafe { &*(buf.as_ptr() as *const libc::signalfd_siginfo) };
                    let sig = info.ssi_signo as i32;
                    log_info!("received signal {} — initiating shutdown", sig);
                    SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            } else if fd == shutdown_read {
                // Authority told us to stop
                let mut buf = [0u8; 1];
                unsafe { libc::read(shutdown_read, buf.as_mut_ptr() as *mut _, 1); }
                break;
            } else if fd == wake_read {
                // Authority has replies ready — drain the wake pipe and process below
                let mut buf = [0u8; 64];
                while unsafe { libc::read(wake_read, buf.as_mut_ptr() as *mut _, 64) } > 0 {}
            } else if death_fd >= 0 && fd == death_fd {
                // Death pipe: launcher exited. POLLHUP means write end closed.
                // μEmacs pattern: set running flag to false, break loop.
                log_info!("death pipe: launcher exited — initiating shutdown");
                SHUTDOWN.store(true, std::sync::atomic::Ordering::Relaxed);
            } else if msg_fd_clients.contains_key(&fd) {
                // msg_fd ready — drain SCM_RIGHTS fds
                drain_msg_fd(fd, &msg_fd_clients, &mut pending_fds);
            } else if io_clients.contains_key(&fd) {
                // Client request_fd ready — read request
                handle_client_read(fd, epoll_fd, &req_tx, &mut io_clients, &mut msg_fd_clients, &mut pending_fds, &mut client_reply_fds);
            } else if let Some(&(shm_ptr, ntsync_fd)) = queue_fd_watchers.get(&fd) {
                // X11 display fd readable — set QS_DRIVER in internal_bits
                const QS_DRIVER: u32 = 0x80000000;
                unsafe {
                    let base = shm_ptr as *mut u8;
                    let seq_atomic = &*(base as *const std::sync::atomic::AtomicI64);
                    let shm = base.add(16); // skip shared_object_t header (seq + id)
                    // internal_bits is at offset 24 in queue_shm_t
                    let internal_bits_ptr = shm.add(24) as *mut u32;
                    let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                    seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                    *internal_bits_ptr |= QS_DRIVER;
                    seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                }
                // Signal ntsync event to wake MsgWaitForMultipleObjects
                if ntsync_fd >= 0 {
                    let mut prev: u32 = 0;
                    unsafe { libc::ioctl(ntsync_fd, 0x80044E88u64, &mut prev as *mut u32); }
                }
                // Disable fd until set_queue_mask re-arms it (EPOLLONESHOT)
            } else if let Some(&ntsync_fd) = pipe_watchers.get(&fd) {
                // Pipe data fd has data — signal the ntsync event to wake the reader.
                // This is the core pipe monitoring: when Process A writes to its pipe fd,
                // data arrives on Process B's pipe fd, and we signal B's ntsync event.
                let mut prev: u32 = 0;
                unsafe { libc::ioctl(ntsync_fd, 0x80044E88u64, &mut prev as *mut u32); } // NTSYNC_IOC_EVENT_SET
            }
        }

        // Process all pending replies from authority
        while let Ok(reply_msg) = reply_rx.try_recv() {
            execute_reply(epoll_fd, reply_msg, &mut io_clients, &mut msg_fd_clients, &mut pending_fds, &mut client_reply_fds, &mut pipe_watchers, &mut queue_fd_watchers);
        }

        if SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
    }

    // Cleanup: close all owned fds
    for (fd, _) in &io_clients {
        unsafe { libc::close(*fd); }
    }
    for (fd, _) in &msg_fd_clients {
        unsafe { libc::close(*fd); }
    }
    for (fd, _) in &pipe_watchers {
        unsafe { libc::close(*fd); }
    }
    for (fd, (_, ntsync_fd)) in &queue_fd_watchers {
        unsafe { libc::close(*fd); }
        if *ntsync_fd >= 0 {
            unsafe { libc::close(*ntsync_fd); }
        }
    }
    unsafe { libc::close(epoll_fd); }
    log_info!("I/O thread exiting");
}

fn handle_accept(
    epoll_fd: RawFd,
    listener_fd: RawFd,
    req_tx: &flume::Sender<RequestMsg>,
    io_clients: &mut FxHashMap<RawFd, IoClient>,
    msg_fd_clients: &mut FxHashMap<RawFd, Vec<RawFd>>,
    _pending_fds: &mut FxHashMap<RawFd, VecDeque<(u32, i32, RawFd)>>,
) {
    // Match ipc.rs Listener::accept() exactly:
    // 1. accept → get connection socket (msg_fd)
    // 2. set msg_fd non-blocking
    // 3. pipe2 with O_CLOEXEC only (NO O_NONBLOCK — Wine expects blocking pipe)
    // 4. send_fd on msg_fd (handshake)
    // 5. close pipe write-end
    // 6. dup pipe read-end, close original
    // 7. set request_fd non-blocking

    let client_fd = unsafe { libc::accept4(listener_fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
    if client_fd < 0 { return; }

    let msg_fd = client_fd;

    // Set msg_fd non-blocking BEFORE send_fd (matches ipc.rs: stream.set_nonblocking(true))
    unsafe {
        let flags = libc::fcntl(msg_fd, libc::F_GETFL);
        libc::fcntl(msg_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Pipe: O_CLOEXEC only, NO O_NONBLOCK (matches ipc.rs)
    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        log_error!("accept: pipe2 failed");
        unsafe { libc::close(msg_fd); }
        return;
    }

    // Send pipe write-end + protocol version to client (Wine handshake)
    let version = crate::ipc::runtime_protocol_version();
    let n = crate::ipc::send_fd(msg_fd, pipe_fds[1], version);
    if n < 0 {
        log_error!("accept: send_fd failed: {}", std::io::Error::last_os_error());
        unsafe { libc::close(msg_fd); libc::close(pipe_fds[0]); libc::close(pipe_fds[1]); }
        return;
    }
    unsafe { libc::close(pipe_fds[1]); }

    // Dup pipe read-end to avoid fd reuse race (matches ipc.rs)
    let safe_fd = unsafe { libc::dup(pipe_fds[0]) };
    unsafe { libc::close(pipe_fds[0]); }
    if safe_fd < 0 {
        log_error!("accept: dup request_fd failed");
        unsafe { libc::close(msg_fd); }
        return;
    }
    let request_fd = safe_fd;

    // Set request_fd non-blocking (matches ipc.rs)
    unsafe {
        let flags = libc::fcntl(request_fd, libc::F_GETFL);
        libc::fcntl(request_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Register with epoll
    epoll_add(epoll_fd, request_fd, libc::EPOLLIN as u32);
    epoll_add(epoll_fd, msg_fd, libc::EPOLLIN as u32);

    // Track I/O state
    io_clients.insert(request_fd, IoClient {
        msg_fd,
        recv_buf: Vec::with_capacity(256),
    });
    msg_fd_clients.entry(msg_fd).or_default().push(request_fd);

    // Tell authority about new client
    let _ = req_tx.send(RequestMsg {
        client_fd: request_fd,
        header: RequestHeader { req: -1, request_size: 0, reply_size: 0 },
        buf: Vec::new(),
        inflight_fds: Vec::new(),
        msg_fd,
    });

    log_info!("accept: request_fd={request_fd} msg_fd={msg_fd} sent={n}");
}

fn drain_msg_fd(
    msg_fd: RawFd,
    _msg_fd_clients: &FxHashMap<RawFd, Vec<RawFd>>,
    pending_fds: &mut FxHashMap<RawFd, VecDeque<(u32, i32, RawFd)>>,
) {
    loop {
        let mut data_buf = [0u8; 64];
        let mut cmsg_buf = [0u8; 256];
        let mut iov = libc::iovec {
            iov_base: data_buf.as_mut_ptr() as *mut _,
            iov_len: data_buf.len(),
        };
        let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
        hdr.msg_iov = &mut iov;
        hdr.msg_iovlen = 1;
        hdr.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
        hdr.msg_controllen = cmsg_buf.len();

        let n = unsafe { libc::recvmsg(msg_fd, &mut hdr, libc::MSG_DONTWAIT) };
        if n <= 0 { break; }

        // Parse SCM_RIGHTS
        let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&hdr) };
        while !cmsg.is_null() {
            if unsafe { (*cmsg).cmsg_level } == libc::SOL_SOCKET
                && unsafe { (*cmsg).cmsg_type } == libc::SCM_RIGHTS
            {
                let data_ptr = unsafe { libc::CMSG_DATA(cmsg) };
                let data_len = unsafe { (*cmsg).cmsg_len } as usize
                    - (data_ptr as usize - cmsg as usize);
                let fd_count = data_len / std::mem::size_of::<i32>();
                for i in 0..fd_count {
                    let received_fd = unsafe { *(data_ptr as *const i32).add(i) };
                    let (tid, fd_num) = if n >= 8 {
                        let t = u32::from_le_bytes([data_buf[0], data_buf[1], data_buf[2], data_buf[3]]);
                        let f = i32::from_le_bytes([data_buf[4], data_buf[5], data_buf[6], data_buf[7]]);
                        (t, f)
                    } else {
                        (0, received_fd)
                    };
                    pending_fds.entry(msg_fd).or_default().push_back((tid, fd_num, received_fd));
                }
            }
            cmsg = unsafe { libc::CMSG_NXTHDR(&hdr, cmsg) };
        }
    }
}

fn handle_client_read(
    fd: RawFd,
    epoll_fd: RawFd,
    req_tx: &flume::Sender<RequestMsg>,
    io_clients: &mut FxHashMap<RawFd, IoClient>,
    msg_fd_clients: &mut FxHashMap<RawFd, Vec<RawFd>>,
    pending_fds: &mut FxHashMap<RawFd, VecDeque<(u32, i32, RawFd)>>,
    client_reply_fds: &mut FxHashMap<RawFd, RawFd>,
) {
    let mut read_buf = [0u8; 4096];
    let n = unsafe { libc::read(fd, read_buf.as_mut_ptr() as *mut _, read_buf.len()) };

    if n <= 0 {
        // EOF — client disconnected
        disconnect_io_client(fd, epoll_fd, io_clients, msg_fd_clients, pending_fds, client_reply_fds);

        // Tell authority about disconnect
        let _ = req_tx.send(RequestMsg {
            client_fd: fd,
            header: RequestHeader { req: -2, request_size: 0, reply_size: 0 },
            buf: Vec::new(),
            inflight_fds: Vec::new(),
            msg_fd: -1,
        });
        return;
    }

    let io_client = match io_clients.get_mut(&fd) {
        Some(c) => c,
        None => return,
    };

    io_client.recv_buf.extend_from_slice(&read_buf[..n as usize]);

    // Process complete requests
    loop {
        if io_client.recv_buf.len() < std::mem::size_of::<RequestHeader>() {
            break;
        }

        let header: RequestHeader = unsafe {
            std::ptr::read_unaligned(io_client.recv_buf.as_ptr() as *const RequestHeader)
        };

        // Wine request: the fixed part is padded to 64 bytes (sizeof generic_request).
        // request_size is the VARARG size, NOT total size. Total = 64 + request_size.
        let total_size = 64 + header.request_size as usize;
        if io_client.recv_buf.len() < total_size {
            break; // incomplete, wait for more data
        }

        let buf: Vec<u8> = io_client.recv_buf.drain(..total_size).collect();

        // Before forwarding: drain msg_fd for any pending SCM_RIGHTS fds
        let msg_fd = io_client.msg_fd;
        drain_msg_fd(msg_fd, &FxHashMap::default(), pending_fds);

        // Collect inflight fds to send with this request
        let inflight = pending_fds.get_mut(&msg_fd)
            .map(|q| q.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();

        let _ = req_tx.send(RequestMsg {
            client_fd: fd,
            header,
            buf,
            inflight_fds: inflight,
            msg_fd: -1,
        });
    }
}

fn disconnect_io_client(
    fd: RawFd,
    epoll_fd: RawFd,
    io_clients: &mut FxHashMap<RawFd, IoClient>,
    msg_fd_clients: &mut FxHashMap<RawFd, Vec<RawFd>>,
    pending_fds: &mut FxHashMap<RawFd, VecDeque<(u32, i32, RawFd)>>,
    client_reply_fds: &mut FxHashMap<RawFd, RawFd>,
) {
    epoll_del(epoll_fd, fd);

    if let Some(io_client) = io_clients.remove(&fd) {
        // Remove from msg_fd tracking
        if let Some(clients) = msg_fd_clients.get_mut(&io_client.msg_fd) {
            clients.retain(|&f| f != fd);
            if clients.is_empty() {
                msg_fd_clients.remove(&io_client.msg_fd);
                pending_fds.remove(&io_client.msg_fd);
                epoll_del(epoll_fd, io_client.msg_fd);
                unsafe { libc::close(io_client.msg_fd); }
            }
        }
    }
    client_reply_fds.remove(&fd);
    unsafe { libc::close(fd); }
    log_info!("disconnect_io: fd={fd}");
}

fn execute_reply(
    epoll_fd: RawFd,
    reply_msg: ReplyMsg,
    io_clients: &mut FxHashMap<RawFd, IoClient>,
    msg_fd_clients: &mut FxHashMap<RawFd, Vec<RawFd>>,
    _pending_fds: &mut FxHashMap<RawFd, VecDeque<(u32, i32, RawFd)>>,
    client_reply_fds: &mut FxHashMap<RawFd, RawFd>,
    pipe_watchers: &mut FxHashMap<RawFd, RawFd>,
    queue_fd_watchers: &mut FxHashMap<RawFd, (usize, RawFd)>,
) {
    let fd = reply_msg.client_fd;

    // Execute effects first
    for effect in reply_msg.effects {
        match effect {
            Effect::RegisterClient { request_fd, msg_fd, is_msg_primary } => {
                if io_clients.contains_key(&request_fd) {
                    continue; // already registered
                }
                // Set non-blocking
                unsafe {
                    let flags = libc::fcntl(request_fd, libc::F_GETFL);
                    if flags >= 0 {
                        libc::fcntl(request_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                }
                epoll_add(epoll_fd, request_fd, libc::EPOLLIN as u32);
                io_clients.insert(request_fd, IoClient {
                    msg_fd,
                    recv_buf: Vec::with_capacity(256),
                });
                if is_msg_primary && !msg_fd_clients.contains_key(&msg_fd) {
                    // Set msg_fd non-blocking and register
                    unsafe {
                        let flags = libc::fcntl(msg_fd, libc::F_GETFL);
                        if flags >= 0 {
                            libc::fcntl(msg_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                        }
                    }
                    epoll_add(epoll_fd, msg_fd, libc::EPOLLIN as u32);
                }
                msg_fd_clients.entry(msg_fd).or_default().push(request_fd);
                // Probe for already-buffered data (secondary thread may have written before epoll_add)
                let probe_n = unsafe { libc::recv(request_fd, [0u8; 4].as_mut_ptr() as *mut _, 4, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
                if probe_n > 0 {
                    log_info!("effect: registered client fd={request_fd} msg_fd={msg_fd} primary={is_msg_primary} (buffered={probe_n}b)");
                } else {
                    log_info!("effect: registered client fd={request_fd} msg_fd={msg_fd} primary={is_msg_primary}");
                }
            }
            Effect::SendFd { target_fd, fd_to_send, protocol_version } => {
                crate::ipc::send_fd(target_fd, fd_to_send, protocol_version);
                unsafe { libc::close(fd_to_send); }
            }
            Effect::WatchPipeFd { pipe_fd, ntsync_fd } => {
                if !pipe_watchers.contains_key(&pipe_fd) {
                    epoll_add(epoll_fd, pipe_fd, libc::EPOLLIN as u32);
                    pipe_watchers.insert(pipe_fd, ntsync_fd);
                    log_info!("pipe_watch: monitoring fd={pipe_fd} ntsync_fd={ntsync_fd}");
                } else {
                    // Update the ntsync fd if already watched
                    let old = pipe_watchers.insert(pipe_fd, ntsync_fd);
                    if let Some(old_fd) = old {
                        if old_fd != ntsync_fd {
                            unsafe { libc::close(old_fd); }
                        }
                    }
                }
            }
            Effect::WatchQueueFd { queue_fd, shm_ptr, _queue_handle: _, ntsync_event_fd } => {
                if !queue_fd_watchers.contains_key(&queue_fd) {
                    let mut ev = libc::epoll_event {
                        events: libc::EPOLLIN as u32 | libc::EPOLLONESHOT as u32,
                        u64: queue_fd as u64,
                    };
                    unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, queue_fd, &mut ev); }
                    queue_fd_watchers.insert(queue_fd, (shm_ptr, ntsync_event_fd));
                    log_info!("queue_fd_watch: monitoring fd={queue_fd} for QS_DRIVER");
                }
            }
            Effect::RearmQueueFd { queue_fd } => {
                if queue_fd_watchers.contains_key(&queue_fd) {
                    // Clear QS_DRIVER from internal_bits
                    if let Some(&(shm_ptr, _)) = queue_fd_watchers.get(&queue_fd) {
                        const QS_DRIVER: u32 = 0x80000000;
                        unsafe {
                            let base = shm_ptr as *mut u8;
                            let seq_atomic = &*(base as *const std::sync::atomic::AtomicI64);
                            let shm = base.add(16);
                            let internal_bits_ptr = shm.add(24) as *mut u32;
                            let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                            seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                            *internal_bits_ptr &= !QS_DRIVER;
                            seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                        }
                    }
                    // Re-arm EPOLLONESHOT
                    let mut ev = libc::epoll_event {
                        events: libc::EPOLLIN as u32 | libc::EPOLLONESHOT as u32,
                        u64: queue_fd as u64,
                    };
                    unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_MOD, queue_fd, &mut ev); }
                }
            }
        }
    }

    // Update cached reply_fd for this client
    if let Some(rfd) = reply_msg.reply_fd {
        client_reply_fds.insert(fd, rfd);
    }

    // Write reply
    match reply_msg.reply {
        SerializedReply::Data(data) => {
            if data.is_empty() { return; }
            // Determine target fd: use reply_fd if available, else request_fd
            let target = client_reply_fds.get(&fd).copied().unwrap_or(fd);
            let mut written = 0usize;
            while written < data.len() {
                let n = unsafe {
                    libc::write(target, data[written..].as_ptr() as *const _, data.len() - written)
                };
                if n <= 0 {
                    let e = std::io::Error::last_os_error();
                    log_error!("reply write failed: fd={fd} target={target} err={e} written={written}/{}", data.len());
                    break;
                }
                written += n as usize;
            }
            log_info!("reply: fd={fd} target={target} len={} written={written}", data.len());
        }
    }
}

fn epoll_add(epoll_fd: RawFd, fd: RawFd, events: u32) {
    let mut ev = libc::epoll_event { events, u64: fd as u64 };
    let rc = unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
    if rc < 0 {
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if e != libc::EEXIST {
            log_warn!("epoll_add FAILED: fd={fd} errno={e}");
        }
    }
}

fn epoll_del(epoll_fd: RawFd, fd: RawFd) {
    unsafe { libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut()); }
}
