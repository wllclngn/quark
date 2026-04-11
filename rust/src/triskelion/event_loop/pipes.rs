// Named pipe lifecycle and FSCTL ioctl dispatch

use super::*;
#[allow(unused_variables)]


// Async metadata from FSCTL_PIPE_LISTEN ioctl — needed to deliver APC_ASYNC_IO.
#[derive(Debug, Clone, Copy)]
pub(super) struct PipeListenAsync {
    pub(super) server_client_fd: RawFd,  // the server thread's client_fd (to find wait_fd)
    pub(super) cookie: u64,              // select cookie for wake_up_reply
    pub(super) user: u64,                // async user ptr (for irp_completion)
    pub(super) sb: u64,                  // iosb client ptr
    pub(super) _async_event: u32,         // 0 = sync, non-zero = overlapped mode
    pub(super) user_arg: u64,            // for completed_ioctls key
}

// Named pipe state tracking for FSCTL_PIPE_LISTEN / CreateFile connection.
#[derive(Debug)]
pub(super) struct NamedPipeInfo {
    pub(super) server_pid: u32,
    server_handle: u32,         // handle in server process's handle table
    pub(super) client_data_fd: RawFd,      // client's end (held until a client connects)
    state: PipeState,
    // When FSCTL_PIPE_LISTEN is called, we create a ntsync event and
    // return its handle as the Ioctl wait handle. When a client connects
    // via CreateFile, we signal this event to wake the listening thread.
    pub(super) listen_event: Option<(u32, u32)>, // (pid, handle) of the listen wait event
    pub(super) listen_async: Option<PipeListenAsync>, // full async metadata for APC delivery
}

#[derive(Debug, PartialEq)]
pub(super) enum PipeState {
    Listening,
    Connected,
}

// Pending PIPE_WAIT waiter: client blocked waiting for a named pipe listener.
// Signaled when FSCTL_PIPE_LISTEN or create_named_pipe adds a Listening instance.
pub(super) struct PendingPipeWaiter {
    pub(super) pid: u32,
    pub(super) wait_handle: u32,
}

impl EventLoop {

