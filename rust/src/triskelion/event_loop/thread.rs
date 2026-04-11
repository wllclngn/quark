// Thread lifecycle handlers

use super::*;
#[allow(unused_variables)]

impl EventLoop {

    /// Take an inflight fd by number from the process-wide pool or client queue.
    /// Workers drain msg_fd before each request and send fds via BrokerMsg.
    /// The broker puts them in process_inflight_fds before dispatching.
    /// Matches by fd_number. Thread_id is stored for diagnostics but matching
    /// by fd_number alone is sufficient (same as stock wineserver's FIFO lookup).
    pub(crate) fn take_inflight_fd(&mut self, client_fd: RawFd, fd_num: i32) -> Option<RawFd> {
        // 1. Check client's own inflight queue (pre-init, before process_id is set)
        if let Some(fd) = self.clients.get_mut(&client_fd)
            .and_then(|c| c.take_inflight_fd_by_number(fd_num))
        {
            return Some(fd);
        }

        // 2. Check process-wide pool (match by fd_number)
        let pid = self.clients.get(&client_fd).map(|c| c.process_id).unwrap_or(0);
        if pid != 0 {
            if let Some(pool) = self.process_inflight_fds.get_mut(&pid) {
                if let Some(pos) = pool.iter().position(|(_, n, _)| *n == fd_num) {
                    return pool.remove(pos).map(|(_, _, fd)| fd);
                }
            }
        }

        None
    }

    pub(crate) fn handle_new_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<NewThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const NewThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Wine's new_thread protocol:
        // 1. Client creates pipe, sends pipe[0] (read end) to server via SCM_RIGHTS
        // 2. Server receives pipe[0], uses it as the new thread's request_fd
        // 3. New thread writes requests on pipe[1]
        // 4. New thread later calls init_thread, sending reply_fd + wait_fd on process's msg_fd
        //
        // Race: SCM_RIGHTS on msg_fd and the request on request_fd go through
        // different kernel paths. The request can arrive before the fd.
        // Retry the drain a few times with short sleeps to handle this.
        let pid_for_debug = self.clients.get(&(client_fd as RawFd))
            .map(|c| c.process_id).unwrap_or(0);
        let pool_debug: Vec<(u32, i32)> = self.process_inflight_fds.get(&pid_for_debug)
            .map(|p| p.iter().map(|(tid, n, _)| (*tid, *n)).collect())
            .unwrap_or_default();
        log_info!("new_thread: process={:#x} access={:#x} flags={:#x} request_fd={} pid={pid_for_debug} buf_len={}",
            req.process, req.access, req.flags, req.request_fd, buf.len());
        let inflight_fd = self.take_inflight_fd(client_fd as RawFd, req.request_fd);
        if inflight_fd.is_none() && req.request_fd >= 0 {
            log_warn!("new_thread: inflight_fd MISS for req.request_fd={} pid={pid_for_debug} pool={pool_debug:?}",
                req.request_fd);
        }

        // Resolve target process from handle
        let caller_pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let target_pid = caller_pid.and_then(|ppid| {
            self.state.processes.get(&ppid)
                .and_then(|p| p.handles.get(req.process))
                .map(|h| h.object_id as process_id_t)
        }).unwrap_or_else(|| {
            // Fallback: use caller's process
            caller_pid.unwrap_or_else(|| {
                self.state.processes.keys().next().copied().unwrap_or(0)
            })
        });

        let tid = self.state.create_thread(target_pid);
        // CREATE_SUSPENDED: track so init_thread returns suspend=1
        if req.flags & 0x01 != 0 {
            if let Some(thread) = self.state.threads.get_mut(&tid) {
                thread.suspend_count = 1;
            }
        }
        // Thread handles are waitable (WaitForSingleObject)
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        // Store tid in handle so resume_thread can find the target thread
        if handle != 0 {
            let cpid = self.client_pid(client_fd as RawFd);
            if let Some(p) = self.state.processes.get_mut(&cpid) {
                if let Some(e) = p.handles.get_mut(handle) { e.object_id = tid as u64; }
            }
        }

        // Register ntsync event for thread handle so Select/WaitForSingleObject works.
        // Arc clone keeps fd alive even if creator closes its handle.
        let creator_pid = self.client_pid(client_fd as RawFd);
        let mut thread_exit_obj: Option<Arc<crate::ntsync::NtsyncObj>> = None;
        if handle != 0 {
            if let Some(obj) = self.get_or_create_event(true, false) {
                thread_exit_obj = Some(Arc::clone(&obj));
                self.insert_recyclable_event(creator_pid, handle, obj, 1); // INTERNAL
            }
        }

        let mut new_thread_fd: Option<RawFd> = None;

