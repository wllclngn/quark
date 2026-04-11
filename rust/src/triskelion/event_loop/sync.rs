// Synchronization primitives — select, events, mutexes, semaphores, waits

use super::*;
#[allow(unused_variables)]



/// Convert Wine's Select timeout to ntsync absolute CLOCK_MONOTONIC nanoseconds.
/// ntsync treats timeout as absolute on the specified clock.
/// Returns 0 for poll (timeout=0 in Wine = already past).
fn compute_ntsync_timeout(timeout: i64) -> u64 {
    // Get current CLOCK_MONOTONIC as absolute nanoseconds
    let mut mono_ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut mono_ts); }
    let mono_now_ns = (mono_ts.tv_sec as u64) * 1_000_000_000 + (mono_ts.tv_nsec as u64);

    // TIMEOUT_INFINITE (INT64_MAX = 0x7FFFFFFFFFFFFFFF) → use u64::MAX.
    // ntsync timeout is an absolute CLOCK_MONOTONIC deadline in ns.
    // 0 means "deadline at boot time" → immediately expired. u64::MAX = effectively infinite.
    if timeout == i64::MAX {
        return u64::MAX;
    }

    if timeout < 0 {
        // Relative: 100ns units → add to current CLOCK_MONOTONIC
        let rel_ns = ((-timeout) as u64).saturating_mul(100);
        mono_now_ns.saturating_add(rel_ns)
    } else if timeout > 0 {
        // Absolute FILETIME → convert to relative ns, then add to CLOCK_MONOTONIC
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts); }
        let now_100ns = (ts.tv_sec as i64) * 10_000_000 + (ts.tv_nsec as i64) / 100 + TICKS_1601_TO_1970;
        if timeout <= now_100ns {
            1 // already past → use 1ns (poll, not infinite)
        } else {
            let rel_ns = ((timeout - now_100ns) as u64).saturating_mul(100);
            mono_now_ns.saturating_add(rel_ns)
        }
    } else {
        1 // poll — use 1ns (not 0, which means infinite in ntsync)
    }
}

impl EventLoop {

    // Check timeout-only waits (pure sleeps with no ntsync objects).
    // ntsync-backed waits are handled by kernel-blocking worker threads.
    pub(crate) fn check_pending_waits(&mut self) {
        if self.pending_waits.is_empty() {
            return;
        }
        let now = Instant::now();
        // Pop from min-heap while the earliest deadline has passed or client disconnected
        while let Some(top) = self.pending_waits.peek() {
            // Lazy cleanup: skip waits for disconnected clients
            if !self.clients.contains_key(&top.0.client_fd) {
                self.pending_waits.pop();
                continue;
            }
            if now >= top.0.deadline {
                let Reverse(pending) = self.pending_waits.pop().unwrap();
                self.send_wake_up(&pending, 0x0000_0102_u32 as i32); // STATUS_TIMEOUT
            } else {
                break;
            }
        }
        self.arm_timer();
    }