    pub(crate) fn handle_create_named_pipe(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<CreateNamedPipeRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const CreateNamedPipeRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Extract pipe name from objattr vararg
        let pipe_name = extract_objattr_name(buf)
            .unwrap_or_default();

        // Create socketpair for the pipe data channel
        let mut fds = [0i32; 2];
        let ret = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) };
        if ret < 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 }); // NO_MEMORY
        }

        let oid = self.state.alloc_object_id();
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .unwrap_or(0);
        let handle = if let Some(process) = self.state.processes.get_mut(&pid) {
            process.handles.allocate_full(
                crate::objects::HandleEntry::with_fd(oid, fds[0], crate::objects::FD_TYPE_DEVICE, 0x001F01FF, 0x20)
            )
        } else { 0 };

        log_info!("PIPE_CREATE: name=\"{pipe_name}\" pid={pid} handle={handle:#x}");

        // Track pipe data fd for ntsync signaling
        self.side_tables.pipe_handles.insert((pid, handle), super::PipeHandle {
            data_fd: fds[0],
        });

        // Register in named pipe registry (keep client fd for later connection)
        let created = if !pipe_name.is_empty() {
            let instances = self.named_pipes.entry(pipe_name.clone()).or_default();
            let already_exists = !instances.is_empty();
            instances.push(NamedPipeInfo {
                server_pid: pid,
                server_handle: handle,
                client_data_fd: fds[1],  // held until a client connects
                state: PipeState::Listening,
                listen_event: None,
                listen_async: None,
            });
            // Wake any clients blocked in PIPE_WAIT — new Listening instance.
            self.wake_pipe_waiters(&pipe_name);
            if already_exists { 0i32 } else { 1i32 }
        } else {
            // Unnamed pipe -- keep both ends alive. The client end (fds[1])
            // gets its own handle in the same process. The parent will
            // DuplicateHandle it into the child process for stdin/stdout/stderr.
            // Wine's wineserver keeps both pipe_end objects alive until explicitly
            // closed. Closing fds[1] here would cause EPIPE on the child's end,
            // deadlocking services.exe -> rpcss.exe pipe communication.
            let client_oid = self.state.alloc_object_id();
            if let Some(process) = self.state.processes.get_mut(&pid) {
                let client_entry = crate::objects::HandleEntry::with_fd(
                    client_oid, fds[1], crate::objects::FD_TYPE_DEVICE, 0x0012019F, 0
                );
                self.unnamed_pipe_client_handles.insert(handle, process.handles.allocate_full(client_entry));
            }
            1
        };


        let reply = CreateNamedPipeReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            created,
        };
        reply_fixed(&reply)
    }

    pub(crate) fn handle_ioctl(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<IoctlRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const IoctlRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // async_data layout: handle(u32) event(u32) iosb(u64) user(u64) apc(u64) apc_context(u64)
        let async_handle = u32::from_le_bytes([req.r#async[0], req.r#async[1], req.r#async[2], req.r#async[3]]);
        let async_event = u32::from_le_bytes([req.r#async[4], req.r#async[5], req.r#async[6], req.r#async[7]]);
        let async_iosb = u64::from_le_bytes([
            req.r#async[8], req.r#async[9], req.r#async[10], req.r#async[11],
            req.r#async[12], req.r#async[13], req.r#async[14], req.r#async[15],
        ]);
        let async_user = u64::from_le_bytes([
            req.r#async[16], req.r#async[17], req.r#async[18], req.r#async[19],
            req.r#async[20], req.r#async[21], req.r#async[22], req.r#async[23],
        ]);

        const FSCTL_PIPE_LISTEN: u32 = 0x0011_0008;
        const FSCTL_PIPE_DISCONNECT: u32 = 0x0011_0004;
        const FSCTL_PIPE_WAIT: u32 = 0x0011_0018;
        const FSCTL_PIPE_TRANSCEIVE: u32 = 0x0011_C017;

        match req.code {
            FSCTL_PIPE_LISTEN => {
                // Find which named pipe instance this handle belongs to
                let pid = self.client_pid(client_fd as RawFd);
                let pipe_name = self.named_pipes.iter()
                    .find(|(_, instances)| instances.iter().any(|info| info.server_pid == pid && info.server_handle == async_handle))
                    .map(|(name, _)| name.clone());

                if let Some(pipe_name) = pipe_name {
                    log_info!("PIPE_LISTEN: name=\"{pipe_name}\" pid={pid} handle={async_handle:#x} overlapped={}", async_event != 0);

                    if async_event != 0 {
                        // Overlapped I/O: the caller provided their own event handle.
                        // Reset the event — matches stock wineserver's create_async
                        // (async.c:369). Without this, a stale signal from the previous
                        // completion causes WaitForMultipleObjects to return immediately,
                        // rpcrt4 sees STATUS_PENDING in the IOSB, and floods pipe instances.
                        if let Some((obj, _)) = self.ntsync_objects.get(&(pid, async_event)) {
                            let _ = obj.event_reset();
                        }
                        if let Some(instances) = self.named_pipes.get_mut(&pipe_name) {
                            if let Some(info) = instances.iter_mut().find(|i| i.server_pid == pid && i.server_handle == async_handle) {
                                // Auto-disconnect: if pipe was Connected (previous client left
                                // without FSCTL_PIPE_DISCONNECT), create fresh socketpair.
                                if info.state == PipeState::Connected {
                                    let mut fds = [0i32; 2];
                                    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } == 0 {
                                        if let Some(process) = self.state.processes.get_mut(&pid) {
                                            if let Some(entry) = process.handles.get_mut(async_handle) {
                                                if let Some(old_fd) = entry.fd { unsafe { libc::close(old_fd); } }
                                                entry.fd = Some(fds[0]);
                                            }
                                        }
                                        if info.client_data_fd >= 0 { unsafe { libc::close(info.client_data_fd); } }
                                        info.client_data_fd = fds[1];
                                    }
                                }
                                info.state = PipeState::Listening;
                                info.listen_event = Some((pid, async_event));
                                info.listen_async = Some(PipeListenAsync {
                                    server_client_fd: client_fd,
                                    cookie: 0,
                                    user: async_user,
                                    sb: async_iosb,
                                    _async_event: async_event,
                                    user_arg: async_user,
                                });
                            }
                        }
                        // Wake any clients blocked in PIPE_WAIT for this pipe.
                        self.wake_pipe_waiters(&pipe_name);

                        // Overlapped: return STATUS_PENDING with no wait handle.
                        // The caller's async_event is the notification mechanism.
                        // Returning wait != 0 causes ntdll to enter select and BLOCK,
                        // which prevents services from signaling __wine_svcctlstarted.
                        reply_fixed(&IoctlReply {
                            header: ReplyHeader { error: 0x00000103, reply_size: 0 }, // STATUS_PENDING
                            wait: 0,
                            options: 0,
                        })
                    } else {
                        // Synchronous: create a ntsync event as wait handle so the
                        // caller blocks in Select until a client connects.
                        let wait_handle = if let Some(evt) = self.get_or_create_event(false, false) {
                            let oid = self.state.alloc_object_id();
                            let h = if let Some(process) = self.state.processes.get_mut(&pid) {
                                let entry = crate::objects::HandleEntry::with_fd(
                                    oid, -1, crate::objects::FD_TYPE_FILE, 0x001F0003, 0x20
                                );
                                process.handles.allocate_full(entry)
                            } else { 0 };
                            if h != 0 {
                                self.insert_recyclable_event(pid, h, evt, 1); // INTERNAL
                            }
                            h
                        } else { 0 };

                        if wait_handle != 0 {
                            if let Some(instances) = self.named_pipes.get_mut(&pipe_name) {
                                if let Some(info) = instances.iter_mut().find(|i| i.server_pid == pid && i.server_handle == async_handle) {
                                    // Auto-disconnect (same as overlapped path)
                                    if info.state == PipeState::Connected {
                                        let mut fds = [0i32; 2];
                                        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } == 0 {
                                            if let Some(process) = self.state.processes.get_mut(&pid) {
                                                if let Some(entry) = process.handles.get_mut(async_handle) {
                                                    if let Some(old_fd) = entry.fd { unsafe { libc::close(old_fd); } }
                                                    entry.fd = Some(fds[0]);
                                                }
                                            }
                                            if info.client_data_fd >= 0 { unsafe { libc::close(info.client_data_fd); } }
                                            info.client_data_fd = fds[1];
                                        }
                                    }
                                    info.state = PipeState::Listening;
                                    info.listen_event = Some((pid, wait_handle));
                                    info.listen_async = Some(PipeListenAsync {
                                    server_client_fd: client_fd,
                                    cookie: 0,
                                    user: async_user,
                                    sb: async_iosb,
_async_event: 0, // sync mode — no overlapped event
                                    user_arg: async_user,
                                });
                                }
                            }
                            // Wake any clients blocked in PIPE_WAIT for this pipe.
                            self.wake_pipe_waiters(&pipe_name);

                            // Return STATUS_PENDING + wait handle — client will Select on it
                            reply_fixed(&IoctlReply {
                                header: ReplyHeader { error: 0x00000103, reply_size: 0 }, // STATUS_PENDING
                                wait: wait_handle,
                                options: 0,
                            })
                        } else {
                            // No ntsync — return success immediately (best effort)
                            reply_fixed(&IoctlReply {
                                header: ReplyHeader { error: 0, reply_size: 0 },
                                wait: 0,
                                options: 0,
                            })
                        }
                    }
                } else {
                    // Not a named pipe handle — return success (stub)
                    reply_fixed(&IoctlReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        wait: 0,
                        options: 0,
                    })
                }
            }
            FSCTL_PIPE_DISCONNECT => {
                // Disconnect a pipe instance — create new socketpair and reset to listening.
                // The old socketpair is dead (client closed their end → server got EOF).
                // A fresh socketpair lets the next client connect cleanly.
                let pid = self.client_pid(client_fd as RawFd);
                let pipe_name = self.named_pipes.iter()
                    .find(|(_, instances)| instances.iter().any(|info| info.server_pid == pid && info.server_handle == async_handle))
                    .map(|(name, _)| name.clone());
                if let Some(name) = pipe_name {
                    if let Some(instances) = self.named_pipes.get_mut(&name) {
                        if let Some(info) = instances.iter_mut().find(|i| i.server_pid == pid && i.server_handle == async_handle) {
                            // Create new socketpair for this instance
                            let mut fds = [0i32; 2];
                            if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) } == 0 {
                                // Update server handle's fd to new server end
                                if let Some(process) = self.state.processes.get_mut(&pid) {
                                    if let Some(entry) = process.handles.get_mut(info.server_handle) {
                                        if let Some(old_fd) = entry.fd {
                                            unsafe { libc::close(old_fd); }
                                        }
                                        entry.fd = Some(fds[0]);
                                    }
                                }
                                if info.client_data_fd >= 0 { unsafe { libc::close(info.client_data_fd); } }
                                info.client_data_fd = fds[1];
                            }
                            info.state = PipeState::Listening;
                            info.listen_event = None;
                        }
                    }
                }
                reply_fixed(&IoctlReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    wait: 0,
                    options: 0,
                })
            }
            FSCTL_PIPE_WAIT => {
                // Client wants to wait for a named pipe to become available.
                // Vararg: FILE_PIPE_WAIT_FOR_BUFFER { timeout(i64), name_length(u32),
                //         timeout_specified(u8), padding(u8), name[...] }
                let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
                if vararg.len() >= 14 {
                    let name_length = u32::from_le_bytes([vararg[8], vararg[9], vararg[10], vararg[11]]) as usize;
                    let name_offset = 14;
                    if vararg.len() >= name_offset + name_length {
                        let pipe_name = utf16le_to_string(&vararg[name_offset..name_offset + name_length]).to_lowercase();
                        let pid = self.client_pid(client_fd as RawFd);
                        log_info!("PIPE_WAIT: name=\"{pipe_name}\" pid={pid}");

                        // Check if a listener is already available
                        let has_listener = self.named_pipes.get(&pipe_name)
                            .map_or(false, |instances| instances.iter().any(|i| i.state == PipeState::Listening));

                        if has_listener {
                            return reply_fixed(&IoctlReply {
                                header: ReplyHeader { error: 0, reply_size: 0 },
                                wait: 0, options: 0,
                            });
                        }

                        // No listener available — block until PIPE_LISTEN fires or pipe is created.
                        // Handles both cases: pipe exists but no listener, OR pipe doesn't exist yet.
                        // Stock wineserver queues the waiter regardless. Without this, clients
                        // spin in a tight loop calling PIPE_WAIT for pipes not yet created (e.g. lrpc\irpcss).
                        {
                            let pid = self.client_pid(client_fd as RawFd);
                            let wait_handle = if let Some(evt) = self.get_or_create_event(false, false) {
                                let oid = self.state.alloc_object_id();
                                let h = if let Some(process) = self.state.processes.get_mut(&pid) {
                                    process.handles.allocate_full(
                                        crate::objects::HandleEntry::with_fd(
                                            oid, -1, crate::objects::FD_TYPE_FILE, 0x001F0003, 0x20
                                        )
                                    )
                                } else { 0 };
                                if h != 0 {
                                    self.insert_recyclable_event(pid, h, evt, 1); // INTERNAL
                                }
                                h
                            } else { 0 };

                            if wait_handle != 0 {
                                self.pending_pipe_waiters.entry(pipe_name).or_default().push(
                                    PendingPipeWaiter { pid, wait_handle }
                                );
                                return reply_fixed(&IoctlReply {
                                    header: ReplyHeader { error: 0x00000103, reply_size: 0 }, // STATUS_PENDING
                                    wait: wait_handle, options: 0,
                                });
                            }
                        }
                    }
                }
                // Couldn't create wait handle — return STATUS_OBJECT_NAME_NOT_FOUND
                // so the client knows the pipe isn't available yet.
                reply_fixed(&IoctlReply {
                    header: ReplyHeader { error: 0xC0000034, reply_size: 0 },
                    wait: 0, options: 0,
                })
            }
            FSCTL_PIPE_TRANSCEIVE => {
                // Atomic write+read on a connected named pipe (used by RPC).
                // Write the request data from vararg, then read the reply.
                let pid = self.client_pid(client_fd as RawFd);
                let entry_info = self.state.processes.get(&pid)
                    .and_then(|p| p.handles.get(async_handle))
                    .map(|e| (e.fd, e.options));

                let (fd, options) = match entry_info {
                    Some((Some(fd), options)) => (fd, options),
                    _ => {
                        return reply_fixed(&IoctlReply {
                            header: ReplyHeader { error: 0xC0000008, reply_size: 0 },
                            wait: 0, options: 0,
                        });
                    }
                };

                // Write the request data to the pipe
                let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
                if !vararg.is_empty() {
                    let written = unsafe {
                        libc::send(fd, vararg.as_ptr() as *const _, vararg.len(), libc::MSG_NOSIGNAL)
                    };
                    if written < 0 {
                        let _errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                        return reply_fixed(&IoctlReply {
                            header: ReplyHeader { error: 0xC000014B, reply_size: 0 }, // STATUS_PIPE_BROKEN
                            wait: 0, options: 0,
                        });
                    }
                }

                // Read the reply — non-blocking first
                let max_reply = max_reply_vararg(buf) as usize;
                let max_size = if max_reply > 0 { max_reply } else { 65536 };
                let mut read_buf = vec![0u8; max_size];
                let n = unsafe {
                    libc::recv(fd, read_buf.as_mut_ptr() as *mut libc::c_void,
                               max_size, libc::MSG_DONTWAIT)
                };

                if n > 0 {
                    read_buf.truncate(n as usize);
                    reply_vararg(&IoctlReply {
                        header: ReplyHeader { error: 0, reply_size: n as u32 },
                        wait: 0, options,
                    }, &read_buf)
                } else if n == 0 {
                    reply_fixed(&IoctlReply {
                        header: ReplyHeader { error: 0xC000014B, reply_size: 0 },
                        wait: 0, options: 0,
                    })
                } else {
                    // EAGAIN — reply not available yet. Register async read.
                    let wait = self.get_pipe_wait_handle(pid, async_handle);
                    if wait != 0 {
                        self.pending_pipe_reads.push(AsyncPipeRead {
                            pipe_fd: fd,
                            max_bytes: max_size,
                            pid,
                            wait_handle: wait,
                            client_fd: client_fd as RawFd,
                        });
                        reply_fixed(&IoctlReply {
                            header: ReplyHeader { error: 0x00000103, reply_size: 0 },
                            wait, options,
                        })
                    } else {
                        // No wait handle — do a blocking read (short timeout)
                        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                        let ready = unsafe { libc::poll(&mut pfd, 1, 1000) }; // 1s timeout
                        if ready > 0 {
                            let n = unsafe {
                                libc::recv(fd, read_buf.as_mut_ptr() as *mut libc::c_void,
                                           max_size, libc::MSG_DONTWAIT)
                            };
                            if n > 0 {
                                read_buf.truncate(n as usize);
                                return reply_vararg(&IoctlReply {
                                    header: ReplyHeader { error: 0, reply_size: n as u32 },
                                    wait: 0, options,
                                }, &read_buf);
                            }
                        }
                        reply_fixed(&IoctlReply {
                            header: ReplyHeader { error: 0xC00000B5, reply_size: 0 }, // STATUS_IO_TIMEOUT
                            wait: 0, options: 0,
                        })
                    }
                }
            }
            _ => {
                // Unknown ioctl — return success (stub)
                reply_fixed(&IoctlReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    wait: 0,
                    options: 0,
                })
            }
        }
    }


    /// Try to connect a client to a named pipe. Returns Some(reply) if the
    /// basename matches a registered pipe, None otherwise.
    /// Finds the first instance in Listening state (supports multiple server instances).
    pub(super) fn try_connect_named_pipe(&mut self, client_fd: i32, basename: &str, access: u32, options: u32) -> Option<Reply> {
        let pid = self.client_pid(client_fd as RawFd);
        // Check if any registered pipe matches this basename
        let instances = match self.named_pipes.get(basename) {
            Some(i) => i,
            None => return None,
        };
        log_info!("PIPE_CONNECT: name=\"{basename}\" client_pid={pid}");

        // Find the first listening instance
        let idx = instances.iter().position(|info| info.state == PipeState::Listening);
        if idx.is_none() {
            return Some(reply_fixed(&ReplyHeader { error: 0xC00000AE, reply_size: 0 })); // STATUS_PIPE_BUSY
        }
        let idx = idx.unwrap();

        // Extract info from the listening instance before mutating
        let client_data_fd = unsafe { libc::dup(instances[idx].client_data_fd) };
        if client_data_fd < 0 {
            return Some(reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 })); // NO_MEMORY
        }
        let listen_event = instances[idx].listen_event;
        let listen_async = instances[idx].listen_async;


        // Keep client_data_fd alive -- closing it kills the server end of the
        // socketpair (EPIPE on write). Mark as Connected, clear async metadata.
        {
            let info = &mut self.named_pipes.get_mut(basename).unwrap()[idx];
            info.state = PipeState::Connected;
            info.listen_async = None;
        }

        // Deliver pipe listen completion via APC — matching Wine's wineserver behavior.
        //
        // Wine's server calls check_wait() which checks APCs BEFORE handles.
        // If pending APC + SELECT_INTERRUPTIBLE → returns STATUS_KERNEL_APC
        // regardless of handle state. We replicate this by:
        //   1. Queue APC in pending_kernel_apcs
        //   2. Write STATUS_KERNEL_APC directly to wait_fd (bypasses ntsync kernel)
        //   3. Set apc_flag so the ntsync wait thread exits WITHOUT writing
        //   4. Store deferred event signal for after APC delivery
        //
        // The ntsync wait thread may return Signaled (other event) or Alerted,
        // but either way it sees apc_flag=true and exits without writing to wait_fd.
        // Our direct write is the ONLY write — no double-write race.
        if let Some(la) = listen_async {
            // Queue APC data — delivered on the thread's next Select
            let mut apc_data = [0u8; 28];
            apc_data[0..4].copy_from_slice(&2u32.to_le_bytes());   // APC_ASYNC_IO = 2
            apc_data[4..8].copy_from_slice(&0u32.to_le_bytes());   // STATUS_SUCCESS
            apc_data[8..16].copy_from_slice(&la.user.to_le_bytes());
            apc_data[16..24].copy_from_slice(&la.sb.to_le_bytes());
            apc_data[24..28].copy_from_slice(&0u32.to_le_bytes()); // result = 0
            self.pending_kernel_apcs
                .entry(la.server_client_fd)
                .or_default()
                .push(apc_data);

            // Defer event signal until prev_apc (after irp_completion writes IOSB).
            // Signaling before IOSB is written causes rpcrt4 to see STATUS_PENDING
            // and flood pipe instances (14K+).
            if let Some((evpid, evhandle)) = listen_event {
                self.deferred_event_signals
                    .entry(la.server_client_fd)
                    .or_default()
                    .push((evpid, evhandle));
            }

            // Store completion for get_async_result fallback
            let server_pid = self.clients.get(&la.server_client_fd)
                .map(|c| c.process_id).unwrap_or(0);
            self.completed_ioctls.insert((server_pid, la.user_arg), 0);

            // Post to completion port if the pipe handle is bound to one.
            // This wakes rpcrt4's worker thread via the I/O completion port.
            if let Some(pipe_handle) = self.named_pipes.iter()
                .find(|(_, instances)| instances.iter().any(|i| i.server_pid == server_pid && i.state == PipeState::Connected))
                .and_then(|(_, instances)| instances.iter().find(|i| i.server_pid == server_pid && i.state == PipeState::Connected))
                .map(|i| i.server_handle)
            {
                if let Some(&(port_handle, ckey)) = self.side_tables.completion_bindings.get(&(server_pid, pipe_handle)) {
                    let msg = super::CompletionMsg {
                        ckey,
                        cvalue: la.user as u64,
                        information: 0,
                        status: 0,
                    };
                    // Deliver directly to waiter or enqueue
                    if let Some(waiters) = self.completion_waiters.get_mut(&port_handle) {
                        if let Some(waiter) = waiters.pop() {
                            self.thread_completion_cache.insert(waiter.client_fd, msg);
                            if let Some((obj, _)) = self.ntsync_objects.get(&(waiter.pid, waiter.wait_handle)) {
                                let _ = obj.event_set();
                            }
                            if waiters.is_empty() {
                                self.completion_waiters.remove(&port_handle);
                            }
                        } else {
                            self.completion_queues.entry(port_handle).or_default().push(msg);
                        }
                    } else {
                        self.completion_queues.entry(port_handle).or_default().push(msg);
                    }
                }
            }

            // Resolve the effective cookie: la.cookie was captured in Select's
            // cookie-capture loop (line 269 of sync.rs). If the connect happens
            // before the thread enters Select, la.cookie is 0. Fall back to
            // current_wait_cookie stored in the Client struct when Select
            // spawned the ntsync worker thread.
            let _effective_cookie = if la.cookie != 0 {
                la.cookie
            } else {
                self.clients.get(&la.server_client_fd)
                    .map(|c| c.current_wait_cookie)
                    .unwrap_or(0)
            };

            // Wake the thread to process the system APC. Two mechanisms:
            // 1. Worker interrupt: wakes daemon-side ntsync worker (server-side wait)
            // 2. SIGUSR1: wakes thread from inproc ntsync wait. Wine's SIGUSR1 handler
            //    calls wait_suspend → server_select(SELECT_INTERRUPTIBLE) → our daemon
            //    delivers the APC inside the signal handler. This matches stock wineserver's
            //    send_thread_signal(thread, SIGUSR1) in queue_apc for system APCs when
            //    the thread is not in an apc_wait.
            if let Some(interrupt) = self.client_worker_interrupts.get(&la.server_client_fd) {
                let _ = interrupt.event_set();
            }
            let (unix_pid, unix_tid) = self.clients.get(&la.server_client_fd)
                .map(|c| (c.unix_pid, c.unix_tid))
                .unwrap_or((0, 0));
            if unix_tid != 0 {
                unsafe { libc::syscall(libc::SYS_tgkill, unix_pid, unix_tid, libc::SIGUSR1); }
            }
        }

        // Map generic access rights to specific file access rights.
        // Wine's ntdll checks specific rights (FILE_WRITE_DATA) not generic (GENERIC_WRITE).
        // Without this mapping, server_get_unix_fd fails with STATUS_ACCESS_DENIED.
        let mapped_access = Self::map_file_access(access);

        // Create a handle for the client process
        let oid = self.state.alloc_object_id();
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        let handle = if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                // FILE_SYNCHRONOUS_IO_NONALERT (0x20) forces Wine's NtReadFile into
                // synchronous poll-based reads. Without it, Wine treats pipe reads as
                // async, which requires server-side fd monitoring to complete.
                let pipe_options = options | 0x20;
                let h = process.handles.allocate_full(
                    crate::objects::HandleEntry::with_fd(oid, client_data_fd, crate::objects::FD_TYPE_DEVICE, mapped_access, pipe_options)
                );
                // Track pipe data fd for ntsync signaling
                self.side_tables.pipe_handles.insert((pid, h), super::PipeHandle {
                    data_fd: client_data_fd,
                });
                h
            } else { 0 }
        } else { 0 };

        Some(reply_fixed(&CreateFileReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        }))
    }

    pub(crate) fn handle_set_named_pipe_info(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        if buf.len() < std::mem::size_of::<SetNamedPipeInfoRequest>() {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        }
        // Stock wineserver records the flags so set_pipe_state can read them back.
        // Triskelion has no consumer for them — handler returns success without
        // storing. Reintroduce a real store + read path before any code starts
        // depending on the value.
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    // Wake all pending PIPE_WAIT waiters for a given pipe name.
    // Called when a pipe instance transitions to Listening state
    // (FSCTL_PIPE_LISTEN or create_named_pipe). Matches stock wineserver's
    // async_wake_up(&pipe->waiters, STATUS_SUCCESS).
    fn wake_pipe_waiters(&mut self, pipe_name: &str) {
        if let Some(waiters) = self.pending_pipe_waiters.remove(pipe_name) {
            log_info!("wake_pipe_waiters: \"{pipe_name}\" waking {} waiter(s)", waiters.len());
            for waiter in waiters {
                if let Some((obj, _)) = self.ntsync_objects.get(&(waiter.pid, waiter.wait_handle)) {
                    let _ = obj.event_set();
                }
            }
        }
    }
}
