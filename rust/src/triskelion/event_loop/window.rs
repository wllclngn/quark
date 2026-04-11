// Window management, desktop, messaging, and input handling

use super::*;

impl EventLoop {

    /// Walk up parents to find first ancestor with PAINT_HAS_SURFACE (stock: get_top_clipping_window)
    fn get_top_clipping_window(&self, handle: u32) -> u32 {
        const PAINT_HAS_SURFACE: u16 = 0x01;
        let mut win = handle;
        loop {
            let ws = match self.window_states.get(&win) {
                Some(ws) => ws,
                None => return win,
            };
            if ws.paint_flags & PAINT_HAS_SURFACE != 0 { return win; }
            if ws.parent == 0 || ws.parent == self.desktop_top_window { return win; }
            win = ws.parent;
        }
    }

    pub(crate) fn handle_get_message(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetMessageRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetMessageRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let tid = self.client_thread_id(client_fd as RawFd);

        // Parse flags (stock: queue.c:3337)
        // Low 16 bits: PM_NOREMOVE=0, PM_REMOVE=1, PM_NOYIELD=2
        // High 16 bits: QS_* filter mask. 0 = QS_ALLINPUT
        const QS_POSTMESSAGE: u32 = 0x0008;
        const QS_PAINT: u32 = 0x0020;
        const QS_TIMER: u32 = 0x0010;
        const QS_ALLINPUT: u32 = 0x04FF;
        const QS_INPUT: u32 = 0x0407; // QS_MOUSEMOVE|QS_MOUSEBUTTON|QS_KEY|QS_RAWINPUT|QS_TOUCH|QS_POINTER
        const QS_HOTKEY: u32 = 0x0080;
        const QS_ALLPOSTMESSAGE: u32 = 0x0100;
        let filter = {
            let f = req.flags >> 16;
            if f == 0 { QS_ALLINPUT } else { f }
        };

        // Stock Wine queue.c:3339-3350 clears changed_bits BEFORE checking.
        // But stock also clears wake_bits when messages are consumed from
        // server-side queues (hardware queue, posted list, etc.). In our
        // architecture, input messages (QS_KEY, QS_MOUSEMOVE, QS_MOUSEBUTTON)
        // don't pass through server-side queues — they're delivered by the
        // client-side x11drv. So wake_bits for input get set by
        // send_hardware_message but never cleared by message consumption.
        //
        // Wine's check_queue_bits (message.c:2906): if wake_bits & signal_bits
        // is non-zero, the client ALWAYS calls the server → 20k+ req/sec spin.
        //
        // Fix: clear BOTH wake_bits AND changed_bits for filtered categories
        // before checking. When real events arrive, set_queue_bits_for_tid
        // re-sets them. This matches stock's semantic intent (clear stale
        // state before each get_message pass) even though stock achieves it
        // via different code paths.
        if let Some(tid_val) = tid {
            let locator = self.clients.values()
                .find(|c| c.thread_id == tid_val)
                .map(|c| c.queue_locator);
            if let Some(locator) = locator {
                let offset = u64::from_le_bytes([
                    locator[8], locator[9], locator[10], locator[11],
                    locator[12], locator[13], locator[14], locator[15],
                ]) as usize;
                if !self.session_map.is_null() && (offset + 1024) <= self.session_size {
                    unsafe {
                        let base = self.session_map.add(offset);
                        let seq_atomic = &*(base as *const std::sync::atomic::AtomicI64);
                        let shm = base.add(16);
                        let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                        seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                        // Stock queue.c:3339: ONLY clear changed_bits here, NOT wake_bits.
                        // Clearing wake_bits before checking kills signals the game needs
                        // during init (window creation, GL setup). wake_bits get cleared
                        // when messages are actually consumed from the queue.
                        let changed_bits_ptr = shm.add(20) as *mut u32;
                        if filter & QS_POSTMESSAGE != 0 {
                            let mask = QS_POSTMESSAGE | QS_HOTKEY | QS_TIMER | QS_ALLPOSTMESSAGE;
                            *changed_bits_ptr &= !mask;
                        }
                        if filter & QS_INPUT != 0 {
                            *changed_bits_ptr &= !QS_INPUT;
                        }
                        if filter & QS_PAINT != 0 {
                            *changed_bits_ptr &= !QS_PAINT;
                        }
                        seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                    }
                }
            }
        }

        // 0. Check cross-process tracked sent messages (highest priority, stock: queue.c:3348)
        // Sent messages are dispatched before posted messages in Wine's model.
        const QS_SENDMESSAGE_BIT: u32 = 0x0040;
        if filter & QS_SENDMESSAGE_BIT != 0 {
            if let Some(tid_val) = tid {
                if let Some(pending) = self.sent_messages.peek(tid_val) {
                    let reply = GetMessageReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        win: pending.win, msg: pending.msg_code,
                        wparam: pending.wparam, lparam: pending.lparam,
                        r#type: pending.msg_type, x: 0, y: 0, time: 0,
                        total: 0, _pad_0: [0; 4],
                    };
                    log_info!("get_message: tid={tid_val:#x} SENT win={:#06x} msg={:#06x}", pending.win, pending.msg_code);
                    return reply_fixed(&reply);
                }
            }
        }