    // Arm timerfd to fire at the nearest pending wait deadline.
    // Disarms if no waits are pending.
    fn arm_timer(&self) {
        let spec = if let Some(Reverse(nearest_pw)) = self.pending_waits.peek() {
            let nearest = nearest_pw.deadline;
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


    // ---- Sync primitives (critical -- NOT_IMPLEMENTED here = system freeze) ----

    pub(crate) fn handle_select(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        // select is Wine's universal wait/sleep mechanism.
        // Returning immediately causes a CPU spin. We must defer the reply
        // for timed waits, and handle polls (timeout=0) immediately.
        let req = if buf.len() >= std::mem::size_of::<SelectRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SelectRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Check for pending kernel APCs (e.g., pipe connect completion).
        // Must be checked before suspension — the thread might re-enter select after
        // processing an APC (with prev_apc set), and we deliver the next APC.
        // Pop APC from queue. If Wine re-enters Select before ACKing (alert
        // wakes both daemon wait thread and inproc linux_wait_objs), the APC
        // is already gone — but the client stores it from the first delivery.
        // The second Select returns STATUS_USER_APC with empty data (apc_handle=0),
        // which Wine tolerates.
        let pending_apc = self.pending_kernel_apcs.get_mut(&(client_fd as RawFd))
            .and_then(|apcs| apcs.pop());
        if pending_apc.is_some() {
            if let Some(apcs) = self.pending_kernel_apcs.get(&(client_fd as RawFd)) {
                if apcs.is_empty() { self.pending_kernel_apcs.remove(&(client_fd as RawFd)); }
            }
        }
        if let Some(apc_data) = pending_apc {
            let has_deferred = self.deferred_event_signals.contains_key(&(client_fd as RawFd));
            log_info!("APC_DELIVER: fd={client_fd} apc_type={} has_deferred={has_deferred}",
                u32::from_le_bytes([apc_data[0], apc_data[1], apc_data[2], apc_data[3]]));
            // Allocate an apc_handle for the client to ACK in the next select
            let pid = self.client_pid(client_fd as RawFd);
            let apc_handle = {
                let oid = self.state.alloc_object_id();
                if let Some(process) = self.state.processes.get_mut(&pid) {
                    process.handles.allocate(oid)
                } else { 0 }
            };

            // Clear apc_flag — the APC is being delivered on reply_fd, not via
            // the ntsync worker. If a stale flag remains, the NEXT worker thread
            // will exit without writing to wait_fd, causing a hang.
            if let Some(flag) = self.client_apc_flags.get(&(client_fd as RawFd)) {
                flag.store(false, std::sync::atomic::Ordering::Release);
            }

            // No alert reset needed — the daemon never signals client_alerts.
            // Worker interrupt is auto-reset (consumed by ntsync ioctl).

            // Clear current_wait_cookie — thread is no longer in a deferred wait.
            if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                client.current_wait_cookie = 0;
            }

            // DON'T signal deferred events here — the APC hasn't been processed yet.
            // Signal after prev_apc ACK (below), when irp_completion has written the IOSB.

            // System APCs (type >= 2): STATUS_KERNEL_APC so Wine calls invoke_system_apc.
            // User APCs (type < 2): STATUS_USER_APC so Wine exits server_wait immediately.
            // The inproc path (linux_wait_objs) never reaches here for system APCs because
            // we never signal the thread's alert — only the worker interrupt.
            let apc_type = u32::from_le_bytes([apc_data[0], apc_data[1], apc_data[2], apc_data[3]]);
            let status = if apc_type >= 2 { 0x0000_0100u32 } else { 0x0000_00C0u32 };
            let reply = SelectReply {
                header: ReplyHeader { error: status, reply_size: apc_data.len() as u32 },
                apc_handle,
                signaled: 1,
            };
            return reply_vararg(&reply, &apc_data);
        }

        // If prev_apc is set, the client is ACKing a previous APC.
        // The client processed the APC (irp_completion wrote the IOSB) and
        // returned the result via the apc_result VARARG. We must:
        //   1. Parse the apc_result to get completion status
        //   2. Signal deferred events so the caller sees the completed I/O
        //   3. Close the prev_apc handle (allocated when the APC was delivered)
        //   4. Wake fsync slots for deferred handles
        if req.prev_apc != 0 {
            let pid = self.client_pid(client_fd as RawFd);

            // Parse apc_result (40 bytes at VARARG start)
            // Layout: type(u32) + variant-specific fields
            // APC_ASYNC_IO (type=2): status(u32) + total(u32)
            let apc_result_offset = VARARG_OFF;
            let apc_type = if buf.len() >= apc_result_offset + 4 {
                u32::from_le_bytes([
                    buf[apc_result_offset], buf[apc_result_offset + 1],
                    buf[apc_result_offset + 2], buf[apc_result_offset + 3],
                ])
            } else { 0 };
            let _apc_status = if apc_type == 2 && buf.len() >= apc_result_offset + 8 {
                u32::from_le_bytes([
                    buf[apc_result_offset + 4], buf[apc_result_offset + 5],
                    buf[apc_result_offset + 6], buf[apc_result_offset + 7],
                ])
            } else { 0 };

            // Signal deferred events (pipe listen completion, etc.)
            // Stock wineserver: async_set_result → set_event runs here (APC destructor).
            if let Some(events) = self.deferred_event_signals.remove(&(client_fd as RawFd)) {
                log_info!("deferred_event_signal: fd={client_fd} signaling {} event(s)", events.len());
                for (evpid, handle) in &events {
                    if let Some((obj, _)) = self.ntsync_objects.get(&(*evpid, *handle)) {
                        let _ = obj.event_set();
                    } else {
                        log_warn!("deferred_event_signal: MISS pid={evpid} handle={handle:#x}");
                    }
                }
            }

            // Close the APC handle — it was allocated when the APC was delivered
            // (line ~173). Without this, handles leak until the table fills up
            // and Wine gets STATUS_INVALID_HANDLE on subsequent operations.
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.handles.close(req.prev_apc);
            }
        }

        // Thread suspension: when the select vararg includes CONTEXT data
        // (after apc_result + select_op), the thread is suspending.
        // Return STATUS_PENDING; resume_thread wakes it.
        const APC_RESULT_SIZE_CHECK: usize = 40;
        let contexts_offset = super::VARARG_OFF + APC_RESULT_SIZE_CHECK + req.size as usize;
        let has_contexts = buf.len() > contexts_offset;
        if has_contexts && req.size == 0 {
            let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
            // Only block if the thread is still suspended (suspend_count > 0).
            // If resume_thread already ran, suspend_count is 0 — don't block.
            let still_suspended = self.state.threads.get(&tid)
                .map(|t| t.suspend_count > 0)
                .unwrap_or(false);
            if still_suspended {
                let cookie_val = req.cookie;
                if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                    client.suspend_cookie = cookie_val as u64;
                    log_info!("SELECT_SUSPEND: tid={tid} fd={client_fd} cookie={cookie_val:#x} wait_fd={:?}",
                        client.wait_fd);
                }
                return reply_fixed(&SelectReply {
                    header: ReplyHeader { error: 0x103, reply_size: 0 },
                    apc_handle: 0,
                    signaled: 0,
                });
            }
        }

        let has_objects = req.size > 0;

        // Parse select_op to extract wait handles (if ntsync available)
        // VARARG layout: [apc_result(40 bytes)] [select_op(req.size bytes)] [contexts...]
        // select_op: [opcode(u32)] [handles(u32 each)...]
        const APC_RESULT_SIZE: usize = 40;
        const SELECT_WAIT: u32 = 1;
        const SELECT_WAIT_ALL: u32 = 2;

        let mut ntsync_fds: Vec<RawFd> = Vec::new();
        let mut ntsync_arcs: Vec<Arc<crate::ntsync::NtsyncObj>> = Vec::new();
        let mut wait_all = false;
        let mut owner: u32 = 0;