        // Set up the new thread's connection.
        // The inflight fd IS the pipe read end — the server reads requests from it.
        // The new thread's msg_fd is shared with the process (creating thread's msg_fd).
        if let Some(pipe_read_fd) = inflight_fd {
            // Dup the pipe fd to a fresh fd number. This prevents a race where
            // a zombie worker thread (from a previous client that used this fd
            // number) is still polling on it. The dup'd fd is guaranteed unique —
            // no other worker is watching it.
            let safe_fd = unsafe { libc::dup(pipe_read_fd) };
            unsafe { libc::close(pipe_read_fd); }
            if safe_fd < 0 {
                log_error!("new_thread: dup failed for pipe_read_fd={pipe_read_fd}");
            } else {
                // Set non-blocking
                unsafe {
                    let flags = libc::fcntl(safe_fd, libc::F_GETFL);
                    libc::fcntl(safe_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }

                // Create new client. msg_fd is shared for request-level fd draining.
                // is_msg_primary=false so the poll loop doesn't compete with parent.
                let parent_msg_fd = self.clients.get(&(client_fd as RawFd))
                    .map(|c| c.msg_fd).unwrap_or(-1);
                let mut c = crate::ipc::Client::new(safe_fd, parent_msg_fd);
                c.is_msg_primary = false;
                c.process_id = target_pid;
                c.thread_id = tid; // Store tid so init_thread doesn't double-create
                self.clients.insert(safe_fd, c);
                if self.clients.len() > self.peak_clients {
                    self.peak_clients = self.clients.len();
                }
                log_info!("new_thread: created client fd={safe_fd} tid={tid} pid={target_pid} msg_fd={parent_msg_fd}");

                new_thread_fd = Some(safe_fd);
            }
        } else if req.request_fd == -1 {
            // fd == -1: cross-process thread creation (initial thread of child process).
            // Wine's server sends pipe write end via send_client_fd(process, ...).
            // BUT: setup_client_on_socket (in new_process) already created and sent
            // a request pipe to the child. Sending a second pipe would put an extra
            // fd in the child's fd_socket queue, causing assertion failures when the
            // child calls get_handle_fd (receives wrong fd).
            //
            // Check if the target process already has a client connection. If so,
            // just register the thread with the existing connection — no second pipe.
            let existing_client_fd = self.clients.iter()
                .find(|(_, c)| c.process_id == target_pid)
                .map(|(&fd, _)| fd);

            if let Some(ecfd) = existing_client_fd {
                // Target process already has a connection from setup_client_on_socket.
                // No need to create another pipe. Store the tid so init_first_thread
                // reuses it instead of creating a duplicate.
                new_thread_fd = Some(ecfd);
                if let Some(c) = self.clients.get_mut(&ecfd) {
                    c.thread_id = tid;
                }
            } else {
                // No existing connection — create pipe and send to target process.
                let target_msg_fd = self.state.processes.get(&target_pid)
                    .and_then(|p| p.socket_fd)
                    .or_else(|| {
                        self.clients.values()
                            .find(|c| c.process_id == target_pid)
                            .map(|c| c.msg_fd)
                    })
                    .unwrap_or(-1);
                let mut request_pipe = [0i32; 2];
                if target_msg_fd >= 0 && unsafe { libc::pipe2(request_pipe.as_mut_ptr(), libc::O_CLOEXEC) } == 0 {
                    crate::ipc::send_fd(target_msg_fd, request_pipe[1], crate::ipc::runtime_protocol_version());
                    unsafe { libc::close(request_pipe[1]); }

                    unsafe {
                        let flags = libc::fcntl(request_pipe[0], libc::F_GETFL);
                        libc::fcntl(request_pipe[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                    epoll_add(self.epoll_fd, request_pipe[0], libc::EPOLLIN as u32);

                    let mut c = crate::ipc::Client::new(request_pipe[0], target_msg_fd);
                    c.process_id = target_pid;
                    self.clients.insert(request_pipe[0], c);

                    new_thread_fd = Some(request_pipe[0]);
                } else {
                    log_error!("new_thread: tid={tid} handle={handle} target_pid={target_pid} target_msg_fd={target_msg_fd} (pipe2 failed or no msg_fd)");
                }
            }
        } else {
            log_warn!("new_thread: UNHANDLED CASE req.request_fd={} inflight_fd={:?} target_pid={target_pid}",
                req.request_fd, inflight_fd);
        }

        // Register thread exit event keyed by the new thread's request fd
        if handle != 0 {
            if let Some(thread_fd) = new_thread_fd {
                if let Some(exit_obj) = thread_exit_obj {
                    self.thread_exit_events.entry(thread_fd).or_default().push((creator_pid, handle, exit_obj));
                } else {
                    log_warn!("new_thread: no exit_obj for tid={tid} handle={handle:#x} thread_fd={thread_fd}");
                }
            } else {
                log_warn!("new_thread: no thread_fd for tid={tid} handle={handle:#x} — exit event NOT registered!");
            }
        }


        let reply = NewThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            tid,
            handle,
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_init_first_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<InitFirstThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InitFirstThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Drain reply_fd and wait_fd from inflight fds (sent via SCM_RIGHTS on msg_fd).
        // Use retry drain — SCM_RIGHTS can arrive slightly after the request in
        // the concurrent model (different kernel paths for msg_fd vs request_fd).
        let reply_fd_val = self.take_inflight_fd(client_fd as RawFd, req.reply_fd)
            .or_else(|| {
                // Fallback: FIFO (for first connection where fd numbers may not match)
                self.clients.get_mut(&(client_fd as RawFd))
                    .and_then(|c| c.take_inflight_fd())
            });
        let wait_fd_val = self.take_inflight_fd(client_fd as RawFd, req.wait_fd)
            .or_else(|| {
                self.clients.get_mut(&(client_fd as RawFd))
                    .and_then(|c| c.take_inflight_fd())
            });
        if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
            if let Some(fd) = reply_fd_val {
                client.reply_fd = Some(fd);
                log_info!("init_first_thread: reply_fd={fd} for client_fd={client_fd}");
            } else {
                log_error!("init_first_thread: reply_fd NOT FOUND for client_fd={client_fd} inflight_count={}", client.inflight_fds.len());
            }
            if let Some(fd) = wait_fd_val {
                client.wait_fd = Some(fd);
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                let ok = unsafe { libc::fstat(fd, &mut stat) } == 0;
                log_info!("init_first_thread: wait_fd={fd} ino={} for client_fd={client_fd}",
                    if ok { stat.st_ino } else { 0 });
            } else {
                log_error!("init_first_thread: wait_fd NOT FOUND for client_fd={client_fd}");
            }
        }

        // Drain any pending wakes that were deferred because wait_fd was None.
        // These were queued by send_select_wake when an event fired before this
        // client had initialized.
        if let Some(wakes) = self.pending_wakes.remove(&(client_fd as RawFd)) {
            if let Some(client) = self.clients.get(&(client_fd as RawFd)) {
                if let Some(wait_fd) = client.wait_fd {
                    for (cookie, signaled) in wakes {
                        let reply = WakeUpReply { cookie, signaled, _pad: 0 };
                        unsafe {
                            libc::write(wait_fd, &reply as *const _ as *const _, 16);
                        }
                    }
                }
            }
        }

        // Find existing process (created by new_process) or create one.
        // Children should connect via WINESERVERSOCKET but due to fd 0 clobbering
        // they connect via the master socket instead. Match them to unclaimed
        // processes created by new_process (FIFO order).
        let mut is_unparented = false;
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .or_else(|| self.state.unclaimed_pids.pop_front())
            .unwrap_or_else(|| {
                is_unparented = true;
                let pid = self.state.create_process();
                self.state.processes.get_mut(&pid).unwrap().claimed = true;
                pid
            });

        // Mark the process as claimed and remove from unclaimed queue
        if let Some(process) = self.state.processes.get_mut(&pid) {
            process.claimed = true;
        }
        self.state.unclaimed_pids.retain(|&p| p != pid);

        // Reuse thread_id if new_thread already created one for this client
        // (cross-process case: parent called new_thread before child connects).
        // Without this, a duplicate tid is created and the first one is orphaned
        // in process.threads, blocking process exit detection.
        let existing_tid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.thread_id != 0 { Some(c.thread_id) } else { None });
        let tid = existing_tid.unwrap_or_else(|| self.state.create_thread(pid));

        // Allocate SHM queue slot keyed by Wine thread ID (not unix_tid).
        // Message delivery uses Wine tid for lookup, so the key must match.
        let slot = match self.shm.alloc_slot(tid as thread_id_t) {
            Some(s) => s,
            None => return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 }), // STATUS_NO_MEMORY
        };