        // 1. Check posted messages (stock: queue.c:3356)
        if filter & QS_POSTMESSAGE != 0 {
            if let Some(queue) = tid.and_then(|t| self.shm.get_queue(t)) {
                if let Some(msg) = queue.get() {
                    log_info!("get_message: tid={:#x} win={:#06x} msg={:#06x} wp={} lp={}", tid.unwrap_or(0), msg.win, msg.msg, msg.wparam, msg.lparam);
                    return reply_fixed(&GetMessageReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        win: msg.win, msg: msg.msg, wparam: msg.wparam, lparam: msg.lparam,
                        r#type: msg.msg_type, x: msg.x, y: msg.y, time: msg.time,
                        total: 0, _pad_0: [0; 4],
                    });
                }
            }
        }

        // 2. WM_QUIT (stock: queue.c:3370 — after posted, before paint)
        const WM_QUIT: u32 = 0x0012;
        if filter & QS_POSTMESSAGE != 0 {
            if let Some(tid_val) = tid {
                if let Some(&(exit_code, true)) = self.thread_quit_state.get(&tid_val) {
                    if req.flags & 1 != 0 { // PM_REMOVE
                        self.thread_quit_state.remove(&tid_val);
                    }
                    return reply_fixed(&GetMessageReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        win: 0, msg: WM_QUIT, wparam: exit_code as u64, lparam: 0,
                        r#type: MSG_POSTED, x: 0, y: 0, time: 0, total: 0, _pad_0: [0; 4],
                    });
                }
            }
        }

        // 3. Synthesize WM_PAINT (stock: queue.c:3375)
        // NOTE: Do NOT clear needs_paint here — BeginPaint/get_update_region does that.
        const WM_PAINT: u32 = 0x000F;
        if filter & QS_PAINT != 0 {
            if let Some(tid_val) = tid {
                let paint_win = self.window_states.iter()
                    .find(|(_, ws)| ws.tid == tid_val && ws.needs_paint)
                    .map(|(&h, _)| h);
                if let Some(win) = paint_win {
                    log_info!("get_message: tid={:#x} win={win:#06x} msg=WM_PAINT", tid.unwrap_or(0));
                    return reply_fixed(&GetMessageReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        win, msg: WM_PAINT, wparam: 0, lparam: 0,
                        r#type: MSG_POSTED, x: 0, y: 0, time: 0, total: 0, _pad_0: [0; 4],
                    });
                }
            }
        }

        // 4. Check expired timers (stock: queue.c:3388-3400)
        if filter & QS_TIMER != 0 {
            if let Some(tid_val) = tid {
                let has_expired = self.win_timers_expired.get(&tid_val)
                    .map_or(false, |t| !t.is_empty());
                if has_expired {
                    let expired = self.win_timers_expired.get_mut(&tid_val).unwrap();
                    let timer = expired.remove(0);
                    log_info!("get_message: tid={tid_val:#x} win={:#06x} msg=WM_TIMER id={}", timer.win, timer.id);
                    let reply = GetMessageReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        win: timer.win, msg: timer.msg,
                        wparam: timer.id, lparam: timer.lparam,
                        r#type: MSG_POSTED, x: 0, y: 0, time: 0, total: 0, _pad_0: [0; 4],
                    };
                    // If PM_REMOVE, restart the timer (stock: restart_timer in queue.c:1608)
                    if req.flags & 1 != 0 {
                        let restarted = super::WinTimer {
                            when: std::time::Instant::now() + std::time::Duration::from_millis(timer.rate_ms as u64),
                            rate_ms: timer.rate_ms,
                            win: timer.win,
                            msg: timer.msg,
                            id: timer.id,
                            lparam: timer.lparam,
                        };
                        self.win_timers_pending.entry(tid_val).or_default().push(restarted);
                    } else {
                        // PM_NOREMOVE: put it back so next peek sees it
                        self.win_timers_expired.entry(tid_val).or_default().insert(0, timer);
                    }
                    return reply_fixed(&reply);
                }
            }
        }

        // 5. Store wake/changed masks for the queue (stock: queue.c:3402-3408)
        // These control what events wake the thread from MsgWaitForMultipleObjects
        if let Some(client) = self.clients.get(&(client_fd as RawFd)) {
            if let Some(queue_loc) = {
                let c = client;
                if c.queue_locator != [0u8; 16] { Some(c.queue_locator) } else { None }
            } {
                // Write wake_mask and changed_mask to queue shared memory
                let offset = u64::from_le_bytes(queue_loc[8..16].try_into().unwrap());
                if !self.session_map.is_null() {
                    self.shared_write(offset, |shm| unsafe {
                        // queue_shm_t layout (from protocol.def):
                        //   +0: access_time (u64)
                        //   +8: wake_mask (u32)
                        //  +12: wake_bits (u32)   ← DO NOT WRITE HERE
                        //  +16: changed_mask (u32)
                        //  +20: changed_bits (u32)
                        //  +24: internal_bits (u32)
                        *(shm.add(8) as *mut u32) = req.wake_mask;
                        *(shm.add(16) as *mut u32) = req.changed_mask;
                    });
                }
            }
        }

        // No messages — return STATUS_PENDING (stock: queue.c:3402-3411).
        // Write wake_mask + changed_mask to SHM so the client can be woken
        // for the right events going forward, then signal/reset the sync
        // object based on queue status.
        //
        // changed_bits were already cleared at the TOP of this handler
        // (matching stock queue.c:3339-3350). wake_bits are NOT cleared here
        // — they represent "something arrived" and get cleared when the
        // message is actually consumed from the queue. The client's
        // check_queue_masks uses (changed_bits & changed_mask) which is now 0
        // for the categories we cleared, so the spin stops.
        if let Some(client) = self.clients.get(&(client_fd as RawFd)) {
            let locator = client.queue_locator;
            let offset = u64::from_le_bytes([
                locator[8], locator[9], locator[10], locator[11],
                locator[12], locator[13], locator[14], locator[15],
            ]) as usize;
            if !self.session_map.is_null() && (offset + 1024) <= self.session_size {
                unsafe {
                    let base = self.session_map.add(offset);
                    let seq_atomic = &*(base as *const std::sync::atomic::AtomicI64);
                    let shm = base.add(16);
                    let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                    seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                    *(shm.add(8) as *mut u32) = req.wake_mask;     // wake_mask
                    *(shm.add(16) as *mut u32) = req.changed_mask; // changed_mask
                    seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                }
            }
        }

        reply_fixed(&GetMessageReply {
            header: ReplyHeader { error: 0x103, reply_size: 0 },
            win: 0, msg: 0, wparam: 0, lparam: 0,
            r#type: 0, x: 0, y: 0, time: 0, total: 0, _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_get_queue_status(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
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


    pub(crate) fn handle_send_message(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SendMessageRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SendMessageRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Resolve target thread: req.id if nonzero, otherwise look up window's owner
        let target_tid = if req.id != 0 {
            req.id
        } else if req.win != 0 {
            self.window_states.get(&req.win).map(|ws| ws.tid).unwrap_or(0)
        } else {
            0
        };

        let is_blocking_send = matches!(req.r#type,
            MSG_OTHER_PROCESS | MSG_ASCII | MSG_UNICODE | MSG_CALLBACK);
        let sender_tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        const QS_SMRESULT: u32 = 0x8000;

        // Desktop window (tid=0) or no target: acknowledge immediately
        if target_tid == 0 {
            if is_blocking_send && sender_tid != 0 {
                self.set_queue_bits_for_tid(sender_tid, QS_SMRESULT);
            }
            return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
        }

        // Check if target thread is connected
        let target_exists = self.clients.values().any(|c| c.thread_id == target_tid);
        if !target_exists {
            if is_blocking_send && sender_tid != 0 {
                self.set_queue_bits_for_tid(sender_tid, QS_SMRESULT);
            }
            return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
        }

        // Resolve PIDs for the same-process vs cross-process fork
        let sender_pid = self.client_pid(client_fd as RawFd);
        let target_pid = self.clients.values()
            .find(|c| c.thread_id == target_tid)
            .map(|c| c.process_id)
            .unwrap_or(0);
        let same_process = sender_pid != 0 && sender_pid == target_pid;

        if let Some(queue) = self.shm.get_queue(target_tid) {
            // For same-process blocking sends, force MSG_UNICODE so Wine's
            // peek_message skips unpack_message(). We don't pack the vararg
            // data, so MSG_OTHER_PROCESS would cause Wine to unpack raw
            // pointers as packed structs → garbage → c0000005.
            // MSG_UNICODE tells Wine: use wparam/lparam directly (valid
            // same-process pointers).
            let wire_type = if same_process && req.r#type == 1 { // MSG_OTHER_PROCESS→MSG_UNICODE
                3i32
            } else {
                req.r#type
            };

            let msg = crate::queue::QueuedMessage {
                win: req.win,
                msg: req.msg,
                wparam: req.wparam,
                lparam: req.lparam,
                msg_type: wire_type,
                x: 0,
                y: 0,
                time: 0,
                _pad: [0; 2],
            };

            // Non-blocking: always ring buffer
            if req.r#type == MSG_POSTED || req.r#type == MSG_NOTIFY {
                if queue.post(msg) {
                    self.set_queue_bits_for_tid(target_tid, QS_POSTMESSAGE | QS_ALLPOSTMESSAGE);
                    return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
                }
            }

            // Blocking sent message: fork on same-process vs cross-process
            if same_process {
                // SAME-PROCESS TRACKED: sender blocks until receiver's
                // reply_message signals QS_SMRESULT. Required because
                // lparam often points to the sender's stack frame — if
                // we signal QS_SMRESULT immediately, the sender returns,
                // frees the stack, and the receiver reads garbage (c0000005).
                if queue.post(msg) {
                    self.set_queue_bits_for_tid(target_tid, QS_SENDMESSAGE);
                    if is_blocking_send && sender_tid != 0 {
                        self.sent_messages.track(crate::sent_messages::PendingSentMessage {
                            sender_tid,
                            receiver_tid: target_tid,
                            msg_code: req.msg,
                            win: req.win,
                            wparam: req.wparam,
                            lparam: req.lparam,
                            msg_type: req.r#type,
                        });
                    }
                    return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
                }
            } else {
                // CROSS-PROCESS: adaptive fast vs tracked routing.
                // Cold start = tracked (conservative, correct Wine semantics).
                // Warm start = learned from prior reply observations.
                //
                // Force fast-path for messages to daemon-owned windows (desktop).
                // Nobody runs a message loop for these, so tracked sends block forever.
                let is_daemon_owned = req.win == self.desktop_top_window
                    || req.win == self.desktop_msg_window;
                let use_fast = is_daemon_owned || self.sent_messages.should_fast_path(req.msg);
                if queue.post(msg) {
                    self.set_queue_bits_for_tid(target_tid, QS_SENDMESSAGE);
                    if is_blocking_send && sender_tid != 0 {
                        if use_fast {
                            // Promoted or daemon-owned: sender gets immediate QS_SMRESULT
                            self.set_queue_bits_for_tid(sender_tid, QS_SMRESULT);
                        } else {
                            // Tracked: sender blocks until reply_message
                            self.sent_messages.track(crate::sent_messages::PendingSentMessage {
                                sender_tid,
                                receiver_tid: target_tid,
                                msg_code: req.msg,
                                win: req.win,
                                wparam: req.wparam,
                                lparam: req.lparam,
                                msg_type: req.r#type,
                            });
                        }
                    }
                    return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
                }
            }
        }

        // No SHM queue or ring full — still signal sender for blocking sends
        if is_blocking_send && sender_tid != 0 {
            self.set_queue_bits_for_tid(sender_tid, QS_SMRESULT);
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_set_queue_fd(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetQueueFdRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetQueueFdRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Stock wineserver (queue.c): gets the unix fd from the handle, dups it,
        // and registers it for POLLIN events. When the fd fires (X11 events arrive),
        // the message queue wakes so get_message can process input.
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });

        let unix_fd = pid.and_then(|p| self.state.processes.get(&p))
            .and_then(|p| p.handles.get(req.handle))
            .and_then(|e| e.fd);

        if let Some(fd) = unix_fd {
            let dup_fd = unsafe { libc::dup(fd) };
            if dup_fd >= 0 {
                // Store on client
                if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                    if let Some(old) = client.queue_fd {
                        unsafe { libc::close(old); }
                    }
                    client.queue_fd = Some(dup_fd);
                }

                // Get SHM pointer for queue's internal_bits and ntsync event fd
                let client = self.clients.get(&(client_fd as RawFd));
                let shm_ptr = client.map(|c| {
                    let locator = c.queue_locator;
                    let offset = u64::from_le_bytes([
                        locator[8], locator[9], locator[10], locator[11],
                        locator[12], locator[13], locator[14], locator[15],
                    ]) as usize;
                    if !self.session_map.is_null() && (offset + 1024) <= self.session_size {
                        unsafe { self.session_map.add(offset) as usize }
                    } else { 0 }
                }).unwrap_or(0);

                let queue_handle = client.map(|c| c.queue_handle).unwrap_or(0);
                let pid = client.map(|c| c.process_id).unwrap_or(0);

                // Get ntsync event fd for waking the queue.
                // Create eagerly if it doesn't exist yet — GET_INPROC_SYNC may
                // not have been called yet, and the I/O thread needs a valid fd
                // to signal when X11 events arrive (QS_DRIVER).
                let ntsync_fd = if queue_handle != 0 && pid != 0 {
                    if let Some((obj, _)) = self.ntsync_objects.get(&(pid, queue_handle)) {
                        obj.fd()
                    } else if let Some(obj) = self.get_or_create_event(false, false) {
                        let fd = obj.fd();
                        self.ntsync_objects.insert((pid, queue_handle), (obj, 1));
                        log_info!("set_queue_fd: created queue ntsync event pid={pid} handle={queue_handle:#x} fd={fd}");
                        fd
                    } else { -1 }
                } else { -1 };

                // Emit effect for I/O thread to monitor this fd
                self.pending_queue_fd_watches.push((dup_fd, shm_ptr, queue_handle, ntsync_fd));

                // Set QS_DRIVER immediately — there may already be pending X11 events
                if shm_ptr != 0 {
                    const QS_DRIVER: u32 = 0x80000000;
                    unsafe {
                        let base = shm_ptr as *mut u8;
                        let seq_atomic = &*(base as *const std::sync::atomic::AtomicI64);
                        let shm = base.add(16);
                        let internal_bits_ptr = shm.add(24) as *mut u32;
                        let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                        seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                        *internal_bits_ptr |= QS_DRIVER;
                        seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                    }
                }

                // Wake the queue via ntsync so get_message unblocks.
                // Use QS_POSTMESSAGE (not QS_INPUT bits) -- there are no
                // input messages at init time and stale QS_INPUT bits cause
                // the client to call get_message in a tight loop.
                if let Some(tid) = self.clients.get(&(client_fd as RawFd)).map(|c| c.thread_id) {
                    self.set_queue_bits_for_tid(tid, 0x0008); // QS_POSTMESSAGE
                }

            } else {
                return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
            }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 });
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_set_queue_mask(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetQueueMaskRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetQueueMaskRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Update queue shared memory so the client's check_queue_masks() sees
        // the new wake_mask/changed_mask and stops calling the server in a loop.
        // queue_shm_t layout at shared_object offset+16:
        //   +0: access_time (u64)
        //   +8: wake_mask (u32)
        //  +12: wake_bits (u32)
        //  +16: changed_mask (u32)
        //  +20: changed_bits (u32)
        let queue_locator = self.clients.get(&(client_fd as RawFd))
            .map(|c| c.queue_locator);
        if let Some(locator) = queue_locator {
            let offset = u64::from_le_bytes([
                locator[8], locator[9], locator[10], locator[11],
                locator[12], locator[13], locator[14], locator[15],
            ]) as usize;
            if !self.session_map.is_null() && (offset + 1024) <= self.session_size {
                unsafe {
                    let shm = self.session_map.add(offset + 16); // skip seq + id
                    let mut ts: libc::timespec = std::mem::zeroed();
                    libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
                    let now = (ts.tv_sec as u64) * 10_000_000 + (ts.tv_nsec as u64) / 100;
                    let seq_atomic = &*(self.session_map.add(offset) as *const std::sync::atomic::AtomicI64);
                    let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                    seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                    *(shm as *mut u64) = now;             // access_time
                    *(shm.add(8) as *mut u32) = req.wake_mask;    // wake_mask
                    *(shm.add(16) as *mut u32) = req.changed_mask; // changed_mask
                    seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                }
            }
        }

        // Read back wake_bits and changed_bits from shared memory
        let (wb, cb) = if let Some(locator) = queue_locator {
            let offset = u64::from_le_bytes([
                locator[8], locator[9], locator[10], locator[11],
                locator[12], locator[13], locator[14], locator[15],
            ]) as usize;
            if !self.session_map.is_null() && (offset + 1024) <= self.session_size {
                unsafe {
                    let shm = self.session_map.add(offset + 16);
                    let wake_bits = *(shm.add(12) as *const u32);
                    let changed_bits = *(shm.add(20) as *const u32);
                    (wake_bits, changed_bits)
                }
            } else { (0, 0) }
        } else { (0, 0) };

        // Clear QS_DRIVER and re-arm queue_fd ONLY when poll_events is set
        // (stock: queue.c:3117). Clearing unconditionally was killing input --
        // x11drv's ProcessEvents never saw QS_DRIVER because set_queue_mask
        // cleared it on every message loop iteration before ProcessEvents fired.
        if req.poll_events != 0 {
            if let Some(locator) = self.clients.get(&(client_fd as RawFd)).map(|c| c.queue_locator) {
                let offset = u64::from_le_bytes([
                    locator[8], locator[9], locator[10], locator[11],
                    locator[12], locator[13], locator[14], locator[15],
                ]) as usize;
                if !self.session_map.is_null() && (offset + 1024) <= self.session_size {
                    unsafe {
                        let base = self.session_map.add(offset);
                        let seq_atomic = &*(base as *const std::sync::atomic::AtomicI64);
                        let shm = base.add(16);
                        let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                        seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                        let internal_bits_ptr = shm.add(24) as *mut u32;
                        *internal_bits_ptr &= !0x80000000u32; // QS_DRIVER
                        seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                    }
                }
            }
        }
        // Re-arm queue_fd polling (EPOLLONESHOT pattern)
        if req.poll_events != 0 {
            if let Some(queue_fd) = self.clients.get(&(client_fd as RawFd)).and_then(|c| c.queue_fd) {
                self.pending_queue_fd_rearms.push(queue_fd);
            }
        }

        let reply = SetQueueMaskReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            wake_bits: wb,
            changed_bits: cb,
        };
        reply_fixed(&reply)
    }


    // ---- Window classes ----

    pub(crate) fn handle_create_class(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        let name_u16 = vararg_to_u16(vararg);

        let req = if buf.len() >= std::mem::size_of::<CreateClassRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const CreateClassRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let atom = if req.atom != 0 {
            // Client provided an atom (e.g. system classes like 32769)
            req.atom
        } else if let Some(&existing) = self.state.atom_names.get(&name_u16) {
            existing
        } else {
            let atom = self.state.next_atom;
            self.state.next_atom += 1;
            self.state.atoms.insert(atom, (vararg.to_vec(), 1));
            // Store both original and lowercase so case-insensitive lookups work
            let lower: Vec<u16> = name_u16.iter().map(|&c| {
                if c < 128 { (c as u8 as char).to_lowercase().next().unwrap() as u16 } else { c }
            }).collect();
            self.state.atom_names.insert(name_u16, atom);
            self.state.atom_names.insert(lower, atom);
            atom
        };

        // Allocate a shared_object_t for this class in the session memfd.
        // The client reads class_shm_t from it (atom, style, names, etc).
        let locator = self.alloc_shared_object();

        // Write the complete class_shm_t into the shared object.
        // class_shm_t layout (at shared_object_t + 16):
        //   +0:  atom (u32)          +4:  style (u32)
        //   +8:  cls_extra (u32)     +12: win_extra (u32)
        //   +16: instance (u64)
        //   +24: name_offset (u32)   +28: name_len (u32)
        //   +32: name[MAX_ATOM_LEN]  (510 bytes of WCHAR)
        let class_offset = u64::from_le_bytes(locator[8..16].try_into().unwrap());
        let vararg_copy = vararg.to_vec();
        self.shared_write(class_offset, |shm| unsafe {
            *(shm as *mut u32) = atom;                           // atom (atom_t = u32)
            *(shm.add(4) as *mut u32) = req.style;              // style
            *(shm.add(8) as *mut u32) = req.cls_extra as u32;     // cls_extra
            *(shm.add(12) as *mut u32) = req.win_extra as u32;  // win_extra
            *(shm.add(16) as *mut u64) = req.instance;          // instance
            *(shm.add(24) as *mut u32) = req.name_offset;       // name_offset (in WCHARs)
            *(shm.add(28) as *mut u32) = vararg_copy.len() as u32; // name_len (in bytes)
            // Copy class name into name[MAX_ATOM_LEN] (max 510 bytes)
            let name_copy_len = vararg_copy.len().min(510);
            std::ptr::copy_nonoverlapping(vararg_copy.as_ptr(), shm.add(32), name_copy_len);
        });

        // Store locator for this atom so create_window can set window_shm_t.class
        self.class_locators.insert(atom, locator);
        // Store client_ptr so create_window can return it (Wine dereferences this!)
        let _pid = self.client_pid(client_fd as std::os::unix::io::RawFd);
        self.class_client_ptrs.insert(atom, req.client_ptr);
        self.class_win_extra.insert(atom, req.win_extra as i32);

        // Verify: read back the id from shared memory to confirm it's correct
        let class_offset = u64::from_le_bytes(locator[8..16].try_into().unwrap());
        let locator_id = u64::from_le_bytes(locator[0..8].try_into().unwrap());
        if !self.session_map.is_null() && (class_offset as usize + 16) <= self.session_size {
            let actual_id = unsafe {
                let base = self.session_map.add(class_offset as usize);
                std::ptr::read_volatile(base.add(8) as *const u64)
            };
            if actual_id != locator_id {
                log_error!("create_class: ID MISMATCH! locator_id={locator_id:#x} actual_id={actual_id:#x} offset={class_offset}");
            } else {
            }
        }

        let reply = CreateClassReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            locator,
            atom,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    // ---- Window management stubs ----

    pub(crate) fn handle_create_window(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        const DESKTOP_CLASS_ATOM: u32 = 32769; // 0x8001
        const NTUSER_OBJ_WINDOW: u16 = 0x01;
        const NTUSER_DPI_PER_MONITOR_AWARE: u32 = 0x12;

        let req = if buf.len() >= std::mem::size_of::<CreateWindowRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const CreateWindowRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Resolve atom: if req.atom is 0, the class name is in the VARARG (UTF-16LE).
        // wine_server_add_atom sends atom=0 + VARARG for string class names,
        // or atom=N for integer atoms (like #32769).
        let atom = if req.atom != 0 {
            req.atom
        } else {
            let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
            let name_u16 = vararg_to_u16(vararg);
            // Try exact match, then case-insensitive (class names are case-insensitive)
            self.state.atom_names.get(&name_u16).copied().unwrap_or_else(|| {
                let lower: Vec<u16> = name_u16.iter().map(|&c| {
                    if c < 128 { (c as u8 as char).to_lowercase().next().unwrap() as u16 } else { c }
                }).collect();
                self.state.atom_names.get(&lower).copied().unwrap_or(0)
            })
        };

        let pid = self.client_pid(client_fd as RawFd);
        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);

        // Capture the shared offset that alloc_user_handle will use for this window
        let window_shared_offset = self.next_shared_offset;

        // Allocate a user handle in Wine's format: (index << 1) + 0x0020
        let handle = self.alloc_user_handle(NTUSER_OBJ_WINDOW, tid, pid);

        // HWND_MESSAGE = 0xFFFFFFFD (-3 as u32, truncated from 64-bit HWND)
        let is_hwnd_message = req.parent == 0xFFFFFFFD;
        let reply_parent = if atom == DESKTOP_CLASS_ATOM {
            0 // desktop window has no parent
        } else if is_hwnd_message && self.desktop_msg_window != 0 {
            self.desktop_msg_window // message-only windows parent to msg window
        } else if req.parent == 0 && self.desktop_top_window != 0 {
            self.desktop_top_window // top-level windows parent to desktop
        } else {
            req.parent
        };

        // Compute dpi_context matching stock wineserver (window.c:2253-2264):
        // - Non-desktop parent: inherit parent's dpi_context
        // - Otherwise: use request's dpi_context, or default NTUSER_DPI_PER_MONITOR_AWARE
        let dpi_context = if reply_parent != 0 && reply_parent != self.desktop_top_window {
            self.window_states.get(&reply_parent)
                .map(|ws| ws.dpi_context)
                .unwrap_or(if req.dpi_context != 0 { req.dpi_context } else { NTUSER_DPI_PER_MONITOR_AWARE })
        } else if req.dpi_context != 0 {
            req.dpi_context
        } else {
            NTUSER_DPI_PER_MONITOR_AWARE
        };

        // Write window_shm_t (class + dpi_context) with seqlock protection.
        // If the class atom has no locator (e.g., system classes not registered via create_class),
        // allocate a dummy shared_object_t. A zeroed locator causes NULL deref in Wine's win32u.
        let class_loc = if let Some(loc) = self.class_locators.get(&atom) {
            *loc
        } else {
            let loc = self.alloc_shared_object();
            self.class_locators.insert(atom, loc);
            loc
        };
        self.set_window_shm(window_shared_offset, &class_loc, dpi_context);

        // Pre-set PAINT_HAS_SURFACE for visible top-level windows so the display
        // driver can attach a surface on the first set_window_pos. For windows created
        // hidden (no WS_VISIBLE), the client sets this later via paint_flags.
        const WS_VISIBLE: u32 = 0x10000000;
        const PAINT_HAS_SURFACE: u16 = 0x01;
        let is_toplevel = reply_parent == self.desktop_top_window && atom != DESKTOP_CLASS_ATOM;
        let is_desktop = reply_parent == 0 && atom == DESKTOP_CLASS_ATOM;
        let initial_paint_flags = if (is_toplevel || is_desktop) && (req.style & WS_VISIBLE != 0) {
            PAINT_HAS_SURFACE
        } else {
            0
        };

        // For top-level visible windows, initialize rects to monitor dimensions.
        // This gives the game a fullscreen window from the start. The X11 driver
        // uses the server-side rects to size the X window on first map.
        let initial_rect = if is_toplevel && (req.style & WS_VISIBLE != 0) && self.monitor_rect != [0u8; 16] {
            self.monitor_rect
        } else {
            [0u8; 16]
        };

        // Track server-side window state
        self.window_states.insert(handle, WindowState {
            style: req.style,
            ex_style: req.ex_style,
            is_unicode: 1,
            owner: req.owner,
            parent: reply_parent,
            tid,
            id: 0,
            instance: req.instance,
            user_data: 0,
            dpi_context,
            window_rect: initial_rect,
            client_rect: initial_rect,
            visible_rect: initial_rect,
            surface_rect: initial_rect,
            paint_flags: initial_paint_flags,
            needs_paint: false,
            window_text: Vec::new(),
            extra_bytes: vec![0u8; self.class_win_extra.get(&atom).copied().unwrap_or(0).max(0) as usize],
        });

        // Detect desktop window (atom 32769) and message window
        if atom == DESKTOP_CLASS_ATOM && self.desktop_top_window == 0 {
            self.desktop_top_window = handle;
            // Auto-create the HWND_MESSAGE parent window
            let msg_shared_offset = self.next_shared_offset;
            self.desktop_msg_window = self.alloc_user_handle(NTUSER_OBJ_WINDOW, tid, pid);
            self.set_window_shm(msg_shared_offset, &class_loc, NTUSER_DPI_PER_MONITOR_AWARE);
            self.window_states.insert(self.desktop_msg_window, WindowState {
                style: 0, ex_style: 0, is_unicode: 1, owner: 0,
                parent: 0, tid, id: 0, instance: 0, user_data: 0,
                dpi_context: NTUSER_DPI_PER_MONITOR_AWARE,
                window_rect: [0u8; 16], client_rect: [0u8; 16], visible_rect: [0u8; 16], surface_rect: [0u8; 16], paint_flags: 0,
                needs_paint: false, window_text: Vec::new(), extra_bytes: Vec::new(),
            });
        }

        // Return the client_ptr that was stored during create_class.
        // Wine dereferences this as a CLASS* — returning 0 causes NULL deref crash.
        // Stock wineserver returns STATUS_INVALID_HANDLE if the class isn't found.
        let class_ptr = self.class_client_ptrs.get(&atom).copied().unwrap_or(0);
        if class_ptr == 0 {
            let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
            let name_str = String::from_utf16_lossy(&vararg_to_u16(vararg));
            log_warn!("create_window: no class for atom={atom} handle={handle:#x} name=\"{name_str}\" req_atom={} — returning error", req.atom);
            return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }); // STATUS_INVALID_HANDLE
        }
        let win_extra = self.class_win_extra.get(&atom).copied().unwrap_or(0);


        log_info!("create_window: handle={handle:#06x} atom={atom} style={:#010x} req_parent={:#06x} parent={reply_parent:#06x} tid={tid:#x}", req.style, req.parent);

        let reply = CreateWindowReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            parent: reply_parent,
            owner: req.owner,
            extra: win_extra,
            class_ptr,
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_destroy_window(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<DestroyWindowRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const DestroyWindowRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let old_style = self.window_states.get(&req.handle).map(|ws| ws.style).unwrap_or(0);
        log_info!("destroy_window: handle={:#06x} style={old_style:#010x}", req.handle);
        // Reparent children to destroyed window's parent
        let parent = self.window_states.get(&req.handle).map(|ws| ws.parent).unwrap_or(0);
        for (_, child_ws) in self.window_states.iter_mut() {
            if child_ws.parent == req.handle { child_ws.parent = parent; }
        }
        // Clean up properties and clipboard listeners for this window
        self.window_properties.retain(|(wh, _), _| *wh != req.handle);
        self.clipboard_listeners.remove(&req.handle);
        self.window_states.remove(&req.handle);
        // Recycle handle index
        const FIRST_USER_HANDLE: u32 = 0x0020;
        if req.handle >= FIRST_USER_HANDLE {
            let index = (req.handle - FIRST_USER_HANDLE) >> 1;
            self.user_handle_free_list.push(index);
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_set_window_owner(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetWindowOwnerRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetWindowOwnerRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let prev_owner = if let Some(ws) = self.window_states.get_mut(&req.handle) {
            let old = ws.owner;
            ws.owner = req.owner;
            old
        } else { 0 };

        reply_fixed(&SetWindowOwnerReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            full_owner: req.owner,
            prev_owner,
        })
    }


    pub(crate) fn handle_get_window_info(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        if let Some(ws) = self.window_states.get(&req.handle) {
            // Wine 11.5 offset-based get: negative offsets = standard fields, positive = extra bytes
            let info: u64 = match req.offset {
                -16 => ws.style as u64,       // GWL_STYLE
                -20 => ws.ex_style as u64,    // GWL_EXSTYLE
                -12 => ws.id,                 // GWLP_ID
                -6  => ws.instance,           // GWLP_HINSTANCE
                -4  => ws.is_unicode as u64,  // GWLP_WNDPROC (returns is_unicode flag)
                -21 => ws.user_data,          // GWLP_USERDATA
                off if off >= 0 => {
                    // Extra bytes at positive offset
                    let off = off as usize;
                    let size = (req.size as usize).min(8);
                    if off + size <= ws.extra_bytes.len() {
                        let mut val = [0u8; 8];
                        val[..size].copy_from_slice(&ws.extra_bytes[off..off + size]);
                        u64::from_le_bytes(val)
                    } else {
                        0
                    }
                }
                _ => 0,
            };
            reply_fixed(&GetWindowInfoReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                last_active: req.handle,
                is_unicode: ws.is_unicode as i32,
                info,
            })
        } else {
            reply_fixed(&GetWindowInfoReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                last_active: 0,
                is_unicode: 1,
                info: 0,
            })
        }
    }


    pub(crate) fn handle_set_window_info(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetWindowInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetWindowInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let old_info: u64 = if let Some(ws) = self.window_states.get_mut(&req.handle) {
            match req.offset {
                -16 => { // GWL_STYLE
                    let old = ws.style as u64;
                    ws.style = req.new_info as u32;
                    ws.paint_flags |= 0x0040; // PAINT_NONCLIENT on style change
                    old
                }
                -20 => { // GWL_EXSTYLE
                    let old = ws.ex_style as u64;
                    ws.ex_style = req.new_info as u32;
                    ws.paint_flags |= 0x0040; // PAINT_NONCLIENT
                    old
                }
                -12 => { // GWLP_ID
                    let old = ws.id;
                    ws.id = req.new_info;
                    old
                }
                -6 => { // GWLP_HINSTANCE
                    let old = ws.instance;
                    ws.instance = req.new_info;
                    old
                }
                -4 => { // GWLP_WNDPROC (is_unicode)
                    let old = ws.is_unicode as u64;
                    ws.is_unicode = req.new_info as i16;
                    old
                }
                -21 => { // GWLP_USERDATA
                    let old = ws.user_data;
                    ws.user_data = req.new_info;
                    old
                }
                off if off >= 0 => {
                    let off = off as usize;
                    let size = (req.size as usize).min(8);
                    if off + size <= ws.extra_bytes.len() {
                        let mut old_val = [0u8; 8];
                        old_val[..size].copy_from_slice(&ws.extra_bytes[off..off + size]);
                        let new_bytes = req.new_info.to_le_bytes();
                        ws.extra_bytes[off..off + size].copy_from_slice(&new_bytes[..size]);
                        u64::from_le_bytes(old_val)
                    } else {
                        0
                    }
                }
                _ => 0,
            }
        } else {
            0
        };

        reply_fixed(&SetWindowInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            old_info,
        })
    }


    pub(crate) fn handle_set_window_pos(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetWindowPosRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetWindowPosRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Store rects and update paint_flags from client
        const PAINT_CLIENT_FLAGS: u16 = 0x07; // HAS_SURFACE | HAS_PIXEL_FORMAT | HAS_LAYERED_SURFACE
        let mut activate_tid: Option<u32> = None;
        let mut activate_hwnd: u32 = 0;
        if let Some(ws) = self.window_states.get_mut(&req.handle) {
            ws.window_rect = req.window;
            ws.client_rect = req.client;
            // visible_rect and surface_rect from VARARG extra rects (if present)
            let vararg = &buf[std::mem::size_of::<SetWindowPosRequest>()..];
            if vararg.len() >= 16 {
                ws.visible_rect.copy_from_slice(&vararg[0..16]);
            } else {
                ws.visible_rect = req.window;
            }
            if vararg.len() >= 32 {
                ws.surface_rect.copy_from_slice(&vararg[16..32]);
            } else {
                ws.surface_rect = req.window;
            }
            // Update paint_flags from client (stock: window.c:2721)
            ws.paint_flags = (ws.paint_flags & !PAINT_CLIENT_FLAGS) | (req.paint_flags & PAINT_CLIENT_FLAGS);

            // If the client set HAS_PIXEL_FORMAT but not HAS_SURFACE, and this is a
            // visible top-level window, force HAS_SURFACE. The Vulkan path
            // (vkCreateWin32SurfaceKHR -> set_window_pixel_format) sets PIXEL_FORMAT
            // but the GDI window_surface may not exist yet. Without HAS_SURFACE,
            // get_top_clipping_window stops at the wrong ancestor and surface_win
            // calculation breaks, preventing DXVK from presenting frames.
            if ws.paint_flags & 0x02 != 0 && ws.paint_flags & 0x01 == 0
                && ws.parent == self.desktop_top_window
                && ws.style & 0x10000000 != 0
            {
                ws.paint_flags |= 0x01;
            }

            // Apply SWP_SHOWWINDOW/SWP_HIDEWINDOW to style bits (stock: window.c:1959)
            const SWP_SHOWWINDOW: u16 = 0x0040;
            const SWP_HIDEWINDOW: u16 = 0x0080;
            const SWP_NOREDRAW: u16 = 0x0008;
            const WS_VISIBLE: u32 = 0x10000000;
            if req.swp_flags & SWP_SHOWWINDOW != 0 {
                ws.style |= WS_VISIBLE;
                // When a top-level window first becomes visible and monitor_rect
                // is known, override rects to fullscreen. SDL/LOVE2D creates
                // windows hidden then shows via SWP_SHOWWINDOW.
                if ws.parent == self.desktop_top_window
                    && req.handle != self.desktop_top_window
                    && self.monitor_rect != [0u8; 16]
                {
                    ws.window_rect = self.monitor_rect;
                    ws.client_rect = self.monitor_rect;
                    ws.visible_rect = self.monitor_rect;
                    ws.surface_rect = self.monitor_rect;
                }
                // Stash tid and handle for WM_ACTIVATEAPP post after borrow ends
                activate_tid = Some(ws.tid);
                activate_hwnd = req.handle as u32;
            }
            if req.swp_flags & SWP_HIDEWINDOW != 0 {
                ws.style &= !WS_VISIBLE;
            }
            let should_paint = ((req.swp_flags & SWP_SHOWWINDOW) != 0) ||
               ((req.swp_flags & SWP_NOREDRAW) == 0 && req.handle != self.desktop_top_window);
            if should_paint && !ws.needs_paint {
                ws.needs_paint = true;
            }
        }

        // Wake the queue so MsgWaitForMultipleObjects unblocks
        let wake_tid = self.window_states.get(&req.handle).and_then(|ws| {
            if ws.needs_paint { Some(ws.tid) } else { None }
        });
        if let Some(tid) = wake_tid {
            const QS_PAINT: u32 = 0x0020;
            self.set_queue_bits_for_tid(tid, QS_PAINT);
        }

        // Post WM_ACTIVATEAPP when a window first shows (SWP_SHOWWINDOW).
        // SDL checks this to decide whether to start rendering.
        if let Some(tid) = activate_tid {
            if tid != 0 {
                const WM_ACTIVATEAPP: u32 = 0x001C;
                if let Some(queue) = self.shm.get_queue(tid) {
                    let msg = crate::queue::QueuedMessage {
                        win: activate_hwnd,
                        msg: WM_ACTIVATEAPP,
                        wparam: 1, lparam: 0,
                        msg_type: MSG_POSTED,
                        x: 0, y: 0, time: 0, _pad: [0; 2],
                    };
                    queue.post(msg);
                    self.set_queue_bits_for_tid(tid, QS_POSTMESSAGE | QS_ALLPOSTMESSAGE);
                }
                // Update active/focus in shared memory
                if let Some(client) = self.clients.values().find(|c| c.thread_id == tid) {
                    let cfd = client.fd;
                    self.write_input_shm(cfd, 4, activate_hwnd); // active
                    self.write_input_shm(cfd, 8, activate_hwnd); // focus
                }
            }
        }

        let (new_style, new_ex_style) = self.window_states.get(&req.handle)
            .map(|ws| (ws.style, ws.ex_style))
            .unwrap_or((0, 0));

        // Return surface_win for top-level windows or windows with PAINT_HAS_SURFACE.
        // Check req.handle's parent directly (not top's parent, which was the Apr 6
        // regression). HWND_MESSAGE children (parent=0x22) are excluded -- x11drv
        // refuses to create X11 windows for them (winex11.drv/window.c:2838).
        let top = self.get_top_clipping_window(req.handle);
        let is_toplevel = self.window_states.get(&req.handle)
            .map(|ws| ws.parent == self.desktop_top_window)
            .unwrap_or(false);
        let has_surface = self.window_states.get(&top)
            .map(|w| w.paint_flags & 0x01 != 0).unwrap_or(false);
        let surface_win = if is_toplevel || has_surface { top } else { 0 };
        let _style = self.window_states.get(&req.handle).map(|ws| ws.style).unwrap_or(0);

        log_info!("set_window_pos: handle={:#06x} swp={:#06x} paint={:#04x} style={new_style:#010x} surface_win={surface_win:#06x}", req.handle, req.swp_flags, req.paint_flags);

        reply_fixed(&SetWindowPosReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            new_style,
            new_ex_style,
            surface_win,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_get_window_rectangles(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowRectanglesRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowRectanglesRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let (window, client) = self.window_states.get(&req.handle)
            .map(|ws| (ws.window_rect, ws.client_rect))
            .unwrap_or(([0u8; 16], [0u8; 16]));

        reply_fixed(&GetWindowRectanglesReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            window,
            client,
        })
    }


    pub(crate) fn handle_get_window_text(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowTextRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowTextRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let text = self.window_states.get(&req.handle)
            .map(|ws| ws.window_text.as_slice())
            .unwrap_or(&[]);

        if text.is_empty() {
            return reply_fixed(&GetWindowTextReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                length: 0,
                _pad_0: [0; 4],
            });
        }

        let max = max_reply_vararg(buf) as usize;
        let send_len = text.len().min(max);
        reply_vararg(&GetWindowTextReply {
            header: ReplyHeader { error: 0, reply_size: send_len as u32 },
            length: text.len() as u32,
            _pad_0: [0; 4],
        }, &text[..send_len])
    }


    pub(crate) fn handle_set_window_text(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetWindowTextRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetWindowTextRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let text = if buf.len() > VARARG_OFF { buf[VARARG_OFF..].to_vec() } else { Vec::new() };

        if let Some(ws) = self.window_states.get_mut(&req.handle) {
            ws.window_text = text;
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_get_windows_offset(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetWindowsOffsetReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            x: 0,
            y: 0,
            mirror: 0,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_get_visible_region(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetVisibleRegionRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetVisibleRegionRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // get_top_clipping_window: walk up parents to find surface owner
        let top_handle = self.get_top_clipping_window(req.window);

        // top_rect = surface_rect of the top clipping window
        let top_rect = self.window_states.get(&top_handle)
            .map(|ws| ws.surface_rect)
            .unwrap_or([0u8; 16]);

        // win_rect = client_rect (DCX_WINDOW=0) or window_rect (DCX_WINDOW=1)
        const DCX_WINDOW: u32 = 0x1;
        let win_rect = self.window_states.get(&req.window)
            .map(|ws| if req.flags & DCX_WINDOW != 0 { ws.window_rect } else { ws.client_rect })
            .unwrap_or([0u8; 16]);

        // paint_flags from the requesting window
        let paint_flags = self.window_states.get(&req.window)
            .map(|ws| ws.paint_flags as u32)
            .unwrap_or(0);

        // Region data: one RECT covering the window
        let region_rect = win_rect;
        let max_vararg = max_reply_vararg(buf) as usize;
        if max_vararg >= 16 {
            reply_vararg(&GetVisibleRegionReply {
                header: ReplyHeader { error: 0, reply_size: 16 },
                top_win: top_handle,
                top_rect,
                win_rect,
                paint_flags,
                total_size: 16,
                _pad_0: [0; 4],
            }, &region_rect)
        } else {
            reply_fixed(&GetVisibleRegionReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                top_win: top_handle,
                top_rect,
                win_rect,
                paint_flags,
                total_size: 16,
                _pad_0: [0; 4],
            })
        }
    }


    pub(crate) fn handle_get_desktop_window(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let _force = if buf.len() >= std::mem::size_of::<GetDesktopWindowRequest>() {
            let req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetDesktopWindowRequest) };
            req.force
        } else { 0 };

        // Always return the pre-created desktop handles immediately.
        // The GUID property and GraphicsDriver registry key are pre-set at daemon startup,
        // so load_desktop_driver can find the driver without waiting for explorer.
        // Explorer is spawned by wineboot (not by get_desktop_window returning 0).
        // Returning 0 here caused init_display_driver (called during DLL init under
        // loader lock) to fail — get_desktop_window can't spawn explorer under loader
        // lock, so it returned NULL, load_desktop_driver(NULL) failed, and null_user_driver
        // was installed permanently.

        reply_fixed(&GetDesktopWindowReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            top_window: self.desktop_top_window,
            msg_window: self.desktop_msg_window,
        })
    }


    pub(crate) fn handle_set_parent(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetParentRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetParentRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Validate new parent exists (0 = no parent is valid, desktop windows are valid)
        if req.parent != 0 && req.parent != self.desktop_top_window && req.parent != self.desktop_msg_window
            && !self.window_states.contains_key(&req.parent) {
            return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }); // STATUS_INVALID_HANDLE
        }
        let old_parent = if let Some(ws) = self.window_states.get_mut(&req.handle) {
            let old = ws.parent;
            ws.parent = req.parent;
            old
        } else { 0 };

        reply_fixed(&SetParentReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            old_parent,
            full_parent: req.parent,
        })
    }


    pub(crate) fn handle_get_window_children_from_point(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowChildrenFromPointRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowChildrenFromPointRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Walk from parent down through children, finding windows that contain the point.
        // Returns the chain: parent → child → grandchild → ... → deepest window containing (x,y).
        let parent = if req.parent != 0 { req.parent } else { self.desktop_top_window };
        let mut chain: Vec<u32> = Vec::new();
        self.child_from_point(parent, req.x, req.y, &mut chain);

        let max = max_reply_vararg(buf) as usize;
        let vararg: Vec<u8> = chain.iter().flat_map(|h| h.to_le_bytes()).collect();
        let send_len = vararg.len().min(max);
        let count = send_len / 4;

        if count == 0 {
            return reply_fixed(&GetWindowChildrenFromPointReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                count: 0,
                _pad_0: [0; 4],
            });
        }


        let reply = GetWindowChildrenFromPointReply {
            header: ReplyHeader { error: 0, reply_size: send_len as u32 },
            count: count as i32,
            _pad_0: [0; 4],
        };
        reply_vararg(&reply, &vararg[..send_len])
    }

    /// Find the chain of child windows containing the given point.
    /// Window rects are stored as [left, top, right, bottom] in LE i32 bytes.
    fn child_from_point(&self, parent: u32, x: i32, y: i32, chain: &mut Vec<u32>) {
        for (&h, ws) in &self.window_states {
            if ws.parent != parent || h == parent { continue; }
            // Parse window_rect: [left(4), top(4), right(4), bottom(4)]
            let left = i32::from_le_bytes([ws.window_rect[0], ws.window_rect[1], ws.window_rect[2], ws.window_rect[3]]);
            let top = i32::from_le_bytes([ws.window_rect[4], ws.window_rect[5], ws.window_rect[6], ws.window_rect[7]]);
            let right = i32::from_le_bytes([ws.window_rect[8], ws.window_rect[9], ws.window_rect[10], ws.window_rect[11]]);
            let bottom = i32::from_le_bytes([ws.window_rect[12], ws.window_rect[13], ws.window_rect[14], ws.window_rect[15]]);

            if x >= left && x < right && y >= top && y < bottom {
                chain.push(h);
                // Recurse into this child with coords relative to client rect
                let cl = i32::from_le_bytes([ws.client_rect[0], ws.client_rect[1], ws.client_rect[2], ws.client_rect[3]]);
                let ct = i32::from_le_bytes([ws.client_rect[4], ws.client_rect[5], ws.client_rect[6], ws.client_rect[7]]);
                self.child_from_point(h, x - cl, y - ct, chain);
                return; // Stock wine returns deepest match, not all siblings
            }
        }
    }


    pub(crate) fn handle_get_thread_desktop(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Build obj_locator: { id: u64, offset: u64 } pointing to desktop shared_object_t in session memfd
        let mut locator = [0u8; 16];
        locator[0..8].copy_from_slice(&self.desktop_locator_id.to_le_bytes());
        locator[8..16].copy_from_slice(&self.desktop_offset.to_le_bytes());
        let reply = GetThreadDesktopReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            locator,
            handle: 4, // desktop handle
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_set_thread_desktop(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let mut locator = [0u8; 16];
        locator[0..8].copy_from_slice(&self.desktop_locator_id.to_le_bytes());
        locator[8..16].copy_from_slice(&self.desktop_offset.to_le_bytes());
        let reply = SetThreadDesktopReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            locator,
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_process_winstation(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        // Return the process's own winstation handle, not a global one.
        // Services.exe sets its winstation to __wineservice_winstation.
        // The game should get the DEFAULT winstation (WinSta0).
        // If the process never called set_process_winstation, return the
        // default handle allocated at startup (not the service one).
        let pid = self.client_pid(client_fd as RawFd);
        let handle = self.process_winstations.get(&pid).copied()
            .unwrap_or(self.default_winstation_handle);
        let reply = GetProcessWinstationReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_set_process_winstation(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetProcessWinstationRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetProcessWinstationRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        if pid != 0 {
            self.process_winstations.insert(pid, req.handle);
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_create_desktop(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let oid = self.state.alloc_object_id();
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let handle = if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.handles.allocate(oid)
            } else { 4 }
        } else { 4 };

        // Reassign desktop window to explorer's thread/process.
        // Desktop was pre-created at startup with tid=0/pid=0.
        // Now explorer owns it — display driver init happens under explorer's identity.
        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        let explorer_pid = self.client_pid(client_fd as RawFd);
        if let Some(ws) = self.window_states.get_mut(&self.desktop_top_window) {
            ws.tid = tid;
        }
        // Update user_entry in session shm so client sees explorer's tid/pid
        if self.desktop_top_window != 0 {
            const FIRST_USER_HANDLE: u32 = 0x0020;
            let index = (self.desktop_top_window - FIRST_USER_HANDLE) >> 1;
            let entry_offset = (index as usize) * 32;
            if self.session_fd >= 0 {
                let tid_bytes = tid.to_le_bytes();
                let pid_bytes = explorer_pid.to_le_bytes();
                unsafe {
                    libc::pwrite(self.session_fd, tid_bytes.as_ptr() as *const _, 4, (entry_offset + 8) as i64);
                    libc::pwrite(self.session_fd, pid_bytes.as_ptr() as *const _, 4, (entry_offset + 12) as i64);
                }
            }
        }
        // Signal desktop ready — get_desktop_window(force=0) will now return handles.
        self.desktop_ready = true;

        // Atomic signal via SHM header — launcher polls this, no file I/O.
        self.shm.set_desktop_ready();

        // Note: __wine_display_device_guid is pre-set at daemon startup (mod.rs)
        // with a deterministic null GUID. No timing dependency on explorer.


        reply_fixed(&CreateDesktopReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_get_thread_input(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req_tid = if buf.len() >= std::mem::size_of::<GetThreadInputRequest>() {
            let req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetThreadInputRequest) };
            req.tid
        } else { 0 };

        // tid=0 means foreground input; use the calling thread's input
        let mut locator = if req_tid == 0 {
            self.clients.get(&(client_fd as RawFd))
                .map(|c| c.input_locator)
                .unwrap_or([0u8; 16])
        } else {
            self.clients.values()
                .find(|c| c.thread_id == req_tid)
                .map(|c| c.input_locator)
                .unwrap_or_else(|| {
                    self.clients.get(&(client_fd as RawFd))
                        .map(|c| c.input_locator)
                        .unwrap_or([0u8; 16])
                })
        };

        // Ensure input locator is never zero — allocate on demand if needed
        if locator == [0u8; 16] {
            let pid = self.clients.get(&(client_fd as RawFd)).map(|c| c.process_id).unwrap_or(0);
            let existing = self.clients.values()
                .find(|c| c.process_id == pid && c.input_locator != [0u8; 16])
                .map(|c| c.input_locator);
            locator = if let Some(loc) = existing {
                if let Some(c) = self.clients.get_mut(&(client_fd as RawFd)) {
                    c.input_locator = loc;
                }
                loc
            } else {
                let i = self.alloc_shared_object();
                let input_offset = u64::from_le_bytes(i[8..16].try_into().unwrap());
                self.shared_write(input_offset, |shm| unsafe {
                    *(shm as *mut i32) = 1; // foreground = 1
                });
                if let Some(c) = self.clients.get_mut(&(client_fd as RawFd)) {
                    c.input_locator = i;
                }
                i
            };
        }

        reply_fixed(&GetThreadInputReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            locator,
        })
    }


    pub(crate) fn handle_get_cursor_history(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_get_rawinput_buffer(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetRawinputBufferReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            next_size: 0,
            time: 0,
            count: 0,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_set_cursor(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetCursorRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetCursorRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Save previous state
        let prev_handle = self.cursor_handle;
        let prev_count = self.cursor_count;
        let prev_x = self.cursor_x;
        let prev_y = self.cursor_y;

        // Apply changes based on flags (stock: queue.c:4011)
        const SET_CURSOR_HANDLE: u32 = 0x01;
        const SET_CURSOR_COUNT: u32 = 0x02;
        const SET_CURSOR_POS: u32 = 0x04;
        const SET_CURSOR_CLIP: u32 = 0x08;
        const SET_CURSOR_NOCLIP: u32 = 0x10;

        if req.flags & SET_CURSOR_HANDLE != 0 {
            self.cursor_handle = req.handle;
        }
        if req.flags & SET_CURSOR_COUNT != 0 {
            self.cursor_count += req.show_count;
        }
        if req.flags & SET_CURSOR_POS != 0 {
            self.cursor_x = req.x;
            self.cursor_y = req.y;
            self.cursor_last_change = self.cursor_last_change.wrapping_add(1);
        }
        if req.flags & SET_CURSOR_CLIP != 0 {
            self.cursor_clip = req.clip;
        }
        if req.flags & SET_CURSOR_NOCLIP != 0 {
            self.cursor_clip = [0u8; 16];
        }

        reply_fixed(&SetCursorReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            prev_handle,
            prev_count,
            prev_x,
            prev_y,
            new_x: self.cursor_x,
            new_y: self.cursor_y,
            new_clip: self.cursor_clip,
            last_change: self.cursor_last_change,
            _pad_0: [0; 4],
        })
    }


    /// Read a u32 from the caller's input_shm_t at the given byte offset.
    fn read_input_shm(&self, client_fd: i32, offset: usize) -> u32 {
        let locator = self.clients.get(&(client_fd as RawFd))
            .map(|c| c.input_locator).unwrap_or([0u8; 16]);
        let shm_offset = u64::from_le_bytes(locator[8..16].try_into().unwrap_or([0;8])) as usize;
        if self.session_map.is_null() || shm_offset == 0 { return 0; }
        // input_shm_t starts at shared_object_t + 16 (after seq:i64 + id:u64)
        let addr = shm_offset + 16 + offset;
        if addr + 4 > self.session_size { return 0; }
        unsafe { *(self.session_map.add(addr) as *const u32) }
    }

    /// Write a u32 to the caller's input_shm_t at the given byte offset (with seqlock).
    fn write_input_shm(&self, client_fd: i32, offset: usize, value: u32) {
        let locator = self.clients.get(&(client_fd as RawFd))
            .map(|c| c.input_locator).unwrap_or([0u8; 16]);
        let shm_offset = u64::from_le_bytes(locator[8..16].try_into().unwrap_or([0;8]));
        if self.session_map.is_null() || shm_offset == 0 { return; }
        self.shared_write(shm_offset, |shm| unsafe {
            // input_shm_t is inside shared_object_t.shm (offset 16 from shared_object_t base)
            *(shm.add(offset) as *mut u32) = value;
        });
    }

    pub(crate) fn handle_set_foreground_window(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<SetForegroundWindowRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetForegroundWindowRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Read previous active from current foreground input (stock: queue.c:3840)
        let previous = self.read_input_shm(client_fd, 4);

        // Update foreground flag
        self.write_input_shm(client_fd, 0, 1); // foreground = 1

        // Post WM_ACTIVATEAPP (0x001C) with wparam=1 to all windows owned by
        // the new foreground thread. SDL checks this to decide whether to render.
        // Stock wineserver triggers this client-side via set_active_window, but
        // our simplified path needs to post it explicitly.
        const WM_ACTIVATEAPP: u32 = 0x001C;
        let caller_tid = self.clients.get(&(client_fd as RawFd))
            .map(|c| c.thread_id).unwrap_or(0);
        if caller_tid != 0 {
            // Collect windows owned by this thread
            let thread_windows: Vec<u32> = self.window_states.iter()
                .filter(|(_, ws)| ws.tid == caller_tid)
                .map(|(&h, _)| h)
                .collect();
            // Post WM_ACTIVATEAPP to each
            if let Some(queue) = self.shm.get_queue(caller_tid) {
                for hwnd in &thread_windows {
                    let msg = crate::queue::QueuedMessage {
                        win: *hwnd,
                        msg: WM_ACTIVATEAPP,
                        wparam: 1, // activating
                        lparam: 0,
                        msg_type: MSG_POSTED,
                        x: 0, y: 0, time: 0, _pad: [0; 2],
                    };
                    queue.post(msg);
                }
                if !thread_windows.is_empty() {
                    self.set_queue_bits_for_tid(caller_tid, QS_POSTMESSAGE | QS_ALLPOSTMESSAGE);
                }
            }
        }

        reply_fixed(&SetForegroundWindowReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            previous,
            send_msg_old: 0,
            send_msg_new: 0,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_set_focus_window(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetFocusWindowRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetFocusWindowRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Read previous focus (offset 8), write new (stock: queue.c:3860)
        let previous = self.read_input_shm(client_fd, 8);
        self.write_input_shm(client_fd, 8, req.handle);

        reply_fixed(&SetFocusWindowReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            previous,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_set_active_window(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetActiveWindowRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetActiveWindowRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Read previous active (offset 4), write new (stock: queue.c:3882)
        let previous = self.read_input_shm(client_fd, 4);
        self.write_input_shm(client_fd, 4, req.handle);

        reply_fixed(&SetActiveWindowReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            previous,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_set_capture_window(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetCaptureWindowRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetCaptureWindowRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        const CAPTURE_MENU: u32 = 0x01;
        const CAPTURE_MOVESIZE: u32 = 0x02;

        // Read previous capture (offset 12), write new (stock: queue.c:3910)
        let previous = self.read_input_shm(client_fd, 12);
        self.write_input_shm(client_fd, 12, req.handle); // capture
        self.write_input_shm(client_fd, 16,
            if req.flags & CAPTURE_MENU != 0 { req.handle } else { 0 }); // menu_owner
        self.write_input_shm(client_fd, 20,
            if req.flags & CAPTURE_MOVESIZE != 0 { req.handle } else { 0 }); // move_size

        reply_fixed(&SetCaptureWindowReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            previous,
            full_handle: req.handle,
        })
    }


    pub(crate) fn handle_set_user_object_info(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetUserObjectInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetUserObjectInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Resolve handle → object_id → winstation name
        let pid = self.client_pid(_client_fd as std::os::unix::io::RawFd);
        let object_id = if pid != 0 {
            self.state.processes.get(&pid)
                .and_then(|p| p.handles.get(req.handle))
                .map(|e| e.object_id as u32)
        } else {
            None
        };

        // Look up the winstation name for this object
        let name_bytes = object_id.and_then(|oid| self.winstation_names.get(&oid));

        if let Some(name) = name_bytes {
            let name = name.clone();
            let max_vararg = max_reply_vararg(buf) as usize;
            let send_len = name.len().min(max_vararg);
            reply_vararg(
                &SetUserObjectInfoReply {
                    header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                    is_desktop: 0, // winstation, not desktop
                    old_obj_flags: 0,
                },
                &name[..send_len],
            )
        } else {
            let default_name: Vec<u8> = "WinSta0".encode_utf16()
                .flat_map(|c| c.to_le_bytes()).collect();
            let max_vararg = max_reply_vararg(buf) as usize;
            let send_len = default_name.len().min(max_vararg);
            reply_vararg(&SetUserObjectInfoReply {
                header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                is_desktop: 0,
                old_obj_flags: 0,
            }, &default_name[..send_len])
        }
    }


    pub(crate) fn handle_attach_thread_input(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_get_key_state(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let vk = if buf.len() >= std::mem::size_of::<GetKeyStateRequest>() {
            let req: GetKeyStateRequest = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const _) };
            (req.key & 0xFF) as u8
        } else { 0 };
        reply_fixed(&GetKeyStateReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            state: self.read_desktop_keystate(vk),
            _pad_0: [0; 7],
        })
    }


    pub(crate) fn handle_set_key_state(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_update_rawinput_devices(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_set_caret_window(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetCaretWindowRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetCaretWindowRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        // Return previous caret state for this thread
        let (previous, old_rect, old_hide, old_state) = self.caret_state.get(&tid)
            .map(|(w, r, h, s)| (*w, *r, *h, *s))
            .unwrap_or((0, [0u8; 16], 0, 0));
        // Set new caret: window + rect from width/height, hide_count=0, state=0
        if req.handle != 0 {
            let mut rect = [0u8; 16]; // left=0, top=0, right=width, bottom=height
            rect[8..12].copy_from_slice(&req.width.to_le_bytes());
            rect[12..16].copy_from_slice(&req.height.to_le_bytes());
            self.caret_state.insert(tid, (req.handle, rect, 0, 0));
        } else {
            self.caret_state.remove(&tid);
        }
        reply_fixed(&SetCaretWindowReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            previous,
            old_rect,
            old_hide,
            old_state,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_set_caret_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetCaretInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetCaretInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);

        // caret_state: (window_handle, rect, hide_count, state)
        const SET_CARET_POS: u32 = 0x01;
        const SET_CARET_HIDE: u32 = 0x02;
        const SET_CARET_STATE: u32 = 0x04;

        let (old_handle, old_rect, old_hide, old_state) =
            self.caret_state.get(&tid).copied().unwrap_or((0, [0u8; 16], 0, 0));

        let mut new_rect = old_rect;
        let mut new_hide = old_hide;
        let mut new_state = old_state;

        if req.flags & SET_CARET_POS != 0 {
            // Build rect from x,y (width/height preserved from old rect)
            let old_w = i32::from_le_bytes(old_rect[8..12].try_into().unwrap_or([0;4]))
                      - i32::from_le_bytes(old_rect[0..4].try_into().unwrap_or([0;4]));
            let old_h = i32::from_le_bytes(old_rect[12..16].try_into().unwrap_or([0;4]))
                      - i32::from_le_bytes(old_rect[4..8].try_into().unwrap_or([0;4]));
            new_rect[0..4].copy_from_slice(&req.x.to_le_bytes());
            new_rect[4..8].copy_from_slice(&req.y.to_le_bytes());
            new_rect[8..12].copy_from_slice(&(req.x + old_w).to_le_bytes());
            new_rect[12..16].copy_from_slice(&(req.y + old_h).to_le_bytes());
        }
        if req.flags & SET_CARET_HIDE != 0 {
            new_hide += req.hide;
        }
        if req.flags & SET_CARET_STATE != 0 {
            new_state = req.state;
        }

        self.caret_state.insert(tid, (req.handle, new_rect, new_hide, new_state));

        reply_fixed(&SetCaretInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            full_handle: old_handle,
            old_rect,
            old_hide,
            old_state,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_reply_message(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<ReplyMessageRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const ReplyMessageRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
        };

        let receiver_tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        if receiver_tid == 0 {
            return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
        }

        // Wake the most recent sender that was tracked-blocking on this receiver.
        // Observe the reply for adaptive routing (zero = fast-path safe, nonzero = needs tracking).
        if let Some((sender_tid, msg_code)) = self.sent_messages.drain_one_with_code(receiver_tid) {
            self.sent_messages.observe_reply(msg_code, req.result);
            const QS_SMRESULT: u32 = 0x8000;
            self.set_queue_bits_for_tid(sender_tid, QS_SMRESULT);
            log_info!("reply_message: receiver={receiver_tid:#x} -> sender={sender_tid:#x} msg={msg_code:#x} result={}", req.result);
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_update_window_zorder(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Stock: walks siblings to find obscuring windows and reorders.
        // We don't track sibling z-order yet, so this is a no-op.
        // Balatro calls this 2000+ times per session; must return success.
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_accept_hardware_message(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_post_quit_message(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<PostQuitMessageRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const PostQuitMessageRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        if let Some(tid) = self.client_thread_id(client_fd as RawFd) {
            self.thread_quit_state.insert(tid, (req.exit_code, true));
            self.set_queue_bits_for_tid(tid, 0x0008); // QS_POSTMESSAGE — wake message loop
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_get_message_reply(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        // Sender consumed its reply — clear QS_SMRESULT so the next phantom
        // wake doesn't dispatch a stale callback (root cause of c0000005 in
        // Wine's session SHM cleanup path).
        const QS_SMRESULT: u32 = 0x8000;
        if let Some(tid) = self.client_thread_id(client_fd as RawFd) {
            self.clear_queue_bits_for_tid(tid, QS_SMRESULT);
        }
        reply_fixed(&GetMessageReplyReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            result: 0,
        })
    }


    pub(crate) fn handle_set_win_timer(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetWinTimerRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetWinTimerRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let tid = self.client_thread_id(client_fd as RawFd);
        let tid_val = match tid {
            Some(t) if t != 0 => t,
            _ => return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }),
        };

        // Resolve target thread: if req.win is set, use that window's thread
        let target_tid = if req.win != 0 {
            if let Some(ws) = self.window_states.get(&req.win) {
                ws.tid
            } else {
                return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }); // STATUS_INVALID_HANDLE
            }
        } else {
            tid_val
        };

        let mut id = req.id;

        // Remove existing timer with same (win, msg, id) — stock: replacement semantics
        for list in [&mut self.win_timers_pending, &mut self.win_timers_expired] {
            if let Some(timers) = list.get_mut(&target_tid) {
                timers.retain(|t| !(t.win == req.win && t.msg == req.msg && t.id == id));
            }
        }

        // Auto-assign ID for thread timers (win==0) — stock: queue.c:3513-3527
        if req.win == 0 && id == 0 {
            let next = self.next_timer_ids.entry(target_tid).or_insert(0x7fff);
            id = *next;
            *next -= 1;
            if *next <= 0x100 { *next = 0x7fff; }
        }

        // Create timer — stock: set_timer() in queue.c:1636
        // USER_TIMER_MINIMUM is 10ms in stock Wine (winuser.h). Games that ask
        // for 1ms timers (or 0ms) get clamped to 10ms — without this, a
        // run-away SetTimer(rate=1) burns the CPU at 1000 fires/sec.
        const USER_TIMER_MINIMUM: u32 = 10;
        let rate = std::cmp::max(req.rate, USER_TIMER_MINIMUM);
        let timer = super::WinTimer {
            when: std::time::Instant::now() + std::time::Duration::from_millis(rate as u64),
            rate_ms: rate,
            win: req.win,
            msg: req.msg,
            id,
            lparam: req.lparam,
        };

        self.win_timers_pending.entry(target_tid).or_default().push(timer);

        reply_fixed(&SetWinTimerReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            id,
        })
    }


    pub(crate) fn handle_kill_win_timer(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<KillWinTimerRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const KillWinTimerRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let tid = self.client_thread_id(client_fd as RawFd);
        let tid_val = match tid {
            Some(t) if t != 0 => t,
            _ => return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }),
        };

        // Resolve target thread from window handle
        let target_tid = if req.win != 0 {
            if let Some(ws) = self.window_states.get(&req.win) {
                ws.tid
            } else {
                return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 }); // STATUS_INVALID_PARAMETER
            }
        } else {
            tid_val
        };

        // Find and remove timer — stock: queue.c:3562
        let mut found = false;
        for list in [&mut self.win_timers_pending, &mut self.win_timers_expired] {
            if let Some(timers) = list.get_mut(&target_tid) {
                let before = timers.len();
                timers.retain(|t| !(t.win == req.win && t.msg == req.msg && t.id == req.id));
                if timers.len() < before { found = true; }
            }
        }

        if !found {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 }); // STATUS_INVALID_PARAMETER
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    /// Move expired timers from pending to expired and signal their thread's queue event.
    /// Called from the authority housekeeping tick (every TICK_MS).
    pub(crate) fn check_win_timers(&mut self) {
        let now = std::time::Instant::now();
        let mut wake_tids: Vec<u32> = Vec::new();

        for (tid, timers) in &mut self.win_timers_pending {
            let mut i = 0;
            while i < timers.len() {
                if timers[i].when <= now {
                    let expired = timers.remove(i);
                    self.win_timers_expired.entry(*tid).or_default().push(expired);
                    if !wake_tids.contains(tid) {
                        wake_tids.push(*tid);
                    }
                } else {
                    i += 1;
                }
            }
        }

        // Signal each thread's queue ntsync event so get_message wakes up
        for tid in wake_tids {
            let client = self.clients.values().find(|c| c.thread_id == tid);
            if let Some(client) = client {
                let pid = client.process_id;
                let qh = client.queue_handle;
                if qh != 0 {
                    if let Some((obj, _)) = self.ntsync_objects.get(&(pid, qh)) {
                        let _ = obj.event_set();
                    }
                }
            }
        }
    }


    pub(crate) fn handle_register_hotkey(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&RegisterHotkeyReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            replaced: 0,
            flags: 0,
            vkey: 0,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_unregister_hotkey(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&UnregisterHotkeyReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            flags: 0,
            vkey: 0,
        })
    }


    pub(crate) fn handle_get_window_layered_info(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowLayeredInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowLayeredInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Stock (window.c): returns ERROR_INVALID_WINDOW_HANDLE for non-layered windows.
        // WS_EX_LAYERED = 0x00080000.
        let is_layered = self.window_states.get(&req.handle)
            .map(|ws| ws.ex_style & 0x00080000 != 0)
            .unwrap_or(false);

        if is_layered {
            reply_fixed(&GetWindowLayeredInfoReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                color_key: 0, alpha: 255, flags: 0, _pad_0: [0; 4],
            })
        } else {
            reply_fixed(&GetWindowLayeredInfoReply {
                header: ReplyHeader { error: 0xC0000008, reply_size: 0 },
                color_key: 0, alpha: 0, flags: 0, _pad_0: [0; 4],
            })
        }
    }


    pub(crate) fn handle_set_window_layered_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_get_window_region(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowRegionRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowRegionRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let visible_rect = self.window_states.get(&req.window)
            .map(|ws| ws.visible_rect)
            .unwrap_or([0u8; 16]);

        // When surface=1, return the visible rect as the clipping region.
        // The X11 driver needs this to set up the surface and map the window.
        // Stock wineserver returns total_size=16 with the rect as vararg data.
        if req.surface != 0 {
            let max = max_reply_vararg(buf) as usize;
            if max >= 16 {
                return reply_vararg(&GetWindowRegionReply {
                    header: ReplyHeader { error: 0, reply_size: 16 },
                    visible_rect,
                    total_size: 16,
                    _pad_0: [0; 4],
                }, &visible_rect);
            }
        }

        reply_fixed(&GetWindowRegionReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            visible_rect,
            total_size: 0,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_set_window_region(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    // Window property operations
    //
    // Wine sends property names two ways:
    //   1. Integer atom: req.atom = N (nonzero), no VARARG
    //   2. String name:  req.atom = 0, VARARG contains UTF-16LE name
    // We must resolve string names to atoms (via atom_names) so each
    // unique property name gets a unique key. Without this, all string
    // properties on the same window collide at key (window, 0).

    pub(crate) fn handle_get_window_property(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowPropertyRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowPropertyRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let atom = if req.atom != 0 {
            req.atom
        } else {
            let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
            let name_u16 = vararg_to_u16(vararg);
            let _name_str = String::from_utf16_lossy(&name_u16);
            // Try exact match first, then lowercase (atoms are case-insensitive)
            self.state.atom_names.get(&name_u16).copied().unwrap_or_else(|| {
                let lower: Vec<u16> = name_u16.iter().map(|&c| {
                    if c < 128 { (c as u8 as char).to_lowercase().next().unwrap() as u16 } else { c }
                }).collect();
                self.state.atom_names.get(&lower).copied().unwrap_or(0)
            })
        };
        let key = (req.window, atom);
        if let Some(&data) = self.window_properties.get(&key) {
            reply_fixed(&GetWindowPropertyReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                data,
            })
        } else {
            reply_fixed(&GetWindowPropertyReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                data: 0,
            })
        }
    }

    pub(crate) fn handle_set_window_property(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetWindowPropertyRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetWindowPropertyRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let atom = if req.atom != 0 {
            req.atom
        } else {
            let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
            let name_u16 = vararg_to_u16(vararg);
            // Auto-register unknown string names so get/remove can find them later
            let next = self.state.next_atom;
            let atom = *self.state.atom_names.entry(name_u16).or_insert_with(|| {
                self.state.next_atom += 1;
                next
            });
            atom
        };
        let key = (req.window, atom);
        self.window_properties.insert(key, req.data);
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_remove_window_property(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<RemoveWindowPropertyRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const RemoveWindowPropertyRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let atom = if req.atom != 0 {
            req.atom
        } else {
            let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
            let name_u16 = vararg_to_u16(vararg);
            self.state.atom_names.get(&name_u16).copied().unwrap_or(0)
        };
        let key = (req.window, atom);
        let data = self.window_properties.remove(&key).unwrap_or(0);
        reply_fixed(&RemoveWindowPropertyReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            data,
        })
    }

    pub(crate) fn handle_get_window_properties(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetWindowPropertiesReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            total: 0,
            _pad_0: [0; 4],
        })
    }


    // Window tree/list
    pub(crate) fn handle_get_window_tree(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowTreeRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowTreeRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let (parent, owner) = self.window_states.get(&req.handle)
            .map(|ws| (ws.parent, ws.owner))
            .unwrap_or((0, 0));

        // Collect siblings (windows sharing the same parent)
        let mut siblings: Vec<u32> = Vec::new();
        if parent != 0 {
            for (&h, ws) in &self.window_states {
                if ws.parent == parent && h != parent {
                    siblings.push(h);
                }
            }
        }
        let my_idx = siblings.iter().position(|&h| h == req.handle);
        let next_sibling = my_idx.and_then(|i| siblings.get(i + 1).copied()).unwrap_or(0);
        let prev_sibling = my_idx.and_then(|i| if i > 0 { siblings.get(i - 1).copied() } else { None }).unwrap_or(0);
        let first_sibling = siblings.first().copied().unwrap_or(0);
        let last_sibling = siblings.last().copied().unwrap_or(0);

        // Collect children (windows whose parent is req.handle)
        let mut children: Vec<u32> = Vec::new();
        for (&h, ws) in &self.window_states {
            if ws.parent == req.handle && h != req.handle {
                children.push(h);
            }
        }
        let first_child = children.first().copied().unwrap_or(0);
        let last_child = children.last().copied().unwrap_or(0);

        reply_fixed(&GetWindowTreeReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            parent,
            owner,
            next_sibling,
            prev_sibling,
            first_sibling,
            last_sibling,
            first_child,
            last_child,
        })
    }

    pub(crate) fn handle_get_window_list(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowListRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowListRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let mut handles: Vec<u32> = Vec::new();

        if req.handle != 0 && req.children != 0 {
            // Recursive children of specified window
            self.collect_window_children(req.handle, &mut handles);
        } else if req.handle != 0 {
            // Siblings: all windows with the same parent, starting from handle
            if let Some(parent) = self.window_states.get(&req.handle).map(|ws| ws.parent) {
                for (&h, ws) in &self.window_states {
                    if ws.parent == parent {
                        handles.push(h);
                    }
                }
            }
        } else {
            // Top-level windows: children of desktop_top_window
            let desktop_parent = self.desktop_top_window;
            for (&h, ws) in &self.window_states {
                if ws.parent == desktop_parent && h != desktop_parent {
                    handles.push(h);
                }
            }
        }

        // Filter by thread if requested
        if req.tid != 0 {
            let tid = req.tid;
            handles.retain(|&h| {
                self.window_states.get(&h).map(|ws| ws.tid == tid).unwrap_or(false)
            });
        }

        let max = max_reply_vararg(buf) as usize;
        let vararg: Vec<u8> = handles.iter().flat_map(|h| h.to_le_bytes()).collect();
        let send_len = vararg.len().min(max);
        let count = send_len / 4;

        if count == 0 {
            return reply_fixed(&GetWindowListReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                count: 0,
                _pad_0: [0; 4],
            });
        }


        let reply = GetWindowListReply {
            header: ReplyHeader { error: 0, reply_size: send_len as u32 },
            count: count as i32,
            _pad_0: [0; 4],
        };
        reply_vararg(&reply, &vararg[..send_len])
    }

    /// Recursively collect all children of a window handle.
    fn collect_window_children(&self, parent: u32, out: &mut Vec<u32>) {
        for (&h, ws) in &self.window_states {
            if ws.parent == parent && h != parent {
                out.push(h);
                self.collect_window_children(h, out);
            }
        }
    }

    pub(crate) fn handle_get_window_parents(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetWindowParentsRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetWindowParentsRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let mut handles: Vec<u32> = Vec::new();
        let mut current = req.handle;
        // Walk up the parent chain (cap at 64 to prevent cycles)
        for _ in 0..64 {
            match self.window_states.get(&current) {
                Some(ws) if ws.parent != 0 => {
                    handles.push(ws.parent);
                    current = ws.parent;
                }
                _ => break,
            }
        }

        let max = max_reply_vararg(buf) as usize;
        let vararg: Vec<u8> = handles.iter().flat_map(|h| h.to_le_bytes()).collect();
        let send_len = vararg.len().min(max);
        let count = send_len / 4;

        if count == 0 {
            return reply_fixed(&GetWindowParentsReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                count: 0,
                _pad_0: [0; 4],
            });
        }

        let reply = GetWindowParentsReply {
            header: ReplyHeader { error: 0, reply_size: send_len as u32 },
            count: count as i32,
            _pad_0: [0; 4],
        };
        reply_vararg(&reply, &vararg[..send_len])
    }


    pub(crate) fn handle_set_class_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&SetClassInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            old_info: 0,
        })
    }

    pub(crate) fn handle_destroy_class(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<DestroyClassRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const DestroyClassRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let ptr = self.class_client_ptrs.remove(&req.atom).unwrap_or(0);
        self.class_locators.remove(&req.atom);
        self.class_win_extra.remove(&req.atom);
        reply_fixed(&DestroyClassReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            client_ptr: ptr,
        })
    }

    pub(crate) fn handle_get_class_windows(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetClassWindowsReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            count: 0,
            _pad_0: [0; 4],
        })
    }


    // User handle allocation
    pub(crate) fn handle_alloc_user_handle(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<AllocUserHandleRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const AllocUserHandleRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let tid = self.client_thread_id(client_fd as RawFd).unwrap_or(0);
        let pid = self.client_pid(client_fd as RawFd);
        // System Wine's AllocUserHandleRequest has no 'type' field (Proton-only).
        // Default to NTUSER_OBJ_WINDOW (1) as the most common case.
        let handle = self.alloc_user_handle(1, tid, pid);

        reply_fixed(&AllocUserHandleReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_free_user_handle(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<FreeUserHandleRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const FreeUserHandleRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        // Recycle the handle index so alloc_user_handle can reuse it
        const FIRST_USER_HANDLE: u32 = 0x0020;
        if req.handle < FIRST_USER_HANDLE {
            return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }); // STATUS_INVALID_HANDLE
        }
        let index = (req.handle - FIRST_USER_HANDLE) >> 1;
        if index >= self.next_user_handle_index {
            return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }); // STATUS_INVALID_HANDLE
        }
        self.user_handle_free_list.push(index);
        self.window_states.remove(&req.handle);
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    // Hook operations
    pub(crate) fn handle_set_hook(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&SetHookReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_remove_hook(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_start_hook_chain(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&StartHookChainReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: 0,
            pid: 0,
            tid: 0,
            unicode: 0,
            r#proc: 0,
        })
    }

    pub(crate) fn handle_finish_hook_chain(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_get_hook_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetHookInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: 0,
            id: 0,
            pid: 0,
            tid: 0,
            r#proc: 0,
            unicode: 0,
            _pad_0: [0; 4],
        })
    }


    // Desktop/Winstation operations
    pub(crate) fn handle_open_desktop(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let oid = self.state.alloc_object_id();
        let handle = self.alloc_handle_for_client(_client_fd, oid);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        reply_fixed(&OpenDesktopReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_open_input_desktop(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let oid = self.state.alloc_object_id();
        let handle = self.alloc_handle_for_client(_client_fd, oid);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        reply_fixed(&OpenInputDesktopReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_close_desktop(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_set_input_desktop(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_set_desktop_shell_windows(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&SetDesktopShellWindowsReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            old_shell_window: 0,
            old_shell_listview: 0,
            old_progman_window: 0,
            old_taskman_window: 0,
        })
    }

    pub(crate) fn handle_create_winstation(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let oid = self.state.alloc_object_id();
        let handle = self.alloc_handle_for_client(_client_fd, oid);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }

        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        if !vararg.is_empty() {
            self.winstation_names.insert(oid as u32, vararg.to_vec());
        }

        reply_fixed(&CreateWinstationReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_open_winstation(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        let oid = self.state.alloc_object_id();
        let handle = self.alloc_handle_for_client(_client_fd, oid);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        reply_fixed(&OpenWinstationReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_close_winstation(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_enum_winstation(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&EnumWinstationReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            count: 0,
            total: 0,
        })
    }

    pub(crate) fn handle_set_winstation_monitors(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        // Parse request: fixed header (16 bytes) followed by vararg monitor_info array.
        // monitor_info = { rectangle raw (16), rectangle virt (16), u32 flags, u32 dpi } = 40 bytes each
        const MONITOR_INFO_SIZE: usize = 40;
        let req = if buf.len() >= std::mem::size_of::<SetWinstationMonitorsRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetWinstationMonitorsRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        let monitor_count = vararg.len() / MONITOR_INFO_SIZE;

        if req.increment != 0 {
            self.monitor_serial += 1;
        }
        let serial = self.monitor_serial;

        // Write monitor_serial to desktop_shm_t in session shared memory.
        // desktop_shm_t layout: flags(4) + shared_cursor(28) + keystate(256) = offset 288 for monitor_serial.
        const DESKTOP_SHM_MONITOR_SERIAL_OFFSET: usize = 288;
        if !self.session_map.is_null() {
            unsafe {
                let base = self.session_map.add(self.desktop_offset as usize);
                // shared_object_t header is 16 bytes, desktop_shm_t starts at +16
                let shm = base.add(16);

                let seq_atomic = &*(base as *const std::sync::atomic::AtomicI64);
                let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                *(shm.add(DESKTOP_SHM_MONITOR_SERIAL_OFFSET) as *mut u64) = serial;
                seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
            }
        }

        // Update desktop window rect from first monitor's virtual rect.
        // Wine's display driver reads the desktop rect for screen dimensions.
        if monitor_count > 0 && vararg.len() >= MONITOR_INFO_SIZE {
            // Monitor info layout: raw_rect(16) + virt_rect(16) + flags(4) + dpi(4) = 40
            // virt_rect is at offset 16: left(4) + top(4) + right(4) + bottom(4)
            let virt_rect = &vararg[16..32];
            if let Some(ws) = self.window_states.get_mut(&self.desktop_top_window) {
                ws.window_rect.copy_from_slice(virt_rect);
                ws.client_rect.copy_from_slice(virt_rect);
            }
            // Store monitor rect for fullscreen window sizing
            self.monitor_rect.copy_from_slice(virt_rect);

            // Update desktop cursor clip to match real monitor dimensions
            let right = i32::from_le_bytes([virt_rect[8], virt_rect[9], virt_rect[10], virt_rect[11]]);
            let bottom = i32::from_le_bytes([virt_rect[12], virt_rect[13], virt_rect[14], virt_rect[15]]);
            if !self.session_map.is_null() && right > 0 && bottom > 0 {
                self.shared_write(self.desktop_offset, |shm| unsafe {
                    // cursor.clip: x(4) + y(4) + last_change(4) + clip.left(4) + clip.top(4) + clip.right(4) + clip.bottom(4)
                    *(shm.add(24) as *mut i32) = right;
                    *(shm.add(28) as *mut i32) = bottom;
                });
            }
        }


        reply_fixed(&SetWinstationMonitorsReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            serial,
        })
    }


    // Message queue
    pub(crate) fn handle_get_msg_queue(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        // Ensure queue locator exists — allocate on demand if needed
        // (stock wineserver creates queues lazily via get_current_queue).
        let locator = if let Some(client) = self.clients.get(&(client_fd as RawFd)) {
            if client.queue_locator != [0u8; 16] {
                client.queue_locator
            } else {
                // Try sharing from same process
                let pid = client.process_id;
                let existing = self.clients.values()
                    .find(|c| c.process_id == pid && c.queue_locator != [0u8; 16])
                    .map(|c| c.queue_locator);
                if let Some(loc) = existing {
                    if let Some(c) = self.clients.get_mut(&(client_fd as RawFd)) {
                        c.queue_locator = loc;
                    }
                    loc
                } else {
                    let q = self.alloc_shared_object();
                    if let Some(c) = self.clients.get_mut(&(client_fd as RawFd)) {
                        c.queue_locator = q;
                    }
                    q
                }
            }
        } else {
            [0u8; 16]
        };
        reply_fixed(&GetMsgQueueReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            locator,
        })
    }

    pub(crate) fn handle_get_msg_queue_handle(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        // Return a cached queue handle, or create one on first call.
        // Wine needs a valid waitable handle for the thread's message queue.
        // Backed by an ntsync auto-reset event so MsgWaitForMultipleObjects
        // can block on it via Select, and send_message/post_message can wake
        // the blocked thread by signaling it.
        // Initially signaled: Wine's windowing init expects the first MsgWait
        // to return immediately. Auto-reset: consumed by Select, resets so
        // subsequent MsgWaits block until a message is posted.
        let existing = self.clients.get(&(client_fd as RawFd))
            .map(|c| c.queue_handle)
            .unwrap_or(0);

        let handle = if existing != 0 {
            existing
        } else {
            // Create ntsync auto-reset event (initially signaled) as queue handle
            let evt = self.get_or_create_event(false, true); // auto-reset, signaled
            if let Some(evt) = evt {
                let oid = self.state.alloc_object_id();
                let evt_fd = evt.fd();
                let pid = self.clients.get(&(client_fd as RawFd))
                    .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
                let h = if let Some(pid) = pid {
                    if let Some(process) = self.state.processes.get_mut(&pid) {
                        process.handles.allocate_full(
                            crate::objects::HandleEntry::with_fd(oid, evt_fd, crate::objects::FD_TYPE_FILE, 0x001F0003, 0x20)
                        )
                    } else { 0 }
                } else { 0 };
                if h != 0 {
                    let pid = self.client_pid(client_fd as RawFd);
                    self.ntsync_objects.insert((pid, h), (evt, 1)); // INTERNAL (queue event)
                    self.ntsync_objects_created += 1;
                    if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                        client.queue_handle = h;
                    }
                }
                h
            } else { 0 }
        };

        // Return the process idle event if we have one
        let pid = self.client_pid(client_fd as RawFd);
        let _idle_event = self.process_idle_events.get(&pid)
            .and_then(|_| {
                // The idle event handle was already allocated in the process —
                // but we'd need to look it up. For now return 0 (optional).
                None::<u32>
            })
            .unwrap_or(0);

        reply_fixed(&GetMsgQueueHandleReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            idle_event: 0,
        })
    }

    // handle_get_last_input_time was removed in Wine 11.6 (opcode no longer in
    // protocol.def). Last-input timestamps are now read client-side from
    // KUSER_SHARED_DATA. The handler is dropped from build.rs's dispatch table
    // automatically when regenerating against 11.6 or later.

    pub(crate) fn handle_send_hardware_message(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SendHardwareMessageRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SendHardwareMessageRequest) }
        } else {
            return reply_fixed(&SendHardwareMessageReply {
                header: ReplyHeader { error: 0xC000000D, reply_size: 0 },
                wait: 0, prev_x: 0, prev_y: 0, new_x: 0, new_y: 0, _pad_0: [0; 4],
            });
        };

        let prev_x = self.cursor_x;
        let prev_y = self.cursor_y;

        // input[0..4] = type: 0=MOUSE, 1=KEYBOARD, 2=HARDWARE
        let input_type = u32::from_le_bytes([req.input[0], req.input[1], req.input[2], req.input[3]]);
        match input_type {
            0 => self.queue_mouse_message(req.win, &req.input),
            1 => self.queue_keyboard_message(req.win, &req.input),
            _ => {} // INPUT_HARDWARE — ignore
        }

        reply_fixed(&SendHardwareMessageReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            wait: 0,
            prev_x,
            prev_y,
            new_x: self.cursor_x,
            new_y: self.cursor_y,
            _pad_0: [0; 4],
        })
    }

    /// Update cursor position and keystate in desktop shared memory.
    /// desktop_shm_t layout at session_map + desktop_offset + 16:
    ///   +0:  flags (u32)
    ///   +4:  cursor.x (i32)
    ///   +8:  cursor.y (i32)
    ///   +12: cursor.last_change (u32)
    ///   +16: cursor.clip (4xi32 = 16 bytes)
    ///   +32: keystate[256]
    fn update_desktop_cursor(&mut self, x: i32, y: i32, time: u32) {
        if self.session_map.is_null() { return; }
        let base = unsafe { self.session_map.add(self.desktop_offset as usize) };
        let seq_atomic = unsafe { &*(base as *const std::sync::atomic::AtomicI64) };
        let shm = unsafe { base.add(16) }; // skip shared_object_t header
        unsafe {
            let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
            seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
            *(shm.add(4) as *mut i32) = x;     // cursor.x
            *(shm.add(8) as *mut i32) = y;     // cursor.y
            *(shm.add(12) as *mut u32) = time;  // cursor.last_change
            seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
        }
        self.cursor_x = x;
        self.cursor_y = y;
    }

    fn update_desktop_keystate(&self, vk: u8, down: bool) {
        if self.session_map.is_null() { return; }
        let base = unsafe { self.session_map.add(self.desktop_offset as usize) };
        let seq_atomic = unsafe { &*(base as *const std::sync::atomic::AtomicI64) };
        let shm = unsafe { base.add(16) }; // skip shared_object_t header
        // keystate is at offset 32 in desktop_shm_t
        let ks_ptr = unsafe { shm.add(32).add(vk as usize) };
        unsafe {
            let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
            seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
            let old = *(ks_ptr as *const u8);
            if down {
                *(ks_ptr as *mut u8) = old | 0x80; // key down
            } else {
                *(ks_ptr as *mut u8) = old & !0x80; // key up
            }
            seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
        }
    }

    fn read_desktop_keystate(&self, vk: u8) -> u8 {
        if self.session_map.is_null() { return 0; }
        let shm = unsafe { self.session_map.add(self.desktop_offset as usize + 16) };
        unsafe { *(shm.add(32).add(vk as usize) as *const u8) }
    }

    fn queue_mouse_message(&mut self, win: u32, input: &[u8; 40]) {
        let dx = i32::from_le_bytes([input[4], input[5], input[6], input[7]]);
        let dy = i32::from_le_bytes([input[8], input[9], input[10], input[11]]);
        let flags = u32::from_le_bytes([input[16], input[17], input[18], input[19]]);
        let time = u32::from_le_bytes([input[20], input[21], input[22], input[23]]);

        const MOUSEEVENTF_MOVE: u32 = 0x0001;
        const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
        const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
        const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
        const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
        const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
        const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
        const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;

        // VK codes for button keystate
        const VK_LBUTTON: u8 = 0x01;
        const VK_RBUTTON: u8 = 0x02;
        const VK_MBUTTON: u8 = 0x04;

        // Update cursor position in desktop SHM (stock: queue.c:2253-2272)
        let (x, y) = if flags & MOUSEEVENTF_MOVE != 0 {
            if flags & MOUSEEVENTF_ABSOLUTE != 0 {
                (dx, dy)
            } else {
                (self.cursor_x + dx, self.cursor_y + dy)
            }
        } else {
            (self.cursor_x, self.cursor_y)
        };
        self.update_desktop_cursor(x, y, time);

        // Update desktop keystate for button presses (stock: queue.c:1899, update_desktop_key_state)
        if flags & MOUSEEVENTF_LEFTDOWN != 0 { self.update_desktop_keystate(VK_LBUTTON, true); }
        if flags & MOUSEEVENTF_LEFTUP != 0 { self.update_desktop_keystate(VK_LBUTTON, false); }
        if flags & MOUSEEVENTF_RIGHTDOWN != 0 { self.update_desktop_keystate(VK_RBUTTON, true); }
        if flags & MOUSEEVENTF_RIGHTUP != 0 { self.update_desktop_keystate(VK_RBUTTON, false); }
        if flags & MOUSEEVENTF_MIDDLEDOWN != 0 { self.update_desktop_keystate(VK_MBUTTON, true); }
        if flags & MOUSEEVENTF_MIDDLEUP != 0 { self.update_desktop_keystate(VK_MBUTTON, false); }

        // Build wparam with current button/key state (stock: queue.c:1916-1922)
        let mut wparam: u64 = 0;
        const MK_LBUTTON: u64 = 0x0001;
        const MK_RBUTTON: u64 = 0x0002;
        const MK_MBUTTON: u64 = 0x0010;
        if self.read_desktop_keystate(VK_LBUTTON) & 0x80 != 0 { wparam |= MK_LBUTTON; }
        if self.read_desktop_keystate(VK_RBUTTON) & 0x80 != 0 { wparam |= MK_RBUTTON; }
        if self.read_desktop_keystate(VK_MBUTTON) & 0x80 != 0 { wparam |= MK_MBUTTON; }

        // Message table (stock: queue.c:2226-2241)
        // bit index → message code, queue bits
        let msg_table: [(u32, u32); 7] = [
            (0x0200, 0x0002), // MOUSEEVENTF_MOVE     → WM_MOUSEMOVE, QS_MOUSEMOVE
            (0x0201, 0x0004), // MOUSEEVENTF_LEFTDOWN → WM_LBUTTONDOWN, QS_MOUSEBUTTON
            (0x0202, 0x0004), // MOUSEEVENTF_LEFTUP   → WM_LBUTTONUP, QS_MOUSEBUTTON
            (0x0204, 0x0004), // MOUSEEVENTF_RIGHTDOWN→ WM_RBUTTONDOWN, QS_MOUSEBUTTON
            (0x0205, 0x0004), // MOUSEEVENTF_RIGHTUP  → WM_RBUTTONUP, QS_MOUSEBUTTON
            (0x0207, 0x0004), // MOUSEEVENTF_MIDDLEDOWN→WM_MBUTTONDOWN, QS_MOUSEBUTTON
            (0x0208, 0x0004), // MOUSEEVENTF_MIDDLEUP → WM_MBUTTONUP, QS_MOUSEBUTTON
        ];

        // Find target thread
        let target_tid = if win != 0 {
            self.window_states.get(&win).map(|ws| ws.tid).unwrap_or(0)
        } else {
            self.window_states.values()
                .find(|ws| ws.parent == self.desktop_top_window && ws.style & 0x10000000 != 0)
                .map(|ws| ws.tid)
                .unwrap_or(0)
        };
        if target_tid == 0 { return; }

        let target_win = if win != 0 { win } else {
            self.window_states.iter()
                .find(|(_, ws)| ws.tid == target_tid && ws.parent == self.desktop_top_window)
                .map(|(&h, _)| h).unwrap_or(0)
        };

        let lparam = ((y as u32 as u64) << 16) | (x as u32 as u64 & 0xFFFF);
        self.last_input_time = time;

        // Post input messages to SHM ring as QS_POSTMESSAGE so the client
        // reads them directly without a server round-trip. Stock Wine uses a
        // separate hardware queue (MSG_HARDWARE + QS_INPUT), but our SHM ring
        // architecture delivers all messages through the same posted ring.
        const QS_POSTMESSAGE: u32 = 0x0008;
        for (i, &(msg_code, _qbits)) in msg_table.iter().enumerate() {
            if flags & (1 << i) == 0 { continue; }
            if let Some(queue) = self.shm.get_queue(target_tid as u32) {
                let msg = crate::queue::QueuedMessage {
                    win: target_win,
                    msg: msg_code,
                    wparam,
                    lparam,
                    msg_type: MSG_POSTED,
                    x,
                    y,
                    time,
                    _pad: [0; 2],
                };
                queue.post(msg);
            }
            self.set_queue_bits_for_tid(target_tid, QS_POSTMESSAGE);
        }
    }

    fn queue_keyboard_message(&mut self, win: u32, input: &[u8; 40]) {
        let vkey = u16::from_le_bytes([input[4], input[5]]);
        let scan = u16::from_le_bytes([input[6], input[7]]);
        let flags = u32::from_le_bytes([input[8], input[9], input[10], input[11]]);
        let time = u32::from_le_bytes([input[12], input[13], input[14], input[15]]);

        const KEYEVENTF_KEYUP: u32 = 0x0002;
        const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;

        let is_up = flags & KEYEVENTF_KEYUP != 0;
        let msg_code: u32 = if is_up { 0x0101 } else { 0x0100 }; // WM_KEYUP : WM_KEYDOWN

        // Update desktop keystate (stock: update_desktop_key_state)
        if vkey < 256 {
            self.update_desktop_keystate(vkey as u8, !is_up);
        }

        let mut kf: u32 = 0;
        if flags & KEYEVENTF_EXTENDEDKEY != 0 { kf |= 0x0100; }
        if is_up { kf |= 0xC000; } // KF_REPEAT | KF_UP
        let lparam = (((scan as u64) | (kf as u64)) << 16) | 1;

        let target_tid = if win != 0 {
            self.window_states.get(&win).map(|ws| ws.tid).unwrap_or(0)
        } else {
            self.window_states.values()
                .find(|ws| ws.parent == self.desktop_top_window && ws.style & 0x10000000 != 0)
                .map(|ws| ws.tid)
                .unwrap_or(0)
        };
        if target_tid == 0 { return; }

        self.last_input_time = time;

        if let Some(queue) = self.shm.get_queue(target_tid as u32) {
            let msg = crate::queue::QueuedMessage {
                win: if win != 0 { win } else {
                    self.window_states.iter()
                        .find(|(_, ws)| ws.tid == target_tid && ws.parent == self.desktop_top_window)
                        .map(|(&h, _)| h).unwrap_or(0)
                },
                msg: msg_code,
                wparam: vkey as u64,
                lparam,
                msg_type: MSG_POSTED,
                x: self.cursor_x,
                y: self.cursor_y,
                time,
                _pad: [0; 2],
            };
            queue.post(msg);
        }
        self.set_queue_bits_for_tid(target_tid, 0x0008); // QS_POSTMESSAGE (not QS_KEY)
    }


    // Window updates
    pub(crate) fn handle_get_update_region(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetUpdateRegionRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetUpdateRegionRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Stock: UPDATE_PAINT=0x04, UPDATE_ERASE=0x02 (window.c:2947)
        const UPDATE_PAINT: u32 = 0x04;
        const UPDATE_ERASE: u32 = 0x02;

        // Find window (or child) that needs painting
        let search_root = if req.from_child != 0 { req.from_child } else { req.window };
        let mut paint_child: Option<u32> = None;

        if self.window_states.get(&search_root).map(|ws| ws.needs_paint).unwrap_or(false) {
            paint_child = Some(search_root);
        } else {
            // Search children depth-first (stock: get_window_update_flags)
            let mut children = Vec::new();
            self.collect_window_children(search_root, &mut children);
            for &child in &children {
                if self.window_states.get(&child).map(|ws| ws.needs_paint).unwrap_or(false) {
                    paint_child = Some(child);
                    break;
                }
            }
        }

        let child = match paint_child {
            Some(c) => c,
            None => return reply_fixed(&GetUpdateRegionReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                child: 0, flags: 0, total_size: 0, _pad_0: [0; 4],
            }),
        };

        // Clear paint state when any validate flag is set (stock uses flags & 0x06 = UPDATE_ERASE|UPDATE_PAINT)
        // Stock sends flags=0x03 (UPDATE_NONCLIENT|UPDATE_ERASE) which must also clear.
        if req.flags & (UPDATE_PAINT | UPDATE_ERASE) != 0 {
            let tid = self.window_states.get(&child).map(|ws| ws.tid).unwrap_or(0);
            if let Some(ws) = self.window_states.get_mut(&child) {
                ws.needs_paint = false;
            }
            // Clear QS_PAINT if no more windows for this thread need painting
            if tid != 0 {
                let any_paint = self.window_states.values().any(|ws| ws.tid == tid && ws.needs_paint);
                if !any_paint {
                    self.clear_queue_bits_for_tid(tid, 0x0020); // QS_PAINT
                }
            }
        }

        // Return UPDATE_PAINT | UPDATE_ERASE with total_size=0 (empty region = full repaint)
        reply_fixed(&GetUpdateRegionReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            child, flags: UPDATE_PAINT | UPDATE_ERASE, total_size: 0, _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_redraw_window(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() >= std::mem::size_of::<RedrawWindowRequest>() {
            let req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const RedrawWindowRequest) };
            const RDW_INVALIDATE: u32 = 0x0001;
            const RDW_INTERNALPAINT: u32 = 0x0010;
            const RDW_VALIDATE: u32 = 0x0008;
            const RDW_NOINTERNALPAINT: u32 = 0x0020;
            const RDW_ALLCHILDREN: u32 = 0x0080;
            const RDW_NOCHILDREN: u32 = 0x0040;

            // Collect target window + children if RDW_ALLCHILDREN
            let mut targets = vec![req.window];
            if req.flags & RDW_ALLCHILDREN != 0 && req.flags & RDW_NOCHILDREN == 0 {
                self.collect_window_children(req.window, &mut targets);
            }

            let invalidate = req.flags & (RDW_INVALIDATE | RDW_INTERNALPAINT) != 0;
            let validate = req.flags & (RDW_VALIDATE | RDW_NOINTERNALPAINT) != 0;

            for &handle in &targets {
                if let Some(ws) = self.window_states.get_mut(&handle) {
                    if invalidate { ws.needs_paint = true; }
                    if validate { ws.needs_paint = false; }
                }
            }

            // Wake owning thread's queue so MsgWaitForMultipleObjects unblocks
            if invalidate {
                if let Some(tid) = self.window_states.get(&req.window).map(|ws| ws.tid) {
                    self.set_queue_bits_for_tid(tid, 0x0020); // QS_PAINT
                }
            }
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_get_clipboard_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetClipboardInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            window: 0,
            owner: 0,
            viewer: 0,
            seqno: self.clipboard_seqno,
        })
    }

    pub(crate) fn handle_set_keyboard_repeat(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetKeyboardRepeatRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetKeyboardRepeatRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let prev_enable = self.keyboard_repeat.0;
        if req.enable >= 0 { self.keyboard_repeat.0 = req.enable; }
        if req.delay >= 0 { self.keyboard_repeat.1 = req.delay; }
        if req.period >= 0 { self.keyboard_repeat.2 = req.period; }

        reply_fixed(&SetKeyboardRepeatReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            enable: prev_enable,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_add_clipboard_listener(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() >= std::mem::size_of::<AddClipboardListenerRequest>() {
            let req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const AddClipboardListenerRequest) };
            self.clipboard_listeners.insert(req.window);
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_is_window_hung(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&IsWindowHungReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            is_hung: 0,
            _pad_0: [0; 4],
        })
    }
}
