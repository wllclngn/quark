// Completion ports, jobs, timers, sockets, and device requests

use super::*;

impl EventLoop {

    pub(crate) fn handle_create_job(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        let pid = self.client_pid(client_fd as RawFd);
        let oid = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(handle))
            .map(|e| e.object_id)
            .unwrap_or(0);
        if let Some(obj) = self.get_or_create_event(true, false) {
            self.ntsync_objects.insert((pid, handle), (obj, 1)); // INTERNAL
        }
        self.jobs.insert(oid, crate::objects::Job {
            processes: Vec::new(),
            num_processes: 0,
            total_processes: 0,
            limit_flags: 0,
            completion_port_handle: None,
            completion_key: 0,
        });
        let reply = CreateJobReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_set_job_limits(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetJobLimitsRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetJobLimitsRequest) }
        } else {
            return reply_fixed(&SetJobLimitsReply { header: ReplyHeader { error: 0, reply_size: 0 } });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let job_oid = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.handle))
            .map(|e| e.object_id);
        if let Some(oid) = job_oid {
            if let Some(job) = self.jobs.get_mut(&oid) {
                job.limit_flags = req.limit_flags;
            }
        }
        reply_fixed(&SetJobLimitsReply { header: ReplyHeader { error: 0, reply_size: 0 } })
    }


    pub(crate) fn handle_assign_job(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<AssignJobRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const AssignJobRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let caller_pid = self.client_pid(client_fd as RawFd);

        // Resolve job handle → object_id
        let job_oid = self.state.processes.get(&caller_pid)
            .and_then(|p| p.handles.get(req.job))
            .map(|e| e.object_id);

        // Resolve process handle → target pid (object_id stores pid for process handles)
        let target_pid = self.state.processes.get(&caller_pid)
            .and_then(|p| p.handles.get(req.process))
            .map(|e| e.object_id as u32);

        let (job_oid, target_pid) = match (job_oid, target_pid) {
            (Some(j), Some(t)) => (j, t),
            _ => return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }),
        };

        // Check process has running threads
        let has_threads = self.state.processes.get(&target_pid)
            .map(|p| !p.threads.is_empty())
            .unwrap_or(false);
        if !has_threads {
            return reply_fixed(&ReplyHeader { error: 0xC000010A, reply_size: 0 }); // STATUS_PROCESS_IS_TERMINATING
        }

        // Collect completion port info before mutable borrow
        let completion_info = self.jobs.get(&job_oid)
            .and_then(|j| j.completion_port_handle.map(|port| (port, j.completion_key)));

        // Update job state
        if let Some(job) = self.jobs.get_mut(&job_oid) {
            if !job.processes.contains(&target_pid) {
                job.processes.push(target_pid);
                job.num_processes += 1;
                job.total_processes += 1;
            }
        }
        self.process_job.insert(target_pid, job_oid);

        // Post JOB_OBJECT_MSG_NEW_PROCESS to completion port
        if let Some((port_handle, ckey)) = completion_info {
            let msg = CompletionMsg {
                ckey,
                cvalue: target_pid as u64,
                information: 0,
                status: 1, // JOB_OBJECT_MSG_NEW_PROCESS
            };
            if let Some(waiters) = self.completion_waiters.get_mut(&port_handle) {
                if let Some(waiter) = waiters.pop() {
                    self.thread_completion_cache.insert(waiter.client_fd, msg);
                    if let Some((obj, _)) = self.ntsync_objects.get(&(waiter.pid, waiter.wait_handle)) {
                        let _ = obj.event_set();
                    }
                    if waiters.is_empty() {
                        self.completion_waiters.remove(&port_handle);
                    }
                    return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
                }
            }
            self.completion_queues.entry(port_handle).or_default().push(msg);
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_create_completion(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        let pid = self.client_pid(client_fd as RawFd);
        if let Some(obj) = self.get_or_create_event(true, false) {
            self.ntsync_objects.insert((pid, handle), (obj, 1)); // INTERNAL
        }
        let reply = CreateCompletionReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_set_job_completion_port(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetJobCompletionPortRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetJobCompletionPortRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let job_oid = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.job))
            .map(|e| e.object_id);
        if let Some(oid) = job_oid {
            if let Some(job) = self.jobs.get_mut(&oid) {
                job.completion_port_handle = Some(req.port);
                job.completion_key = req.key;
            }
        }
        reply_fixed(&SetJobCompletionPortReply { header: ReplyHeader { error: 0, reply_size: 0 } })
    }


    pub(crate) fn handle_create_timer(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        let pid = self.client_pid(client_fd as RawFd);
        if let Some(obj) = self.get_or_create_event(true, false) {
            self.ntsync_objects.insert((pid, handle), (obj, 1)); // INTERNAL
        }
        let reply = CreateTimerReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    // ── Missing handler stubs (Phase 2: width-first stubbing) ──────────────

    // Timer operations — NT kernel timers (NtSetTimer/NtCancelTimer).
    // Services and games use these for startup timeouts and polling.

    pub(crate) fn handle_set_timer(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetTimerRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetTimerRequest) }
        } else {
            return reply_fixed(&SetTimerReply {
                header: ReplyHeader { error: 0xC000000D, reply_size: 0 },
                signaled: 0, _pad_0: [0; 4],
            });
        };

        let pid = self.client_pid(client_fd as RawFd);
        let handle = req.handle;

        // Check previous signaled state
        let prev_signaled = self.ntsync_objects.get(&(pid, handle))
            .and_then(|(obj, _)| obj.event_read())
            .map(|(_, s)| s as i32)
            .unwrap_or(0);

        // Convert FILETIME expire to Instant deadline
        use std::time::Instant;
        let now = Instant::now();
        let deadline = if req.expire == 0 {
            // expire=0 means "signal immediately"
            now
        } else {
            // Use compute_ntsync_timeout logic to convert Wine timeout to relative ns
            let mut mono_ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut mono_ts); }
            let _mono_now_ns = (mono_ts.tv_sec as u64) * 1_000_000_000 + (mono_ts.tv_nsec as u64);
            let rel_ns = if req.expire < 0 {
                ((-req.expire) as u64).saturating_mul(100)
            } else {
                let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts); }
                let now_100ns = (ts.tv_sec as i64) * 10_000_000 + (ts.tv_nsec as i64) / 100 + TICKS_1601_TO_1970;
                if req.expire <= now_100ns { 0 } else {
                    ((req.expire - now_100ns) as u64).saturating_mul(100)
                }
            };
            now + std::time::Duration::from_nanos(rel_ns)
        };

        // Remove any existing timer for this (pid, handle)
        self.nt_timers.retain(|&(p, h, _, _)| !(p == pid && h == handle));

        if deadline <= now {
            // Already expired — signal immediately
            if let Some((obj, _)) = self.ntsync_objects.get(&(pid, handle)) {
                let _ = obj.event_set();
            }
        } else {
            let period_ms = if req.period > 0 { req.period as u32 } else { 0 };
            self.nt_timers.push((pid, handle, deadline, period_ms));
        }

        reply_fixed(&SetTimerReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            signaled: prev_signaled,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_get_timer_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetTimerInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetTimerInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let signaled = self.ntsync_objects.get(&(pid, req.handle))
            .and_then(|(obj, _)| obj.event_read())
            .map(|(_, s)| s as i32)
            .unwrap_or(0);
        reply_fixed(&GetTimerInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            when: 0, // TODO: convert deadline back to FILETIME
            signaled,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_open_timer(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&OpenTimerReply {
            header: ReplyHeader { error: 0xc0000034, reply_size: 0 }, // STATUS_OBJECT_NAME_NOT_FOUND
            handle: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_cancel_timer(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<CancelTimerRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const CancelTimerRequest) }
        } else {
            return reply_fixed(&CancelTimerReply {
                header: ReplyHeader { error: 0xC000000D, reply_size: 0 },
                signaled: 0, _pad_0: [0; 4],
            });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let prev_signaled = self.ntsync_objects.get(&(pid, req.handle))
            .and_then(|(obj, _)| obj.event_read())
            .map(|(_, s)| s as i32)
            .unwrap_or(0);
        // Remove timer and reset event
        self.nt_timers.retain(|&(p, h, _, _)| !(p == pid && h == req.handle));
        if let Some((obj, _)) = self.ntsync_objects.get(&(pid, req.handle)) {
            let _ = obj.event_reset();
        }
        reply_fixed(&CancelTimerReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            signaled: prev_signaled,
            _pad_0: [0; 4],
        })
    }

    /// Check NT kernel timers and signal expired ones.
    pub(crate) fn check_nt_timers(&mut self) {
        if self.nt_timers.is_empty() { return; }
        let now = std::time::Instant::now();
        let mut i = 0;
        while i < self.nt_timers.len() {
            let (pid, handle, deadline, period_ms) = self.nt_timers[i];
            if deadline <= now {
                if let Some((obj, _)) = self.ntsync_objects.get(&(pid, handle)) {
                    let _ = obj.event_set();
                }
                if period_ms > 0 {
                    // Reschedule periodic timer
                    self.nt_timers[i].2 = now + std::time::Duration::from_millis(period_ms as u64);
                    i += 1;
                } else {
                    self.nt_timers.swap_remove(i);
                }
            } else {
                i += 1;
            }
        }
    }


    // ── Completion port operations ────────────────────────────────────────
    //
    // Matches stock wineserver's server/completion.c three-phase pattern:
    //   1. add_completion: enqueue message (or wake a blocked waiter directly)
    //   2. remove_completion: dequeue message or block (STATUS_PENDING + wait_handle)
    //   3. get_thread_completion: after wait is satisfied, fetch the cached message

    /// Enqueue a completion message. If a thread is blocked waiting on this
    /// port, deliver the message directly and signal its wait handle.
    pub(crate) fn handle_add_completion(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<AddCompletionRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const AddCompletionRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let msg = CompletionMsg {
            ckey: req.ckey,
            cvalue: req.cvalue,
            information: req.information,
            status: req.status,
        };


        // Check if any thread is blocked waiting on this port
        if let Some(waiters) = self.completion_waiters.get_mut(&req.handle) {
            if let Some(waiter) = waiters.pop() {
                // Deliver directly to the waiting thread

                self.thread_completion_cache.insert(waiter.client_fd, msg);

                // Signal the waiter's ntsync event so its Select/wait returns
                if let Some((obj, _)) = self.ntsync_objects.get(&(waiter.pid, waiter.wait_handle)) {
                    let _ = obj.event_set();
                }

                // Clean up empty waiter list
                if waiters.is_empty() {
                    self.completion_waiters.remove(&req.handle);
                }

                return reply_fixed(&ReplyHeader { error: 0, reply_size: 0 });
            }
        }

        // No waiters — enqueue the message
        self.completion_queues.entry(req.handle).or_default().push(msg);

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_add_fd_completion(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    /// Dequeue a completion message or block until one arrives.
    /// If the queue has messages, return the first one immediately.
    /// Otherwise, create an ntsync event for the caller to wait on and
    /// register a waiter so add_completion can deliver directly.
    pub(crate) fn handle_remove_completion(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<RemoveCompletionRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const RemoveCompletionRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Try immediate dequeue
        if let Some(queue) = self.completion_queues.get_mut(&req.handle) {
            if !queue.is_empty() {
                let msg = queue.remove(0);
                if queue.is_empty() {
                    self.completion_queues.remove(&req.handle);
                }
                return reply_fixed(&RemoveCompletionReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    ckey: msg.ckey,
                    cvalue: msg.cvalue,
                    information: msg.information,
                    status: msg.status,
                    wait_handle: 0,
                });
            }
        }

        // Queue empty — block. Create an ntsync event for the waiter.
        let pid = self.client_pid(client_fd as RawFd);
        let wait_handle = req.handle;

        // Ensure the completion port handle has an ntsync object so Select works.
        // Manual-reset, unsignaled — will be signaled when a completion arrives.
        if self.ntsync.is_some() && !self.ntsync_objects.contains_key(&(pid, wait_handle)) {
            if let Some(obj) = self.get_or_create_event(true, false) {
                self.insert_recyclable_event(pid, wait_handle, obj, 1); // INTERNAL
            }
        }

        // Register this thread as a waiter
        self.completion_waiters.entry(req.handle).or_default().push(CompletionWaiter {
            client_fd: client_fd as RawFd,
            pid,
            wait_handle,
        });


        reply_fixed(&RemoveCompletionReply {
            header: ReplyHeader { error: 0x103, reply_size: 0 }, // STATUS_PENDING
            ckey: 0,
            cvalue: 0,
            information: 0,
            status: 0,
            wait_handle,
        })
    }

    /// Fetch the cached completion message after a wait has been satisfied.
    /// Called by Wine's ntdll after NtWaitForSingleObject returns on the
    /// wait_handle from remove_completion.
    pub(crate) fn handle_get_thread_completion(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let fd = client_fd as RawFd;
        if let Some(msg) = self.thread_completion_cache.remove(&fd) {
            reply_fixed(&GetThreadCompletionReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                ckey: msg.ckey,
                cvalue: msg.cvalue,
                information: msg.information,
                status: msg.status,
                _pad_0: [0; 4],
            })
        } else {
            reply_fixed(&GetThreadCompletionReply {
                header: ReplyHeader { error: 0x103, reply_size: 0 }, // STATUS_PENDING
                ckey: 0,
                cvalue: 0,
                information: 0,
                status: 0,
                _pad_0: [0; 4],
            })
        }
    }

    pub(crate) fn handle_query_completion(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let handle = if buf.len() >= std::mem::size_of::<QueryCompletionRequest>() {
            let req = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const QueryCompletionRequest) };
            req.handle
        } else {
            0
        };
        let depth = self.completion_queues.get(&handle)
            .map(|q| q.len() as u32)
            .unwrap_or(0);
        reply_fixed(&QueryCompletionReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            depth,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_open_completion(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&OpenCompletionReply {
            header: ReplyHeader { error: 0xc0000034, reply_size: 0 },
            handle: 0,
            _pad_0: [0; 4],
        })
    }

    /// Bind an object (e.g., pipe handle) to a completion port.
    /// When async operations complete on the object, a completion message
    /// is posted to the port with the specified key.
    pub(crate) fn handle_set_completion_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetCompletionInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetCompletionInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.client_pid(client_fd as RawFd);

        // Store the binding: (pid, object_handle) → (completion_port_handle, key)
        self.side_tables.completion_bindings.insert((pid, req.handle), (req.chandle, req.ckey));

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_get_exception_status(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    // Device requests
    pub(crate) fn handle_get_next_device_request(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetNextDeviceRequestReply {
            header: ReplyHeader { error: 0x103, reply_size: 0 }, // STATUS_PENDING
            params: [0u8; 32],
            next: 0,
            client_tid: 0,
            client_thread: 0,
            in_size: 0,
            _pad_0: [0; 4],
        })
    }


    // Socket operations
    pub(crate) fn handle_send_socket(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&SendSocketReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            wait: 0,
            options: 0,
            nonblocking: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_recv_socket(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&RecvSocketReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            wait: 0,
            options: 0,
            nonblocking: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_socket_get_events(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&SocketGetEventsReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            flags: 0,
            _pad_0: [0; 4],
        })
    }


    // ---- Phase 2+5 handlers: previously auto-stubbed opcodes ----

    pub(crate) fn handle_get_directory_cache_entry(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // No server-side directory cache. Return STATUS_NO_MORE_ENTRIES so
        // the client falls back to direct readdir — correct behavior for an empty cache.
        reply_fixed(&GetDirectoryCacheEntryReply {
            header: ReplyHeader { error: 0x8000001A, reply_size: 0 }, // STATUS_NO_MORE_ENTRIES
            entry: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_map_builtin_view(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Notification: ntdll tells us a builtin DLL was mapped. Acknowledge.
        // The DLL is already mapped client-side before this call.
        reply_fixed(&MapBuiltinViewReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
        })
    }

    pub(crate) fn handle_get_image_view_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // No image view tracking yet. Return STATUS_INVALID_ADDRESS so the
        // caller uses the direct PE loading fallback path.
        reply_fixed(&GetImageViewInfoReply {
            header: ReplyHeader { error: 0xC0000141, reply_size: 0 }, // STATUS_INVALID_ADDRESS
            base: 0,
            size: 0,
        })
    }

    pub(crate) fn handle_init_window_info(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        // Window shared memory initialization. Save style, ex_style, and is_unicode.
        // Stock wineserver sets all three here (window.c:2411-2422).
        // InitWindowInfo is Proton-specific — parse raw bytes instead of typed struct.
        // Layout: header(12) + handle(u32@12) + style(u32@16) + ex_style(u32@20) + is_unicode(i16@24)
        if buf.len() >= 26 {
            let handle = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
            let style = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
            let ex_style = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
            let is_unicode = i16::from_le_bytes([buf[24], buf[25]]);
            if let Some(ws) = self.window_states.get_mut(&handle) {
                ws.style = style;
                ws.ex_style = ex_style;
                ws.is_unicode = is_unicode;
                ws.paint_flags |= 0x0040; // PAINT_NONCLIENT (Wine window.c:2421)
            }
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_set_irp_result(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // IRP completion acknowledgment. Games don't typically generate device
        // IRPs directly — winedevice.exe does. Acknowledge for now.
        reply_fixed(&SetIrpResultReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
        })
    }

    pub(crate) fn handle_create_debug_obj(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        // Wine calls NtCreateDebugObject during process creation.
        // DbgUiConnectToDbg checks if TEB already has a debug object and returns
        // early if so. We must return a valid handle so DbgUiSetThreadDebugObject
        // stores it, preventing infinite retry loops.
        let pid = self.client_pid(client_fd as RawFd);
        if let Some(evt) = self.get_or_create_event(true, false) {
            let oid = self.state.alloc_object_id();
            // Allocate handle WITH fd so get_handle_fd can return it.
            // Without an fd, Wine loops on get_handle_fd forever.
            // The fd type doesn't matter — Wine just needs a cacheable result.
            let evt_fd = evt.fd();
            if let Some(process) = self.state.processes.get_mut(&pid) {
                let h = process.handles.allocate_full(
                    crate::objects::HandleEntry::with_fd(oid, evt_fd, crate::objects::FD_TYPE_FILE, 0x001F000F, 0x20)
                );
                if h != 0 {
                    self.ntsync_objects.insert((pid, h), (evt, 1));
                    self.ntsync_objects_created += 1;
                    return reply_fixed(&CreateDebugObjReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        handle: h,
                        _pad_0: [0; 4],
                    });
                }
            }
        }
        // Fallback: return a handle value anyway so DbgUiSetThreadDebugObject stores it
        reply_fixed(&CreateDebugObjReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: 0x4, // dummy handle
            _pad_0: [0; 4],
        })
    }


    /// Wine ref: server/debugger.c:DECL_HANDLER(debug_process)
    ///
    /// Attach or detach a debug object from a process. Triskelion doesn't
    /// run a real debugger event loop — we never deliver debug events to
    /// the attached debugger — so this is a stateless success ack. Games
    /// that try to attach as their own debugger (Steam DRM, anti-tamper
    /// integrity-check loops) get a green light but no events. Their wait
    /// loops timeout cleanly via wait_debug_event below.
    ///
    /// The previous behavior was to fall through to the auto-stub path,
    /// which returned STATUS_NOT_IMPLEMENTED and the calling Wine code
    /// then NULL-derefed the missing debug_obj on the next operation.
    pub(crate) fn handle_debug_process(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() < std::mem::size_of::<DebugProcessRequest>() {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        }
        // We don't track the attach state — we have no debugger to deliver
        // events to anyway. Always succeed; the caller proceeds with a valid
        // (but inert) debug relationship.
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    /// Wine ref: server/debugger.c:DECL_HANDLER(wait_debug_event)
    ///
    /// Polled by the debugger thread to fetch the next debug event. We never
    /// queue events, so we always return the "no event yet" state — Wine's
    /// reference does the same when its event_queue is empty by writing the
    /// 4-byte `DbgIdle` (= 0) state code into the reply vararg slot. Pid/tid
    /// stay zero; the debugger sees "no sender" and falls back to its idle
    /// path. Reply struct: pid + tid (both u32), then VARARG(event,debug_event).
    pub(crate) fn handle_wait_debug_event(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() < std::mem::size_of::<WaitDebugEventRequest>() {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        }
        // 4-byte vararg = DbgIdle state. Wine: enum DEBUG_STATE { DbgIdle = 0, ... }
        let dbg_idle: i32 = 0;
        let vararg = dbg_idle.to_le_bytes();
        let reply = WaitDebugEventReply {
            header: ReplyHeader { error: 0, reply_size: vararg.len() as u32 },
            pid: 0,
            tid: 0,
        };
        reply_vararg(&reply, &vararg)
    }


    /// Wine ref: server/debugger.c:DECL_HANDLER(continue_debug_event)
    ///
    /// Resume a debugged process after the debugger handled a debug event.
    /// Since wait_debug_event never returns events, this should never get
    /// called with a real (pid, tid) — but games still call it after their
    /// debugger setup as part of init. Validate the continue status (Wine's
    /// reference rejects unknown values with STATUS_INVALID_PARAMETER) and
    /// return success.
    pub(crate) fn handle_continue_debug_event(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<ContinueDebugEventRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const ContinueDebugEventRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        // Wine accepts: DBG_EXCEPTION_NOT_HANDLED (0x80010001), DBG_EXCEPTION_HANDLED
        // (0x00010001), DBG_CONTINUE (0x00010002), DBG_REPLY_LATER (0x40010001).
        let valid = matches!(req.status, 0x80010001 | 0x00010001 | 0x00010002 | 0x40010001);
        if !valid {
            return reply_fixed(&ReplyHeader { error: 0xC000_000D, reply_size: 0 }); // STATUS_INVALID_PARAMETER
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_create_device_manager(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        // ntoskrnl calls create_device_manager to get a handle for device I/O.
        // It dereferences the returned handle — must be non-zero.
        // Back it with an ntsync event so get_handle_fd returns a valid fd.
        let pid = self.client_pid(client_fd as RawFd);
        if let Some(evt) = self.get_or_create_event(true, false) {
            let oid = self.state.alloc_object_id();
            let evt_fd = evt.fd();
            if let Some(process) = self.state.processes.get_mut(&pid) {
                let h = process.handles.allocate_full(
                    crate::objects::HandleEntry::with_fd(oid, evt_fd, crate::objects::FD_TYPE_FILE, 0x001F000F, 0x20)
                );
                if h != 0 {
                    self.ntsync_objects.insert((pid, h), (evt, 1));
                    self.ntsync_objects_created += 1;
                    return reply_fixed(&CreateDeviceManagerReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        handle: h,
                        _pad_0: [0; 4],
                    });
                }
            }
        }
        reply_fixed(&CreateDeviceManagerReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: 0x4,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_set_kernel_object_ptr(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetKernelObjectPtrRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetKernelObjectPtrRequest) }
        } else {
            return reply_fixed(&SetKernelObjectPtrReply { header: ReplyHeader { error: 0xC000000D, reply_size: 0 } });
        };
        let pid = self.client_pid(client_fd as RawFd);
        self.kernel_object_ptrs.insert((pid, req.manager, req.handle), req.user_ptr);
        reply_fixed(&SetKernelObjectPtrReply { header: ReplyHeader { error: 0, reply_size: 0 } })
    }

    pub(crate) fn handle_get_kernel_object_ptr(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetKernelObjectPtrRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetKernelObjectPtrRequest) }
        } else {
            return reply_fixed(&GetKernelObjectPtrReply { header: ReplyHeader { error: 0xC000000D, reply_size: 0 }, user_ptr: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let user_ptr = self.kernel_object_ptrs.get(&(pid, req.manager, req.handle)).copied().unwrap_or(0);
        reply_fixed(&GetKernelObjectPtrReply { header: ReplyHeader { error: 0, reply_size: 0 }, user_ptr })
    }

    pub(crate) fn handle_grab_kernel_object(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<GrabKernelObjectRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GrabKernelObjectRequest) }
        } else {
            return reply_fixed(&GrabKernelObjectReply { header: ReplyHeader { error: 0xC000000D, reply_size: 0 } });
        };
        // Mark as grabbed — for our simplified model, existence in the map is sufficient.
        reply_fixed(&GrabKernelObjectReply { header: ReplyHeader { error: 0, reply_size: 0 } })
    }

    pub(crate) fn handle_release_kernel_object(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<ReleaseKernelObjectRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const ReleaseKernelObjectRequest) }
        } else {
            return reply_fixed(&ReleaseKernelObjectReply { header: ReplyHeader { error: 0xC000000D, reply_size: 0 } });
        };
        let pid = self.client_pid(client_fd as RawFd);
        // Remove by user_ptr: find and remove the entry matching this manager + user_ptr
        self.kernel_object_ptrs.retain(|&(p, m, _), &mut ptr| {
            !(p == pid && m == req.manager && ptr == req.user_ptr)
        });
        reply_fixed(&ReleaseKernelObjectReply { header: ReplyHeader { error: 0, reply_size: 0 } })
    }

    pub(crate) fn handle_get_kernel_object_handle(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetKernelObjectHandleRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetKernelObjectHandleRequest) }
        } else {
            return reply_fixed(&GetKernelObjectHandleReply { header: ReplyHeader { error: 0xC000000D, reply_size: 0 }, handle: 0, _pad_0: [0; 4] });
        };
        let pid = self.client_pid(client_fd as RawFd);
        // Reverse lookup: find object handle by user_ptr for this manager
        let handle = self.kernel_object_ptrs.iter()
            .find(|((p, m, _), ptr)| *p == pid && *m == req.manager && **ptr == req.user_ptr)
            .map(|((_, _, h), _)| *h)
            .unwrap_or(0);
        reply_fixed(&GetKernelObjectHandleReply {
            header: ReplyHeader { error: if handle != 0 { 0 } else { 0xC0000034 }, reply_size: 0 }, // STATUS_OBJECT_NAME_NOT_FOUND
            handle,
            _pad_0: [0; 4],
        })
    }
}