        // Allocate per-thread queue and input shared objects in session memfd
        let queue_locator = self.alloc_shared_object();
        let input_locator = self.alloc_shared_object();

        // Write input_shm_t.foreground = 1 so the client's seqlock check
        // (try_get_shared_input: valid = !!object->shm.input.foreground)
        // doesn't spin infinitely invalidating the foreground input cache.
        let input_offset = u64::from_le_bytes(input_locator[8..16].try_into().unwrap());
        self.shared_write(input_offset, |shm| unsafe {
            *(shm as *mut i32) = 1; // foreground = 1 (first field of input_shm_t)
        });

        if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
            client.thread_id = tid;
            client.process_id = pid;
            client.queue_locator = queue_locator;
            client.input_locator = input_locator;
            client.unix_pid = req.unix_pid as i32;
            client.unix_tid = req.unix_tid as i32;
        }

        // Synthesize imagepath for processes without startup_info.
        // Children created via new_process get startup_info from the parent.
        // Skip the FIRST unparented process (boot process) — it needs empty
        // startup_info so info_size=0 triggers wineboot. After boot_process_claimed
        // is set, subsequent unparented processes get the game exe path.
        if let Some(process) = self.state.processes.get_mut(&pid) {
            let is_boot = is_unparented && !self.boot_process_claimed;
            if !is_boot
                && (process.startup_info.is_none() || process.startup_info.as_ref().map(|v| v.is_empty()).unwrap_or(true))
            {
                if let Ok(game_exe) = std::env::var("TRISKELION_GAME_EXE") {
                    let nt_path = format!("\\??\\Z:{}", game_exe.replace('/', "\\"));
                    let game_dir = if let Some(pos) = nt_path.rfind('\\') {
                        &nt_path[..pos]
                    } else {
                        &nt_path
                    };
                    let curdir_u16: Vec<u8> = game_dir.encode_utf16()
                        .flat_map(|c| c.to_le_bytes()).collect();
                    let imagepath_u16: Vec<u8> = nt_path.encode_utf16()
                        .flat_map(|c| c.to_le_bytes()).collect();
                    let cmdline_u16: Vec<u8> = nt_path.encode_utf16()
                        .flat_map(|c| c.to_le_bytes()).collect();
                    let struct_size = 96usize;
                    let total = struct_size + curdir_u16.len() + imagepath_u16.len() + cmdline_u16.len();
                    let mut si = vec![0u8; total];
                    si[64..68].copy_from_slice(&(curdir_u16.len() as u32).to_le_bytes());
                    si[72..76].copy_from_slice(&(imagepath_u16.len() as u32).to_le_bytes());
                    si[76..80].copy_from_slice(&(cmdline_u16.len() as u32).to_le_bytes());
                    let mut off = struct_size;
                    si[off..off + curdir_u16.len()].copy_from_slice(&curdir_u16);
                    off += curdir_u16.len();
                    si[off..off + imagepath_u16.len()].copy_from_slice(&imagepath_u16);
                    off += imagepath_u16.len();
                    si[off..off + cmdline_u16.len()].copy_from_slice(&cmdline_u16);
                    process.info_size = si.len() as u32;
                    process.startup_info = Some(si);
                }
            }
        }

        // init_first_thread.info_size = TOTAL data size (startup_info + env, no padding).
        // The client uses this to allocate the buffer for get_startup_info.
        // get_startup_info.reply.info_size = struct-only size (where env begins).
        // Stock: get_process_startup_info_size() returns data_size = info + env (no padding).
        let mut info_size = self.state.processes.get(&pid)
            .map(|p| {
                let si_len = p.startup_info.as_ref().map(|v| v.len()).unwrap_or(0);
                let env_len = p.startup_env.as_ref().map(|v| v.len()).unwrap_or(0);
                (si_len + env_len) as u32
            })
            .unwrap_or(0);

        // First unparented process: assign desktop ownership. info_size=0
        // triggers ntdll's build_initial_params() → run_wineboot() →
        // virtual_init_user_shared_data(). __wineboot_event is NOT
        // pre-signaled, so run_wineboot() creates it fresh, calls
        // virtual_init_user_shared_data(), then spawns wineboot.exe --init.
        // Wineboot runs with QUARK_FAST_BOOT=1 (patch 016) and finishes
        // in milliseconds, then signals the event. This gives us proper
        // KUSER_SHARED_DATA + fast boot without BSODs.
        if is_unparented && !self.boot_process_claimed {
            self.boot_process_claimed = true;
            self.reassign_user_handle_owner(self.desktop_top_window, tid, pid);
            self.reassign_user_handle_owner(self.desktop_msg_window, tid, pid);
            // Store startup info for get_startup_info later, but keep info_size=0
            // so ntdll takes the build_initial_params path.
            if info_size == 0 {
                if let Ok(game_exe) = std::env::var("TRISKELION_GAME_EXE") {
                    let nt_path = format!("\\??\\Z:{}", game_exe.replace('/', "\\"));
                    let game_dir = if let Some(pos) = nt_path.rfind('\\') {
                        &nt_path[..pos]
                    } else {
                        &nt_path
                    };
                    let curdir_u16: Vec<u8> = game_dir.encode_utf16()
                        .flat_map(|c| c.to_le_bytes()).collect();
                    let imagepath_u16: Vec<u8> = nt_path.encode_utf16()
                        .flat_map(|c| c.to_le_bytes()).collect();
                    let cmdline_u16: Vec<u8> = nt_path.encode_utf16()
                        .flat_map(|c| c.to_le_bytes()).collect();
                    let struct_size = 96usize;
                    let total = struct_size + curdir_u16.len() + imagepath_u16.len() + cmdline_u16.len();
                    let mut si = vec![0u8; total];
                    si[64..68].copy_from_slice(&(curdir_u16.len() as u32).to_le_bytes());
                    si[72..76].copy_from_slice(&(imagepath_u16.len() as u32).to_le_bytes());
                    si[76..80].copy_from_slice(&(cmdline_u16.len() as u32).to_le_bytes());
                    let mut off = struct_size;
                    si[off..off + curdir_u16.len()].copy_from_slice(&curdir_u16);
                    off += curdir_u16.len();
                    si[off..off + imagepath_u16.len()].copy_from_slice(&imagepath_u16);
                    off += imagepath_u16.len();
                    si[off..off + cmdline_u16.len()].copy_from_slice(&cmdline_u16);
                    if let Some(process) = self.state.processes.get_mut(&pid) {
                        process.info_size = si.len() as u32;
                        process.startup_info = Some(si);
                    }
                }
                // Keep info_size=0 — ntdll needs build_initial_params path
                info_size = 0;
                log_info!("boot: info_size=0 for first process (wineboot runs with QUARK_FAST_BOOT)");
            }
        } else if is_unparented && self.boot_process_claimed {
            // Game process (second+ unparented): reassign desktop ownership so
            // get_window_thread(desktop) returns a LIVE tid. The boot process
            // (wineboot) already exited, leaving the desktop with a stale tid.
            self.reassign_user_handle_owner(self.desktop_top_window, tid, pid);
            self.reassign_user_handle_owner(self.desktop_msg_window, tid, pid);
            if let Some(ws) = self.window_states.get_mut(&self.desktop_top_window) {
                ws.tid = tid;
            }
            if let Some(ws) = self.window_states.get_mut(&self.desktop_msg_window) {
                ws.tid = tid;
            }
        }


        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
        let server_start = (ts.tv_sec as i64) * 10_000_000 + (ts.tv_nsec as i64) / 100;

        // Wine checks supported_machines_count after init_first_thread.
        // Both AMD64 + I386 — WoW64 support required by Proton 10.0's ntdll.
        const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
        const IMAGE_FILE_MACHINE_I386: u16 = 0x014c;
        let machines: [u16; 2] = [IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_I386];
        let machines_bytes = unsafe {
            std::slice::from_raw_parts(machines.as_ptr() as *const u8, 4)
        };

        // Send the ntsync device fd to the client for inproc synchronization.
        // Wine uses this fd for direct futex_waitv (sys_449) on ntsync objects,
        // bypassing the wineserver for wait operations.
        let inproc_device: u32 = if let Some(ntsync) = &self.ntsync {
            let dev_fd = unsafe { libc::dup(ntsync.fd()) };
            if dev_fd >= 0 {
                // Tag must match reply->inproc_device (Wine asserts handle == reply->inproc_device)
                let tag = 0xFACE0000u32;
                if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                    client.pending_fd = Some((dev_fd, tag));
                }
                tag
            } else { 0 }
        } else { 0 };

        // Eagerly create alert event so pipe completion can signal it.
        self.get_or_create_alert(client_fd as RawFd);

        // session_id: stock wineserver inherits from parent or uses the terminal session.
        // For interactive desktop sessions, this is typically 1 (Windows console session).
        // Wine uses it for winstation/desktop isolation. 0 = services session (wrong for games).
        let session_id = self.state.processes.get(&pid)
            .and_then(|p| {
                if p.parent_pid != 0 {
                    self.state.processes.get(&p.parent_pid).map(|pp| pp.session_id)
                } else {
                    None
                }
            })
            .unwrap_or(1); // Default to session 1 (interactive)

        // Store in process for child inheritance
        if let Some(process) = self.state.processes.get_mut(&pid) {
            process.session_id = session_id;
        }

        let reply = InitFirstThreadReply {
            header: ReplyHeader { error: 0, reply_size: machines_bytes.len() as u32 },
            pid,
            tid,
            server_start,
            session_id,
            inproc_device,
            info_size,
            _pad_0: [0; 4],
        };
        reply_vararg(&reply, machines_bytes)
    }


    pub(crate) fn handle_init_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<InitThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InitThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // The new thread sends reply_fd and wait_fd via SCM_RIGHTS on the PROCESS's
        // msg_fd (shared connection socket). Use retry drain — same SCM_RIGHTS race
        // as new_thread (fd can arrive after the request).
        let reply_fd_val = self.take_inflight_fd(client_fd as RawFd, req.reply_fd);
        let wait_fd_val = self.take_inflight_fd(client_fd as RawFd, req.wait_fd);

        if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
            if let Some(fd) = reply_fd_val {
                client.reply_fd = Some(fd);
            }
            if let Some(fd) = wait_fd_val {
                client.wait_fd = Some(fd);
            }
        }

        // Drain any pending wakes deferred from before wait_fd was available
        if let Some(wakes) = self.pending_wakes.remove(&(client_fd as RawFd)) {
            if let Some(client) = self.clients.get(&(client_fd as RawFd)) {
                if let Some(wait_fd) = client.wait_fd {
                    for (cookie, signaled) in wakes {
                        let reply = WakeUpReply { cookie, signaled, _pad: 0 };
                        unsafe {
                            libc::write(wait_fd, &reply as *const _ as *const _, 16);
                        }
                    }
                }
            }
        }

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .unwrap_or_else(|| {
                self.state.processes.keys().next().copied().unwrap_or_else(|| {
                    self.state.create_process()
                })
            });

        // Reuse tid from new_thread if already assigned (avoids double-creating
        // thread IDs, which would orphan the new_thread tid in process.threads
        // and prevent remaining_threads from reaching 0).
        let existing_tid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.thread_id != 0 { Some(c.thread_id) } else { None });
        let tid = existing_tid.unwrap_or_else(|| self.state.create_thread(pid));

        // SHM slot keyed by Wine thread ID (not unix_tid) — matches message lookup.
        let _slot = self.shm.alloc_slot(tid as thread_id_t);

        // Reuse the process's first thread's shared objects instead of allocating
        // new ones. Stock wineserver allocates queues on demand per-thread, but
        // the session memfd pool is fixed-size (4096 objects). With Wine's RPC
        // thread pool creating thousands of short-lived threads, bump-allocating
        // 2 objects per thread exhausts the pool in seconds.
        let (queue_locator, input_locator) = self.clients.values()
            .find(|c| c.process_id == pid && c.queue_locator != [0u8; 16])
            .map(|c| (c.queue_locator, c.input_locator))
            .unwrap_or_else(|| {
                let q = self.alloc_shared_object();
                let i = self.alloc_shared_object();
                let input_offset = u64::from_le_bytes(i[8..16].try_into().unwrap());
                self.shared_write(input_offset, |shm| unsafe {
                    *(shm as *mut i32) = 1;
                });
                (q, i)
            });

        // Store thread's unix_pid from its process, unix_tid from request, plus teb/entry
        let process_unix_pid = self.clients.values()
            .find(|c| c.process_id == pid && c.unix_pid != 0)
            .map(|c| c.unix_pid)
            .unwrap_or(0);

        if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
            client.thread_id = tid;
            client.process_id = pid;
            client.queue_locator = queue_locator;
            client.input_locator = input_locator;
            client.teb = req.teb;
            client.entry_point = req.entry;
            client.unix_tid = req.unix_tid;
            client.unix_pid = process_unix_pid;
        }

        let has_exit_events = self.thread_exit_events.contains_key(&(client_fd as RawFd));

        // Eagerly create alert event so pipe completion can signal it.
        self.get_or_create_alert(client_fd as RawFd);

        // Tell ntdll whether this thread was created suspended.
        // ntdll will call select() with context data to park itself.
        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        let suspended = self.state.threads.get(&tid)
            .map(|t| t.suspend_count > 0)
            .unwrap_or(false);

        let reply = InitThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            suspend: if suspended { 1 } else { 0 },
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_startup_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });

        // Concatenate startup_info + env (no ROUND_SIZE padding between them).
        // Stock wineserver returns [info (info_size bytes)][env] contiguously.
        // Wine's init_startup_info finds env at info + info_size.
        let (info_size, machine, vararg) = pid
            .and_then(|p| self.state.processes.get_mut(&p))
            .map(|process| {
                let info = process.startup_info.take().unwrap_or_default();
                let env = process.startup_env.take().unwrap_or_default();
                let machine = if process.machine != 0 { process.machine } else { 0x8664 };
                let info_size = process.info_size;
                let mut combined = Vec::with_capacity(info.len() + env.len());
                combined.extend_from_slice(&info);
                combined.extend_from_slice(&env);
                (info_size, machine, combined)
            })
            .unwrap_or((0, 0x8664, Vec::new()));

        log_info!("GET_STARTUP_INFO: pid={:?} info_size={info_size} vararg_len={}", pid, vararg.len());

        let max_vararg = max_reply_vararg(buf) as usize;
        let send_len = vararg.len().min(max_vararg);
        let vararg_slice = &vararg[..send_len];

        let reply = GetStartupInfoReply {
            header: ReplyHeader { error: 0, reply_size: send_len as u32 },
            info_size,
            machine,
            _pad_0: [0; 2],
        };

        if vararg.len() >= 48 {
            // Dump raw struct header for debugging
            let dump_len = 80.min(vararg.len());
            let hex: String = vararg[..dump_len].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
            log_info!("get_startup_info: info_size={info_size} vararg[0..{dump_len}]: {hex}");

            // startup_info_data layout (from Wine server_protocol.h):
            // u32 debug_flags       @0
            // u32 console_flags     @4
            // u32 console           @8
            // u32 hstdin            @12
            // u32 hstdout           @16
            // u32 hstderr           @20
            // u32 x, y, xsize, ysize, xchars, ychars  @24-47
            // u32 attribute         @48
            // u32 flags             @52
            // u32 show              @56
            // u32 process_group_id  @60
            // u32 curdir_len        @64
            // u32 dllpath_len       @68
            // u32 imagepath_len     @72
            let imagepath_len = if vararg.len() >= 76 {
                u32::from_le_bytes([vararg[72], vararg[73], vararg[74], vararg[75]]) as usize
            } else { 0 };
            let curdir_len = if vararg.len() >= 68 {
                u32::from_le_bytes([vararg[64], vararg[65], vararg[66], vararg[67]]) as usize
            } else { 0 };
            let dllpath_len = if vararg.len() >= 72 {
                u32::from_le_bytes([vararg[68], vararg[69], vararg[70], vararg[71]]) as usize
            } else { 0 };
            // imagepath starts after the fixed struct (96 bytes) + curdir + dllpath
            // startup_info_data is 24 u32 fields = 96 bytes fixed.
            // info_size is the TOTAL (struct + all variable strings), not just the struct.
            let struct_size: usize = 96;
            let imagepath_offset = struct_size + curdir_len + dllpath_len;
            if imagepath_offset + imagepath_len <= vararg.len() && imagepath_len > 0 {
                let imagepath_bytes = &vararg[imagepath_offset..imagepath_offset + imagepath_len];
                let imagepath = String::from_utf16_lossy(
                    &imagepath_bytes.chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect::<Vec<u16>>()
                );
                // Also decode curdir
                let curdir_str = if curdir_len > 0 && struct_size + curdir_len <= vararg.len() {
                    let cb = &vararg[struct_size..struct_size + curdir_len];
                    String::from_utf16_lossy(&cb.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<u16>>())
                } else { String::new() };
                log_info!("get_startup_info: imagepath=\"{imagepath}\" curdir=\"{curdir_str}\" (len={imagepath_len})");
            } else {
                log_warn!("get_startup_info: imagepath not found (struct_size={struct_size} curdir={curdir_len} dllpath={dllpath_len} imagepath_len={imagepath_len} vararg_len={})", vararg.len());
            }
        }
        reply_vararg(&reply, vararg_slice)
    }


    pub(crate) fn handle_get_thread_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        // Parse the full request struct (header at offset 0, handle at offset 12)
        if buf.len() < std::mem::size_of::<GetThreadInfoRequest>() {
            return reply_fixed(&GetThreadInfoReply {
                header: ReplyHeader { error: 0xc0000023, reply_size: 0 },
                pid: 0, tid: 0, teb: 0, entry_point: 0, affinity: 0,
                exit_code: 0, priority: 0, base_priority: 0,
                suspend_count: 0, flags: 0, desc_len: 0,
            });
        }
        let req: GetThreadInfoRequest = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const _) };
        let handle = req.handle;

        // 0xFFFFFFFE = current thread, 0xFFFFFFFD = current process (treat as current thread)
        let (pid, tid, teb, entry_point) = if handle == 0xFFFFFFFE || handle == 0xFFFFFFFD || handle == 0 {
            self.clients.get(&(client_fd as RawFd))
                .map(|c| (c.process_id, c.thread_id, c.teb, c.entry_point))
                .unwrap_or((0, 0, 0, 0))
        } else {
            // Resolve handle to tid via thread_handle_tids (from get_next_thread),
            // or by scanning the client table for handles from new_thread.
            let target_tid = self.thread_handle_tids.get(&handle).copied()
                .or_else(|| {
                    // Resolve through process handle table. new_thread stores
                    // tid as object_id (thread.rs:85), so read it back here.
                    let caller_pid = self.client_pid(client_fd as RawFd);
                    self.state.processes.get(&caller_pid)
                        .and_then(|p| p.handles.get(handle))
                        .map(|e| e.object_id as u32)
                });
            if let Some(target_tid) = target_tid {
                // Find the client fd for this tid
                self.clients.values()
                    .find(|c| c.thread_id == target_tid)
                    .map(|c| (c.process_id, c.thread_id, c.teb, c.entry_point))
                    .unwrap_or((0, 0, 0, 0))
            } else {
                // Fall back to caller's thread
                self.clients.get(&(client_fd as RawFd))
                    .map(|c| (c.process_id, c.thread_id, c.teb, c.entry_point))
                    .unwrap_or((0, 0, 0, 0))
            }
        };

        let reply = GetThreadInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            pid,
            tid,
            teb,
            entry_point,
            affinity: 0xFFFF, // Match stock: 16-core affinity mask
            exit_code: 259,   // STILL_ACTIVE (0x103)
            priority: 0,
            base_priority: 0,
            suspend_count: 0,
            flags: 0,
            desc_len: 0,
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_terminate_thread(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = TerminateThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            is_self: 1,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_resume_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<ResumeThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const ResumeThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let caller_pid = self.client_pid(client_fd as RawFd);
        // Resolve handle → thread object_id (which IS the tid)
        let target_tid = self.state.processes.get(&caller_pid)
            .and_then(|p| p.handles.get(req.handle))
            .map(|e| e.object_id as u32);

        log_info!("resume_thread: handle={:#x} caller_pid={caller_pid} target_tid={target_tid:?}", req.handle);

        let mut old_count = 0i32;

        if let Some(ttid) = target_tid {
            // Decrement suspend_count on the Thread object
            if let Some(thread) = self.state.threads.get_mut(&ttid) {
                old_count = thread.suspend_count;
                if thread.suspend_count > 0 {
                    thread.suspend_count -= 1;
                }
            }

            // If thread is connected and blocked in select (suspend_cookie != 0), wake it
            let suspended: Option<(RawFd, i32, u64)> = self.clients.iter()
                .find(|(_, c)| c.thread_id == ttid && c.suspend_cookie != 0)
                .map(|(&fd, c)| (fd, c.wait_fd.unwrap_or(-1), c.suspend_cookie));

            if let Some((target_fd, wait_fd, cookie)) = suspended {
                if wait_fd >= 0 {
                    let mut wake = [0u8; 16];
                    wake[0..8].copy_from_slice(&cookie.to_le_bytes());
                    wake[8..12].copy_from_slice(&0x100u32.to_le_bytes()); // STATUS_KERNEL_APC
                    let wr = unsafe { libc::write(wait_fd, wake.as_ptr() as *const _, 16) };
                    log_info!("RESUME_THREAD: tid={ttid} target_fd={target_fd} wait_fd={wait_fd} cookie={cookie:#x} write_result={wr}");
                    if let Some(client) = self.clients.get_mut(&target_fd) {
                        client.suspend_cookie = 0;
                    }
                }
            } else if old_count > 0 {
                // Thread hasn't called select yet — suspend_count is decremented,
                // so init_thread/select will see it's no longer suspended.
                log_info!("resume_thread: tid={ttid} decremented suspend_count {old_count} -> {} (not yet blocked)",
                    old_count - 1);
            }
        } else {
            log_warn!("resume_thread: handle={:#x} not found in pid={caller_pid} handle table", req.handle);
        }

        reply_fixed(&ResumeThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            count: old_count,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_set_thread_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&SetThreadInfoReply { header: ReplyHeader { error: 0, reply_size: 0 } })
    }


    pub(crate) fn handle_get_thread_times(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
        // Windows FILETIME: 100ns intervals since Jan 1, 1601
        // Approximate: use monotonic time + epoch offset
        let creation = (ts.tv_sec as i64) * 10_000_000 + (ts.tv_nsec as i64) / 100;

        // Return the thread's actual unix_pid/unix_tid (from init_thread request)
        let (unix_pid, unix_tid) = self.clients.get(&(client_fd as RawFd))
            .map(|c| (c.unix_pid, c.unix_tid))
            .unwrap_or((0, 0));

        let reply = GetThreadTimesReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            creation_time: creation,
            exit_time: 0,
            unix_pid,
            unix_tid,
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_open_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<OpenThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const OpenThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        let reply = OpenThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_set_thread_context(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = SetThreadContextReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            is_self: 1,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_thread_context(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let reply = GetThreadContextReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            is_self: 1,
            handle: 0,
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_suspend_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SuspendThreadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SuspendThreadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Resolve thread handle to tid
        let caller_pid = self.client_pid(client_fd as RawFd);
        let target_tid = self.state.processes.get(&caller_pid)
            .and_then(|p| p.handles.get(req.handle))
            .map(|e| e.object_id as u32)
            .or_else(|| self.thread_handle_tids.get(&req.handle).copied());

        let prev_count = if let Some(tid) = target_tid {
            let count = self.state.threads.get(&tid)
                .map(|t| t.suspend_count).unwrap_or(0);
            if let Some(thread) = self.state.threads.get_mut(&tid) {
                thread.suspend_count += 1;
            }
            // Send SIGUSR1 to actually suspend the thread (stock: thread.c:389)
            // Wine's SIGUSR1 handler calls wait_suspend -> server_select(INTERRUPTIBLE)
            if count == 0 {
                if let Some(client) = self.clients.values().find(|c| c.thread_id == tid) {
                    let unix_pid = client.unix_pid;
                    let unix_tid = client.unix_tid;
                    if unix_pid > 0 && unix_tid > 0 {
                        unsafe { libc::syscall(libc::SYS_tgkill, unix_pid, unix_tid, libc::SIGUSR1); }
                    }
                }
            }
            count
        } else { 0 };

        reply_fixed(&SuspendThreadReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            count: prev_count as i32,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_get_next_thread(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() < std::mem::size_of::<GetNextThreadRequest>() {
            return reply_fixed(&GetNextThreadReply {
                header: ReplyHeader { error: 0xc0000023, reply_size: 0 }, // STATUS_BUFFER_TOO_SMALL
                handle: 0, _pad_0: [0; 4],
            });
        }
        let req: GetNextThreadRequest = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const _) };

        // Resolve process handle to pid.
        // 0xffffffff = pseudo-handle for current process (the common case from ntdll's TLS allocator).
        // For real handles, fall back to caller's process — sufficient for TLS allocation.
        let caller_pid = self.client_pid(client_fd as RawFd);
        let target_pid = caller_pid;

        // Collect threads for the target process, sorted by tid for stable iteration
        let mut tids: Vec<u32> = self.state.threads.iter()
            .filter(|(_, t)| t.pid == target_pid)
            .map(|(&tid, _)| tid)
            .collect();
        tids.sort_unstable();
        if req.flags == 1 { tids.reverse(); }

        // Find the thread after `last`
        let next_tid = if req.last == 0 {
            tids.first().copied()
        } else {
            // Resolve `last` handle to a tid via our tracking map
            let last_tid = self.thread_handle_tids.get(&req.last).copied();

            if let Some(lt) = last_tid {
                // Find the next tid after lt in our sorted list
                tids.iter().find(|&&t| t > lt).copied()
            } else {
                None
            }
        };

        match next_tid {
            Some(tid) => {
                // Allocate a handle for this thread in the caller's process.
                // Store tid as object_id so get_thread_info can resolve the
                // handle back to the correct thread (for TEB lookup in
                // alloc_tls_slot's NtQueryInformationThread calls).
                let handle = if let Some(proc) = self.state.processes.get_mut(&caller_pid) {
                    proc.handles.allocate(tid as u64)
                } else { 0 };

                if handle == 0 {
                    return reply_fixed(&GetNextThreadReply {
                        header: ReplyHeader { error: 0x8000001a, reply_size: 0 },
                        handle: 0, _pad_0: [0; 4],
                    });
                }

                // Store mapping from this object_id to the thread's tid
                // so subsequent get_next_thread calls can resolve `last`
                // We reuse the thread_handle_tids map for this
                self.thread_handle_tids.insert(handle, tid);

                reply_fixed(&GetNextThreadReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    handle,
                    _pad_0: [0; 4],
                })
            }
            None => {
                reply_fixed(&GetNextThreadReply {
                    header: ReplyHeader { error: 0x8000001a, reply_size: 0 }, // STATUS_NO_MORE_ENTRIES
                    handle: 0, _pad_0: [0; 4],
                })
            }
        }
    }
}