        if has_objects && self.ntsync.is_some() {
            let select_op_offset = VARARG_OFF + APC_RESULT_SIZE;
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
                    let pid = self.client_pid(client_fd as RawFd);
                    owner = tid;

                    let mut handles_debug: Vec<String> = Vec::new();
                    let mut all_have_ntsync = true;
                    for h_idx in 0..handle_count {
                        let off = handles_start + h_idx * 4;
                        if off + 4 > buf.len() { all_have_ntsync = false; break; }
                        let handle = u32::from_le_bytes([
                            buf[off], buf[off + 1], buf[off + 2], buf[off + 3],
                        ]);
                        if let Some((obj, sync_type)) = self.ntsync_objects.get(&(pid, handle)) {
                            let type_name = match *sync_type { 1 => "INT", 2 => "EVT", 3 => "MUT", 4 => "SEM", _ => "?" };
                            handles_debug.push(format!("{handle:#x}({type_name})"));
                            ntsync_fds.push(obj.fd());
                            ntsync_arcs.push(Arc::clone(obj));
                        } else {
                            // Handle not in ntsync_objects — create an auto-reset signaled fallback.
                            // Signaled so the wait completes rather than blocking indefinitely
                            // on a handle the server doesn't track.
                            if let Some(fallback_obj) = self.get_or_create_event(false, true) {
                                ntsync_fds.push(fallback_obj.fd());
                                ntsync_arcs.push(Arc::clone(&fallback_obj));
                                self.insert_recyclable_event(pid, handle, fallback_obj, 2); // EVENT
                                handles_debug.push(format!("{handle:#x}(FALLBACK)"));
                            } else {
                                handles_debug.push(format!("{handle:#x}(MISSING)"));
                                all_have_ntsync = false;
                                break;
                            }
                        }
                    }

                    // Store select cookie in PipeListenAsync for any pipe listen events
                    // being waited on. This lets try_connect_named_pipe deliver
                    // STATUS_KERNEL_APC via wait_fd instead of signaling the ntsync event.
                    for (_, instances) in self.named_pipes.iter_mut() {
                        for info in instances.iter_mut() {
                            if let Some((ep, eh)) = info.listen_event {
                                if ep == pid {
                                    for h_idx in 0..handle_count {
                                        let off = handles_start + h_idx * 4;
                                        if off + 4 <= buf.len() {
                                            let h = u32::from_le_bytes([buf[off], buf[off+1], buf[off+2], buf[off+3]]);
                                            if h == eh {
                                                if let Some(ref mut la) = info.listen_async {
                                                    if la.cookie != req.cookie {
                                                        la.cookie = req.cookie;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    log_info!("Select: fd={client_fd} pid={pid} handles=[{}] ntsync_fds={} timeout={}",
                        handles_debug.join(","), ntsync_fds.len(),
                        if req.timeout == 0x7FFF_FFFF_FFFF_FFFFu64 as i64 { "INF".to_string() } else { format!("{}ns", req.timeout) });
                    if !all_have_ntsync {
                        ntsync_fds.clear();
                    }
                }
            }
        }

        // Wine protocol: Select returns a reply on reply_fd for EVERY call.
        // Immediate results: error=STATUS_TIMEOUT or STATUS_WAIT_0+idx, signaled=1.
        //   Client sees signaled=1, breaks immediately, never calls wait_select_reply.
        // Pending results: error=STATUS_PENDING, signaled=0.
        //   Client enters wait_select_reply(), blocks on wait_fd for WakeUpReply.

        // Try immediate ntsync poll for object waits.
        // IMPORTANT: ntsync treats timeout=0 as INFINITE WAIT (NULL timeout_ptr).
        // Use timeout=1 (1ns absolute CLOCK_MONOTONIC, already in the past) for poll.
        let mut immediate_result: Option<i32> = None;
        if !ntsync_fds.is_empty() {
            if let Some(ntsync) = &self.ntsync {
                let result = if wait_all {
                    ntsync.wait_all(&ntsync_fds, 1, owner)
                } else {
                    ntsync.wait_any(&ntsync_fds, 1, owner)
                };
                match result {
                    crate::ntsync::WaitResult::Signaled(index) => {
                        immediate_result = Some(index as i32);
                    }
                    _ => {
                        if req.timeout == 0 {
                            immediate_result = Some(0x0000_0102_i32); // STATUS_TIMEOUT
                        }
                        // else: fall through to deferred path
                    }
                }
            }
        } else if req.timeout == 0 {
            // No objects, poll: immediately return STATUS_TIMEOUT
            immediate_result = Some(0x0000_0102_i32); // STATUS_TIMEOUT
        }

        // Immediate result: return directly in reply on reply_fd.
        // Client sees signaled=1, breaks without calling wait_select_reply.
        if let Some(signaled) = immediate_result {
            let error = if signaled == 0x0000_0102_i32 {
                0x0000_0102u32 // STATUS_TIMEOUT
            } else {
                signaled as u32 // STATUS_WAIT_0 + index
            };
            return reply_fixed(&SelectReply {
                header: ReplyHeader { error, reply_size: 0 },
                apc_handle: 0,
                signaled: 1,
            });
        }

        // Deferred wait: ntsync objects go to a kernel-blocking worker thread,
        // timeout-only waits (no objects) use the timerfd mechanism.
        let timeout_ns = compute_ntsync_timeout(req.timeout);

        // Signal idle event: process is entering a real blocking wait.
        // This is the WaitForInputIdle trigger — the process has finished init
        // and is now waiting for events (its message loop).
        if !ntsync_fds.is_empty() || timeout_ns > 1_000_000 {
            let idle_pid = self.client_pid(client_fd as RawFd);
            let already_signaled = self.state.processes.get(&idle_pid)
                .map(|p| p.idle_signaled).unwrap_or(true);
            if !already_signaled {
                if let Some(idle_event) = self.process_idle_events.get(&idle_pid) {
                    let _ = idle_event.event_set();
                }
                if let Some(proc) = self.state.processes.get_mut(&idle_pid) {
                    proc.idle_signaled = true;
                }
            }
        }

        if !ntsync_fds.is_empty() {
            // Kernel-blocking path: spawn thread, let ntsync do the waiting.
            let ntsync_device_fd = self.ntsync.as_ref().unwrap().fd();
            // Use worker interrupt (auto-reset) as alert_fd — NOT the thread's
            // inproc alert. Signaling client_alerts for system APCs would trigger
            // Wine's sync.c:441 assertion in the inproc ntsync wait path.
            let alert_fd = self.get_or_create_worker_interrupt(client_fd as RawFd);
            let apc_flag = self.get_or_create_apc_flag(client_fd as RawFd);
            let wait_fd = self.clients.get(&(client_fd as RawFd))
                .and_then(|c| c.wait_fd);

            if let Some(wfd) = wait_fd {
                // Store current wait cookie so try_connect_named_pipe can use it
                // to write STATUS_KERNEL_APC to wait_fd even when PipeListenAsync
                // didn't capture the cookie (connect before this select).
                if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                    client.current_wait_cookie = req.cookie;
                }

                // dup wait_fd so the thread owns its own copy (no race on disconnect)
                let dup_wfd = unsafe { libc::dup(wfd) };
                if dup_wfd >= 0 {
                    let cookie = req.cookie;
                    let is_wait_all = wait_all;
                    let thread_owner = owner;

                    // Arc clones keep ntsync fds alive while the wait thread
                    // is blocked — no dup() needed. close_handle drops the map
                    // entry's Arc, but the wait thread's clones keep the fd open.
                    let wait_arcs = std::mem::take(&mut ntsync_arcs);
                    let wait_obj_fds: Vec<RawFd> = wait_arcs.iter().map(|a| a.fd()).collect();

                    {

                        let thread_client_fd = client_fd;
                        let apc_flag_clone = apc_flag.clone();
                        std::thread::spawn(move || {
                            let result = if is_wait_all {
                                crate::ntsync::wait_all_blocking(ntsync_device_fd, &wait_obj_fds, timeout_ns, thread_owner, alert_fd)
                            } else {
                                crate::ntsync::wait_any_blocking(ntsync_device_fd, &wait_obj_fds, timeout_ns, thread_owner, alert_fd)
                            };

                            // Drop Arc clones — releases ntsync fds if last holder
                            drop(wait_arcs);

                            // Check apc_flag FIRST — if set, the broker already wrote
                            // STATUS_KERNEL_APC to wait_fd. We must NOT write anything
                            // (no double-write). Just clean up and exit.
                            if apc_flag_clone.load(std::sync::atomic::Ordering::Acquire) {
                                apc_flag_clone.store(false, std::sync::atomic::Ordering::Release);
                                unsafe { libc::close(dup_wfd); }
                                return;
                            }

                            let signaled = match result {
                                crate::ntsync::WaitResult::Signaled(index) => {
                                    index as i32
                                }
                                crate::ntsync::WaitResult::Timeout => {
                                    0x0000_0102_i32
                                }
                                crate::ntsync::WaitResult::Alerted => {
                                    // Alert event fired (APC delivery). If apc_flag is set,
                                    // the broker already wrote to wait_fd — don't double-write.
                                    if apc_flag_clone.load(std::sync::atomic::Ordering::Acquire) {
                                        apc_flag_clone.store(false, std::sync::atomic::Ordering::Release);
                                        unsafe { libc::close(dup_wfd); }
                                        return;
                                    }
                                    // Otherwise, send STATUS_KERNEL_APC so the client
                                    // re-enters Select and picks up the pending APC.
                                    0x0000_0100_i32 // STATUS_KERNEL_APC
                                }
                                crate::ntsync::WaitResult::Error => {
                                    // Don't silently drop — send STATUS_TIMEOUT so client
                                    // doesn't hang forever waiting for a wake-up
                                    log_warn!("wait_thread: fd={thread_client_fd} ERROR — sending TIMEOUT to prevent hang");
                                    0x0000_0102_i32
                                }
                            };

                            let reply = WakeUpReply { cookie, signaled, _pad: 0 };
                            unsafe {
                                libc::write(dup_wfd, &reply as *const _ as *const _, 16);
                                libc::close(dup_wfd);
                            }
                        });

                        // Return STATUS_PENDING on reply_fd — client enters wait_select_reply.
                        // Worker thread writes WakeUpReply to wait_fd when done.
                        return reply_fixed(&SelectReply {
                            header: ReplyHeader { error: 0x103, reply_size: 0 },
                            apc_handle: 0,
                            signaled: 0,
                        });
                    }
                }
            }
            // Fallback: couldn't dup wait_fd, use timer path
        }

        // Timeout-only wait (no ntsync objects, or fallback): use timerfd.
        // timeout_ns is an absolute CLOCK_MONOTONIC deadline from compute_ntsync_timeout().
        // Convert to relative Duration for Instant arithmetic.
        let deadline = if timeout_ns == u64::MAX {
            Instant::now() + std::time::Duration::from_secs(86400) // effectively infinite
        } else {
            let mut mono_ts: libc::timespec = unsafe { std::mem::zeroed() };
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut mono_ts); }
            let mono_now_ns = (mono_ts.tv_sec as u64) * 1_000_000_000 + (mono_ts.tv_nsec as u64);
            let relative_ns = timeout_ns.saturating_sub(mono_now_ns);
            Instant::now() + std::time::Duration::from_nanos(relative_ns)
        };


        self.pending_waits.push(Reverse(PendingWait {
            deadline,
            client_fd: client_fd as RawFd,
            cookie: req.cookie,
        }));
        self.arm_timer();

        // Return STATUS_PENDING on reply_fd — client enters wait_select_reply.
        // Timer fires send_wake_up which writes WakeUpReply to wait_fd.
        reply_fixed(&SelectReply {
            header: ReplyHeader { error: 0x103, reply_size: 0 },
            apc_handle: 0,
            signaled: 0,
        })
    }


    pub(crate) fn handle_create_event(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        // Parse request fields for ntsync
        let (manual_reset, initial_state) = if buf.len() >= std::mem::size_of::<CreateEventRequest>() {
            let req: CreateEventRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            (req.manual_reset != 0, req.initial_state != 0)
        } else {
            (false, false)
        };

        let name = extract_objattr_name(buf);
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        let pid = self.client_pid(client_fd as RawFd);

        if handle != 0 {
            // Check if named event already exists
            if let Some(ref name) = name {
                if let Some(&(canonical_fd, sync_type)) = self.named_sync.get(name.as_str()) {
                    let dup_fd = unsafe { libc::dup(canonical_fd) };
                    if dup_fd >= 0 {
                        let obj = Arc::new(crate::ntsync::NtsyncObj::from_raw_fd(dup_fd));
                        self.ntsync_objects.insert((pid, handle), (obj, sync_type));
                    }
                    log_warn!("CreateEvent EXISTS name=\"{name}\" handle={handle:#x} fd={client_fd} pid={pid}");
                    let reply = CreateEventReply {
                        header: ReplyHeader { error: 0x40000000, reply_size: 0 }, // STATUS_OBJECT_NAME_EXISTS
                        handle,
                        _pad_0: [0; 4],
                    };
                    return reply_fixed(&reply);
                }
            }

            // Create new kernel ntsync event
            {
                // Upstream Wine inproc_sync_type: 1=INTERNAL, 2=EVENT, 3=MUTEX, 4=SEMAPHORE
                let sync_type = 2u32; // EVENT
                // Named events: always create fresh (need canonical dup), not recyclable
                // Unnamed events: use freelist, mark recyclable
                let obj = if name.is_some() {
                    self.ntsync.as_ref().and_then(|n| n.create_event(manual_reset, initial_state)).map(Arc::new)
                } else {
                    self.get_or_create_event(manual_reset, initial_state)
                };
                if let Some(obj) = obj {
                    if let Some(ref name) = name {
                        log_info!("CreateEvent NEW name=\"{name}\" handle={handle:#x} fd={client_fd} pid={pid} manual={manual_reset} signaled={initial_state}");
                        if let Some(dup) = obj.dup() {
                            self.named_sync.insert(name.clone(), (dup.fd(), sync_type));
                            std::mem::forget(dup); // canonical fd is owned by named_sync map
                        }
                        self.ntsync_objects.insert((pid, handle), (obj, sync_type));
                        self.ntsync_objects_created += 1;
                    } else {
                        log_info!("CreateEvent UNNAMED handle={handle:#x} pid={pid} manual={manual_reset} signaled={initial_state} fd={client_fd}");
                        self.insert_recyclable_event(pid, handle, obj, sync_type);
                    }
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


    pub(crate) fn handle_event_op(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.client_pid(client_fd as RawFd);
        if buf.len() >= std::mem::size_of::<EventOpRequest>() {
            let req: EventOpRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            let op_name = match req.op { 0 => "PULSE", 1 => "SET", 2 => "RESET", _ => "UNKNOWN" };
            if let Some((obj, _)) = self.ntsync_objects.get(&(pid, req.handle)) {
                // Reverse-lookup: find named event for diagnostics
                let obj_fd = obj.fd();
                let _event_name: Option<&str> = self.named_sync.iter()
                    .find(|&(_, &(canonical_fd, _))| {
                        // Check if our obj's ntsync fd was dup'd from this canonical fd
                        // by comparing the ntsync device inode via fstat
                        canonical_fd == obj_fd
                    })
                    .map(|(name, _)| name.as_str());

                // Wine event_op codes: PULSE_EVENT=0, SET_EVENT=1, RESET_EVENT=2
                let prev = match req.op {
                    1 => obj.event_set().unwrap_or(0),
                    2 => obj.event_reset().unwrap_or(0),
                    0 => obj.event_pulse().unwrap_or(0),
                    _ => 0,
                };
                let reply = EventOpReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    state: prev as i32,
                    _pad_0: [0; 4],
                };
                return reply_fixed(&reply);
            }
            // Handle not in ntsync table — create a backing ntsync event on-the-fly.
            if let Some(obj) = self.get_or_create_event(true, false) {
                let prev = match req.op {
                    1 => obj.event_set().unwrap_or(0),
                    2 => obj.event_reset().unwrap_or(0),
                    0 => obj.event_pulse().unwrap_or(0),
                    _ => 0,
                };
                self.ntsync_objects.insert((pid, req.handle), (obj, 2)); // EVENT
                let reply = EventOpReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    state: prev as i32,
                    _pad_0: [0; 4],
                };
                return reply_fixed(&reply);
            }
            log_warn!("event_op {op_name} handle={:#x} pid={pid} — failed to create on-demand event!", req.handle);
        }
        let reply = EventOpReply {
            header: ReplyHeader { error: 0xC0000008, reply_size: 0 }, // STATUS_INVALID_HANDLE
            state: 0,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    // ---- Additional critical stubs to prevent hangs ----

    pub(crate) fn handle_create_mutex(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let owned = if buf.len() >= std::mem::size_of::<CreateMutexRequest>() {
            let req: CreateMutexRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            req.owned != 0
        } else {
            false
        };

        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        let name = extract_objattr_name(buf);
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        let pid = self.client_pid(client_fd as RawFd);

        if handle != 0 {
            // Check if named mutex already exists
            if let Some(ref name) = name {
                if let Some(&(canonical_fd, sync_type)) = self.named_sync.get(name.as_str()) {
                    let dup_fd = unsafe { libc::dup(canonical_fd) };
                    if dup_fd >= 0 {
                        let obj = Arc::new(crate::ntsync::NtsyncObj::from_raw_fd(dup_fd));
                        self.ntsync_objects.insert((pid, handle), (obj, sync_type));
                    }
                    let reply = CreateMutexReply {
                        header: ReplyHeader { error: 0x40000000, reply_size: 0 }, // STATUS_OBJECT_NAME_EXISTS
                        handle,
                        _pad_0: [0; 4],
                    };
                    return reply_fixed(&reply);
                }
            }

            // Create new kernel ntsync mutex
            if let Some(ntsync) = &self.ntsync {
                let (owner, count) = if owned { (tid, 1) } else { (0, 0) };
                if let Some(obj) = ntsync.create_mutex(owner, count) {
                    if let Some(ref name) = name {
                        if let Some(dup) = obj.dup() {
                            self.named_sync.insert(name.clone(), (dup.fd(), 3)); // MUTEX
                            std::mem::forget(dup);
                        }
                    }
                    self.ntsync_objects.insert((pid, handle), (Arc::new(obj), 3)); // MUTEX
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


    pub(crate) fn handle_create_semaphore(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let (initial, max) = if buf.len() >= std::mem::size_of::<CreateSemaphoreRequest>() {
            let req: CreateSemaphoreRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            (req.initial, req.max)
        } else {
            (0, 1)
        };

        let name = extract_objattr_name(buf);
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        let pid = self.client_pid(client_fd as RawFd);

        if handle != 0 {
            // Check if named semaphore already exists
            if let Some(ref name) = name {
                if let Some(&(canonical_fd, sync_type)) = self.named_sync.get(name.as_str()) {
                    let dup_fd = unsafe { libc::dup(canonical_fd) };
                    if dup_fd >= 0 {
                        let obj = Arc::new(crate::ntsync::NtsyncObj::from_raw_fd(dup_fd));
                        self.ntsync_objects.insert((pid, handle), (obj, sync_type));
                    }
                    let reply = CreateSemaphoreReply {
                        header: ReplyHeader { error: 0x40000000, reply_size: 0 }, // STATUS_OBJECT_NAME_EXISTS
                        handle,
                        _pad_0: [0; 4],
                    };
                    return reply_fixed(&reply);
                }
            }

            // Create new kernel ntsync semaphore
            if let Some(ntsync) = &self.ntsync {
                if let Some(obj) = ntsync.create_sem(initial, max) {
                    if let Some(ref name) = name {
                        if let Some(dup) = obj.dup() {
                            self.named_sync.insert(name.clone(), (dup.fd(), 4)); // SEMAPHORE
                            std::mem::forget(dup);
                        }
                    }
                    self.ntsync_objects.insert((pid, handle), (Arc::new(obj), 4)); // SEMAPHORE
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


    pub(crate) fn handle_release_semaphore(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.client_pid(client_fd as RawFd);
        if buf.len() >= std::mem::size_of::<ReleaseSemaphoreRequest>() {
            let req: ReleaseSemaphoreRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            if let Some((obj, _)) = self.ntsync_objects.get(&(pid, req.handle)) {
                match obj.sem_release(req.count) {
                    Ok(prev) => {
                        let reply = ReleaseSemaphoreReply {
                            header: ReplyHeader { error: 0, reply_size: 0 },
                            prev_count: prev,
                            _pad_0: [0; 4],
                        };
                        return reply_fixed(&reply);
                    }
                    Err(_e) => {
                    }
                }
            } else {
            }
        }
        reply_fixed(&ReleaseSemaphoreReply {
            header: ReplyHeader { error: 0xC0000008, reply_size: 0 }, // STATUS_INVALID_HANDLE
            prev_count: 0,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_release_mutex(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.client_pid(client_fd as RawFd);
        if buf.len() >= std::mem::size_of::<ReleaseMutexRequest>() {
            let req: ReleaseMutexRequest = unsafe {
                std::ptr::read_unaligned(buf.as_ptr() as *const _)
            };
            if let Some((obj, _)) = self.ntsync_objects.get(&(pid, req.handle)) {
                let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
                match obj.mutex_unlock(tid) {
                    Ok(prev) => {
                        let reply = ReleaseMutexReply {
                            header: ReplyHeader { error: 0, reply_size: 0 },
                            prev_count: prev,
                            _pad_0: [0; 4],
                        };
                        return reply_fixed(&reply);
                    }
                    Err(_e) => {
                    }
                }
            } else {
            }
        }
        reply_fixed(&ReleaseMutexReply {
            header: ReplyHeader { error: 0xC0000008, reply_size: 0 }, // STATUS_INVALID_HANDLE
            prev_count: 0,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_open_event(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.client_pid(client_fd as RawFd);
        let name = extract_open_name(buf);
        if let Some(ref name) = name {
            if let Some(&(canonical_fd, sync_type)) = self.named_sync.get(name.as_str()) {
                let handle = self.alloc_waitable_handle_for_client(client_fd);
                if handle != 0 {
                    let dup_fd = unsafe { libc::dup(canonical_fd) };
                    if dup_fd >= 0 {
                        let obj = Arc::new(crate::ntsync::NtsyncObj::from_raw_fd(dup_fd));
                        let state = obj.event_read().map(|(m,s)| format!("manual={m} signaled={s}")).unwrap_or("?".into());
                        log_info!("OpenEvent: name=\"{name}\" handle={handle:#x} pid={pid} [{state}] fd={client_fd}");
                        self.ntsync_objects.insert((pid, handle), (obj, sync_type));
                    } else if let Some(fallback) = self.get_or_create_event(sync_type == 4, false) {
                        self.ntsync_objects.insert((pid, handle), (fallback, sync_type));
                    }
                }
                let reply = OpenEventReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    handle,
                    _pad_0: [0; 4],
                };
                return reply_fixed(&reply);
            }
        }
        reply_fixed(&OpenEventReply {
            header: ReplyHeader { error: 0xc0000034, reply_size: 0 }, // STATUS_OBJECT_NAME_NOT_FOUND
            handle: 0,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_create_keyed_event(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            log_error!("create_keyed_event: handle=0! fd={client_fd}");
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        let pid = self.client_pid(client_fd as RawFd);
        if let Some(obj) = self.get_or_create_event(true, false) {
            self.ntsync_objects.insert((pid, handle), (obj, 1)); // INTERNAL (keyed event)
        }
        log_info!("create_keyed_event: handle={handle:#x} pid={pid} fd={client_fd}");
        reply_fixed(&CreateKeyedEventReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }


    // Sync object queries
    pub(crate) fn handle_open_mutex(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.client_pid(client_fd as RawFd);
        let name = extract_open_name(buf);
        if let Some(ref name) = name {
            if let Some(&(canonical_fd, sync_type)) = self.named_sync.get(name.as_str()) {
                let handle = self.alloc_waitable_handle_for_client(client_fd);
                if handle != 0 {
                    let dup_fd = unsafe { libc::dup(canonical_fd) };
                    if dup_fd >= 0 {
                        let obj = Arc::new(crate::ntsync::NtsyncObj::from_raw_fd(dup_fd));
                        self.ntsync_objects.insert((pid, handle), (obj, sync_type));
                    } else if let Some(fallback) = self.get_or_create_event(false, false) {
                        self.ntsync_objects.insert((pid, handle), (fallback, sync_type));
                    }
                }
                return reply_fixed(&OpenMutexReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    handle,
                    _pad_0: [0; 4],
                });
            }
        }
        reply_fixed(&OpenMutexReply {
            header: ReplyHeader { error: 0xc0000034, reply_size: 0 },
            handle: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_open_semaphore(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.client_pid(client_fd as RawFd);
        let name = extract_open_name(buf);
        if let Some(ref name) = name {
            if let Some(&(canonical_fd, sync_type)) = self.named_sync.get(name.as_str()) {
                let handle = self.alloc_waitable_handle_for_client(client_fd);
                if handle != 0 {
                    let dup_fd = unsafe { libc::dup(canonical_fd) };
                    if dup_fd >= 0 {
                        let obj = Arc::new(crate::ntsync::NtsyncObj::from_raw_fd(dup_fd));
                        self.ntsync_objects.insert((pid, handle), (obj, sync_type));
                    } else if let Some(fallback) = self.get_or_create_event(false, false) {
                        self.ntsync_objects.insert((pid, handle), (fallback, sync_type));
                    }
                }
                return reply_fixed(&OpenSemaphoreReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    handle,
                    _pad_0: [0; 4],
                });
            }
        }
        reply_fixed(&OpenSemaphoreReply {
            header: ReplyHeader { error: 0xc0000034, reply_size: 0 },
            handle: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_open_keyed_event(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let oid = self.state.alloc_object_id();
        let handle = self.alloc_handle_for_client(_client_fd, oid);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        reply_fixed(&OpenKeyedEventReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_query_event(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let (manual_reset, state) = if buf.len() >= std::mem::size_of::<QueryEventRequest>() {
            let req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const QueryEventRequest) };
            let pid = self.client_pid(client_fd as RawFd);
            self.ntsync_objects.get(&(pid, req.handle))
                .and_then(|(obj, _)| obj.event_read())
                .map(|(m, s)| (m as i32, s as i32))
                .unwrap_or((0, 0))
        } else { (0, 0) };
        reply_fixed(&QueryEventReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            manual_reset,
            state,
        })
    }

    pub(crate) fn handle_query_mutex(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let (count, owned) = if buf.len() >= std::mem::size_of::<QueryMutexRequest>() {
            let req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const QueryMutexRequest) };
            let pid = self.client_pid(client_fd as RawFd);
            self.ntsync_objects.get(&(pid, req.handle))
                .and_then(|(obj, _)| obj.mutex_read())
                .map(|(owner, count)| (count, if owner != 0 { 1i32 } else { 0 }))
                .unwrap_or((0, 0))
        } else { (0, 0) };
        reply_fixed(&QueryMutexReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            count,
            owned,
            abandoned: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_query_semaphore(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let (current, max) = if buf.len() >= std::mem::size_of::<QuerySemaphoreRequest>() {
            let req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const QuerySemaphoreRequest) };
            let pid = self.client_pid(client_fd as RawFd);
            self.ntsync_objects.get(&(pid, req.handle))
                .and_then(|(obj, _)| obj.sem_read())
                .unwrap_or((0, 1))
        } else { (0, 1) };
        reply_fixed(&QuerySemaphoreReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            current,
            max,
        })
    }


    // Async/IO operations
    pub(crate) fn handle_register_async(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() >= std::mem::size_of::<RegisterAsyncRequest>() {
            let _req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const RegisterAsyncRequest) };
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_cancel_async(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<CancelAsyncRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const CancelAsyncRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.client_pid(client_fd as RawFd);
        let _tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);

        // Cancel matching async operations:
        // - req.handle != 0: cancel asyncs on that file handle
        // - req.iosb != 0: cancel only the specific IOSB
        // - req.only_thread != 0: only cancel from the calling thread
        // - All zero: cancel everything for this process (fallback)
        if req.handle != 0 {
            // Look up the fd for this handle to match against pending_reads
            let target_fd = self.state.processes.get(&pid)
                .and_then(|p| p.handles.get(req.handle))
                .and_then(|e| e.fd);

            if let Some(tfd) = target_fd {
                self.pending_reads.retain(|(p, _), pr| {
                    if *p != pid { return true; }
                    if pr.fd != tfd { return true; }
                    false // cancel this one
                });
            }
        } else {
            // No handle specified — cancel all for process
            self.pending_reads.retain(|(p, _), _| *p != pid);
        }

        reply_fixed(&CancelAsyncReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            cancel_handle: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_get_async_result(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetAsyncResultRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetAsyncResultRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.client_pid(client_fd as RawFd);

        // Check completed ioctl operations (e.g., FSCTL_PIPE_LISTEN)
        if let Some(status) = self.completed_ioctls.remove(&(pid, req.user_arg)) {
            return reply_fixed(&GetAsyncResultReply {
                header: ReplyHeader { error: status, reply_size: 0 },
            });
        }

        // Check completed async pipe reads
        if let Some(pos) = self.completed_pipe_reads.iter().position(|cr| cr.pid == pid && cr.client_fd == client_fd as RawFd) {
            let cr = self.completed_pipe_reads.remove(pos);
            if cr.data.is_empty() {
                // EOF
                return reply_fixed(&GetAsyncResultReply {
                    header: ReplyHeader { error: 0xC000014B, reply_size: 0 }, // STATUS_PIPE_BROKEN
                });
            }
            return reply_vararg(&GetAsyncResultReply {
                header: ReplyHeader { error: 0, reply_size: cr.data.len() as u32 },
            }, &cr.data);
        }

        // Try live recv() on the pending read fd
        if let Some(pending) = self.pending_reads.get(&(pid, req.user_arg)) {
            let fd = pending.fd;
            let max = pending.max_bytes;
            let mut read_buf = vec![0u8; max];
            let n = unsafe { libc::recv(fd, read_buf.as_mut_ptr() as *mut libc::c_void, max, libc::MSG_DONTWAIT) };

            if n > 0 {
                read_buf.truncate(n as usize);
                self.pending_reads.remove(&(pid, req.user_arg));
                return reply_vararg(&GetAsyncResultReply {
                    header: ReplyHeader { error: 0, reply_size: n as u32 },
                }, &read_buf);
            }
        }

        reply_fixed(&ReplyHeader { error: 0x00000103, reply_size: 0 }) // STATUS_PENDING
    }

    pub(crate) fn handle_set_async_direct_result(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetAsyncDirectResultRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetAsyncDirectResultRequest) }
        } else {
            return reply_fixed(&SetAsyncDirectResultReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                handle: 0, _pad_0: [0; 4],
            });
        };
        let reply_handle = if req.mark_pending != 0 { req.handle } else { 0 };
        reply_fixed(&SetAsyncDirectResultReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: reply_handle, _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_cancel_sync(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    // Exception handling
    pub(crate) fn handle_queue_exception_event(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // No debugger attached for games. Return STATUS_PORT_NOT_SET.
        // ntdll dispatches through SEH chain normally on this error code.
        // Auto-stub returns error=0 which means "queued to debugger, wait" — WRONG.
        reply_fixed(&QueueExceptionEventReply {
            header: ReplyHeader { error: 0xC0000077, reply_size: 0 }, // STATUS_PORT_NOT_SET
            handle: 0,
            _pad_0: [0; 4],
        })
    }


    // APC
    pub(crate) fn handle_queue_apc(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Self-APCs: client checks is_self and executes locally without using the handle.
        // handle=0 is fine — the client never calls get_handle_fd or NtWait on it.
        reply_fixed(&QueueApcReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: 0,
            is_self: 1,
        })
    }

    pub(crate) fn handle_get_apc_result(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetApcResultReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            result: [0u8; 40],
        })
    }


    // ---- ntsync inproc sync fd ----

    pub(crate) fn handle_get_inproc_sync_fd(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetInprocSyncFdRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetInprocSyncFdRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.client_pid(client_fd as RawFd);
        let handle = req.handle;

        // Diagnostic: track which handles each process requests ntsync fds for.
        // The last one before silence is the handle the process is stuck waiting on.
        {
            let (type_name, state_str) = self.ntsync_objects.get(&(pid, handle))
                .map(|(obj, st)| {
                    let tn = match *st { 1 => "INT", 2 => "EVT", 3 => "MUT", 4 => "SEM", _ => "?" };
                    let ss = match *st {
                        1 | 2 => obj.event_read().map(|(m,s)| format!("manual={m} signaled={s}")).unwrap_or("?".into()),
                        3 => obj.mutex_read().map(|(o,c)| format!("owner={o} count={c}")).unwrap_or("?".into()),
                        4 => obj.sem_read().map(|(c,mx)| format!("count={c} max={mx}")).unwrap_or("?".into()),
                        _ => "?".into(),
                    };
                    (tn, ss)
                })
                .unwrap_or(("NONE", String::new()));
            log_info!("GET_INPROC_SYNC: pid={pid} handle={handle:#x} type={type_name} [{state_str}] fd={client_fd}");
        }

        // Look up the ntsync object for this handle
        if let Some((obj, sync_type)) = self.ntsync_objects.get(&(pid, handle)) {
            let ntsync_fd = obj.fd();
            let dup_fd = unsafe { libc::fcntl(ntsync_fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup_fd >= 0 {
                if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                    client.pending_fd = Some((dup_fd, handle));
                }
            } else {
                return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
            }
            let wine_type = *sync_type;
            let access = self.state.processes.get(&pid)
                .and_then(|p| p.handles.get(handle))
                .map(|e| e.access)
                .unwrap_or(0x001F0003);

            return reply_fixed(&GetInprocSyncFdReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                r#type: wine_type as i32,
                access,
            });
        }

        // Handle not in ntsync_objects — check if it's a process/thread handle
        // that needs an exit event. For WaitForSingleObject on process/thread handles,
        // Wine waits for the exit event which triskelion stores separately.
        if let Some(process) = self.state.processes.get(&pid) {
            if let Some(entry) = process.handles.get(handle) {
                // Check if there's an exit event for this handle's object
                let target_pid = entry.object_id as u32;
                // Look for process exit event
                if let Some(events) = self.process_exit_events.get(&target_pid) {
                    for (ppid, h, evt) in events {
                        if *ppid == pid && *h == handle {
                            let dup_fd = unsafe { libc::fcntl(evt.fd(), libc::F_DUPFD_CLOEXEC, 0) };
                            if dup_fd >= 0 {
                                if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                                    client.pending_fd = Some((dup_fd, handle));
                                }
                            }
                            return reply_fixed(&GetInprocSyncFdReply {
                                header: ReplyHeader { error: 0, reply_size: 0 },
                                r#type: 1,
                                access: entry.access,
                            });
                        }
                    }
                }
            }
        }

        // Handle not found — create on-demand event.
        // For pipe handles: UNSIGNALED, monitored by I/O thread for data arrival.
        // For other handles: SIGNALED (timeout waits complete, not block forever).
        let pipe_data_fd = self.side_tables.pipe_handles.get(&(pid, handle)).map(|p| p.data_fd);
        let initial_signaled = pipe_data_fd.is_none();
        if let Some(obj) = self.get_or_create_event(true, initial_signaled) {
            let ntsync_fd = obj.fd();
            let dup_fd = unsafe { libc::fcntl(ntsync_fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup_fd >= 0 {
                if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                    client.pending_fd = Some((dup_fd, handle));
                }
            }
            let access = self.state.processes.get(&pid)
                .and_then(|p| p.handles.get(handle))
                .map(|e| e.access)
                .unwrap_or(0x001F0003);
            // If pipe handle, set up I/O thread monitoring
            if let Some(pipe_fd) = pipe_data_fd {
                // Dup the ntsync fd for the I/O thread (it needs its own copy)
                let watch_ntsync = unsafe { libc::fcntl(ntsync_fd, libc::F_DUPFD_CLOEXEC, 0) };
                if watch_ntsync >= 0 {
                    self.pending_pipe_watches.push((pipe_fd, watch_ntsync));
                    log_info!("pipe_watch: queued fd={pipe_fd} ntsync={watch_ntsync} for pid={pid} handle={handle:#x}");
                }
            }
            self.ntsync_objects.insert((pid, handle), (obj, 1)); // INTERNAL
            return reply_fixed(&GetInprocSyncFdReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                r#type: 1,
                access,
            });
        }
        log_warn!("get_inproc_sync_fd: MISS handle={handle:#x} pid={pid} fd={client_fd}");
        // Catch-all will create a signaled event below
        reply_fixed(&GetInprocSyncFdReply {
            header: ReplyHeader { error: 0xC0000008, reply_size: 0 },
            r#type: 0,
            access: 0,
        })
    }

    pub(crate) fn handle_get_inproc_alert_fd(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        // Alert fd: per-thread event for cancelling blocking waits.
        // Stock: reply->handle = get_thread_id(current) | 1 (arbitrary token)
        //        send_client_fd(process, fd, reply->handle)
        // Client asserts: token == reply->handle
        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        let token = tid | 1;
        let alert_fd = self.get_or_create_alert(client_fd as RawFd);
        let dup_fd = unsafe { libc::fcntl(alert_fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup_fd >= 0 {
            if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                client.pending_fd = Some((dup_fd, token));
            }
        }
        reply_fixed(&GetInprocAlertFdReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: token,
            _pad_0: [0; 4],
        })
    }

}
