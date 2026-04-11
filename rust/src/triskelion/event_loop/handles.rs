// Handle table operations — close, dup, get_fd, alloc

use super::*;
#[allow(unused_variables)]

impl EventLoop {

    // Helpers

    #[inline]
    pub(super) fn client_thread_id(&self, client_fd: RawFd) -> Option<thread_id_t> {
        self.clients.get(&client_fd)
            .and_then(|c| if c.thread_id != 0 { Some(c.thread_id) } else { None })
    }


    #[inline]
    pub(super) fn client_pid(&self, client_fd: RawFd) -> u32 {
        self.clients.get(&client_fd)
            .map(|c| c.process_id)
            .unwrap_or(0)
    }


    #[inline]
    pub(crate) fn handle_close_handle(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let tid = self.client_thread_id(client_fd as RawFd);
        let pid = tid.and_then(|t| self.state.threads.get(&t).map(|th| th.pid));

        if let Some(pid) = pid {
            if buf.len() >= std::mem::size_of::<CloseHandleRequest>() {
                let req: CloseHandleRequest = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr() as *const _)
                };
                // Clean up ntsync object
                self.remove_ntsync_obj(pid, req.handle);
                // Side tables: pipe data fd, io wait handle cache, fd_sent,
                // completion binding. purge() returns the io_wait_handle (if any)
                // so we can release its ntsync object too.
                if let Some(wh) = self.side_tables.purge(pid, req.handle) {
                    self.remove_ntsync_obj(pid, wh);
                }
                if let Some(process) = self.state.processes.get_mut(&pid) {
                    process.handles.close(req.handle);
                }
            }
        }

        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    pub(crate) fn handle_set_handle_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetHandleInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetHandleInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let old_options = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.handle))
            .map(|e| e.options)
            .unwrap_or(0);
        // HANDLE_FLAG_INHERIT = 1 maps to OBJ_INHERIT = 0x02 in options
        let old_flags = if old_options & 0x02 != 0 { 1i32 } else { 0i32 };
        // Apply mask: only change bits specified by mask
        if let Some(process) = self.state.processes.get_mut(&pid) {
            if let Some(entry) = process.handles.get_mut(req.handle) {
                if req.mask & 1 != 0 { // HANDLE_FLAG_INHERIT
                    if req.flags & 1 != 0 {
                        entry.options |= 0x02; // OBJ_INHERIT
                    } else {
                        entry.options &= !0x02;
                    }
                }
            }
        }
        reply_fixed(&SetHandleInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            old_flags,
            _pad_0: [0; 4],
        })
    }


    pub(crate) fn handle_get_handle_fd(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetHandleFdRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetHandleFdRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        // Temporary logging removed (was flooding log with 100k+ entries)

        // Resolve pseudo-handles to real handles (matches stock Wine's get_magic_handle).
        // These are Windows API constants for "current process/thread" that clients pass
        // to server calls. Stock wineserver translates them to real kernel objects.
        // We don't have kernel objects, but these handles don't have unix fds either,
        // so return a cacheable OBJECT_TYPE_MISMATCH to stop the retry loop.
        let handle = match req.handle {
            // Wine pseudo-handles: current process (-1), current thread (-2),
            // process token (-4, 0xFFFFFFFC), thread token (-5, 0xFFFFFFFB),
            // effective token (-6, 0xFFFFFFFA). These don't have unix fds.
            // Return cacheable error so Wine caches it and doesn't loop.
            0xFFFFFFFF | 0x7FFFFFFF | 0xFFFFFFFE | 0xFFFFFFFC | 0xFFFFFFFB | 0xFFFFFFFA => {
                return reply_fixed(&GetHandleFdReply {
                    header: ReplyHeader { error: 0xC0000024, reply_size: 0 },
                    r#type: 0, cacheable: 1, access: 0, options: 0,
                });
            }
            h if h >= 0xFFFFFFF0 => {
                // Overflow sentinel — 64-bit handle truncated to 32 bits.
                // Return cacheable OBJECT_TYPE_MISMATCH so Wine caches the error
                // and stops retrying. Without cacheable=1, Wine loops infinitely.
                // Do NOT send /dev/null fd — that corrupts Wine's fd cache and
                // causes fd queue desync.
                return reply_fixed(&GetHandleFdReply {
                    header: ReplyHeader { error: 0xC0000024, reply_size: 0 },
                    r#type: 0, cacheable: 1, access: 0, options: 0,
                });
            }
            0 => {
                return reply_fixed(&GetHandleFdReply {
                    header: ReplyHeader { error: 0xC0000008, reply_size: 0 },
                    r#type: 0, cacheable: 0, access: 0, options: 0,
                });
            }
            h => h,
        };

        // pid already resolved at top of function

        // Look up the handle's fd and metadata
        let entry_info = pid.and_then(|p| self.state.processes.get(&p))
            .and_then(|p| p.handles.get(handle))
            .map(|e| (e.fd, e.obj_type, e.access, e.options));

        if let Some((unix_fd_opt, obj_type, access, options)) = entry_info {
            // Named pipes: send the fd normally. Wine needs it for NtFsControlFile
            // (FSCTL_PIPE_LISTEN, etc.) to determine fd type and proceed with ioctls.
            // Read/write operations on pipes still go through server-side opcodes
            // because Wine's ntdll checks the fd type and routes accordingly.

            // Only return success if we actually have an fd to send.
            // The client calls receive_fd() after a successful reply —
            // if we don't send an fd, receive_fd blocks forever.
            if let Some(unix_fd) = unix_fd_opt {
                let p = pid.unwrap_or(0);
                let key = (p, handle);
                if self.side_tables.fd_sent.contains(&key) {
                    self.side_tables.fd_sent.remove(&key);
                }

                if cfg!(debug_assertions) {
                    let link = format!("/proc/self/fd/{unix_fd}");
                    if let Ok(target) = std::fs::read_link(&link) {
                        let t = target.display().to_string();
                        if t.contains(".exe") {
                            let mut st: libc::stat = unsafe { std::mem::zeroed() };
                            let size = if unsafe { libc::fstat(unix_fd, &mut st) } == 0 { st.st_size } else { -1 };
                            log_info!("GET_HANDLE_FD_EXE: handle={handle:#x} fd={unix_fd} size={size} type={obj_type} target=\"{t}\"");
                        }
                    }
                }

                // Store fd in pending_fd -- worker sends it BEFORE the reply.
                // This eliminates the msg_fd race where another handler's fd
                // could interleave on the shared msg_fd between send and reply.
                if let Some(client) = self.clients.get_mut(&(client_fd as RawFd)) {
                    let dup_fd = unsafe { libc::fcntl(unix_fd, libc::F_DUPFD_CLOEXEC, 0) };
                    if dup_fd >= 0 {
                        client.pending_fd = Some((dup_fd, handle));
                    } else {
                        log_error!("PENDING_FD_DUP_FAIL: client_fd={client_fd} handle={handle:#x} errno={}", std::io::Error::last_os_error());
                    }
                    self.side_tables.fd_sent.insert(key);
                } else {
                    log_error!("PENDING_FD_NO_CLIENT: client_fd={client_fd}");
                }
                log_info!("GET_HANDLE_FD: handle={handle:#x} fd={unix_fd} type={obj_type} access={access:#x} options={options:#x} client_fd={client_fd}");
                let reply = GetHandleFdReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    r#type: obj_type as i32,
                    cacheable: 1,
                    access,
                    options,
                };
                return reply_fixed(&reply);
            }
            // Handle exists but has no unix fd (sync objects, process handles, etc.)
            // STATUS_OBJECT_TYPE_MISMATCH + cacheable=1: Wine caches this and stops retrying.
            return reply_fixed(&GetHandleFdReply {
                header: ReplyHeader { error: 0xC0000024, reply_size: 0 },
                r#type: 0, cacheable: 1, access: 0, options: 0,
            });
        }

        // Ntsync-only objects (events, mutexes, semaphores, jobs): no unix fd.
        // STATUS_OBJECT_TYPE_MISMATCH + cacheable=1: Wine caches and stops retrying.
        if let Some(p) = pid {
            if self.ntsync_objects.contains_key(&(p, handle)) {
                return reply_fixed(&GetHandleFdReply {
                    header: ReplyHeader { error: 0xC0000024, reply_size: 0 },
                    r#type: 0, cacheable: 1, access: 0, options: 0,
                });
            }
        }

        // Handle not found — log for diagnosis
        let has_ntsync = pid.map(|p| self.ntsync_objects.contains_key(&(p, handle))).unwrap_or(false);
        let table_info = pid.and_then(|p| self.state.processes.get(&p))
            .map(|p| {
                let idx = (handle >> 2) as usize;
                let table_len = p.handles.slot_count();
                let slot_occupied = p.handles.get(handle).is_some();
                format!("table_len={table_len} idx={idx} slot_occupied={slot_occupied}")
            }).unwrap_or_else(|| "no process".to_string());
        log_warn!("GetHandleFd MISS handle={:#x} fd={client_fd} pid={:?} has_ntsync={has_ntsync} {table_info}", handle, pid);
        reply_fixed(&GetHandleFdReply {
            header: ReplyHeader { error: 0xC0000008, reply_size: 0 },
            r#type: 0, cacheable: 0, access: 0, options: 0,
        })
    }


    pub(crate) fn handle_open_directory(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        let reply = OpenDirectoryReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }

    pub(crate) fn handle_open_symlink(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        reply_fixed(&OpenSymlinkReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_query_symlink(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Return \BaseNamedObjects as the symlink target.
        // Wine's NT namespace: \Sessions\BNOLINKS\<session_id> → \BaseNamedObjects
        // All session-scoped named objects live in \BaseNamedObjects.
        let target: Vec<u8> = "\\BaseNamedObjects".encode_utf16()
            .flat_map(|c| c.to_le_bytes()).collect();
        let reply = QuerySymlinkReply {
            header: ReplyHeader { error: 0, reply_size: target.len() as u32 },
            total: target.len() as u32,
            _pad_0: [0; 4],
        };
        reply_vararg(&reply, &target)
    }

    pub(crate) fn handle_create_symlink(&mut self, client_fd: i32, _buf: &[u8]) -> Reply {
        let handle = self.alloc_waitable_handle_for_client(client_fd);
        if handle == 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 });
        }
        reply_fixed(&CreateSymlinkReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_dup_handle(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<DupHandleRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const DupHandleRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let caller_pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });

        // Resolve source and destination process PIDs from handles.
        // req.src_process/dst_process are process handles in the caller's table.
        // object_id on a process handle stores the target pid.
        let src_pid = if req.src_process == 0xFFFFFFFF || req.src_process == 0x7FFFFFFF {
            caller_pid // pseudo-handle = current process
        } else {
            caller_pid.and_then(|cp| self.state.processes.get(&cp))
                .and_then(|p| p.handles.get(req.src_process))
                .map(|e| e.object_id as u32)
                .or(caller_pid) // fallback to caller
        };
        let dst_pid = if req.dst_process == 0xFFFFFFFF || req.dst_process == 0x7FFFFFFF {
            caller_pid
        } else {
            caller_pid.and_then(|cp| self.state.processes.get(&cp))
                .and_then(|p| p.handles.get(req.dst_process))
                .map(|e| e.object_id as u32)
                .or(caller_pid)
        };

        // Resolve pseudo-handles for src_handle (stock: get_magic_handle in handle.c)
        // 0xfffffffe = current thread, 0xffffffff/0x7fffffff = current process
        // 0xfffffffc = current process token, 0xfffffffb = current thread token
        let src_entry = match req.src_handle {
            0xFFFFFFFE => {
                // Current thread pseudo-handle
                let tid = self.clients.get(&(client_fd as RawFd)).map(|c| c.thread_id).unwrap_or(0);
                Some((tid as u64, None, 2, 0x1FFFFF, 0)) // THREAD_ALL_ACCESS
            }
            0xFFFFFFFF | 0x7FFFFFFF => {
                // Current process pseudo-handle
                let pid = caller_pid.unwrap_or(0);
                Some((pid as u64, None, 1, 0x1FFFFF, 0)) // PROCESS_ALL_ACCESS
            }
            0xFFFFFFFC => {
                // Current process token pseudo-handle
                Some((0, None, 5, 0xF01FF, 0)) // TOKEN_ALL_ACCESS
            }
            _ => {
                // Normal handle lookup in source process
                src_pid.and_then(|p| self.state.processes.get(&p))
                    .and_then(|p| p.handles.get(req.src_handle))
                    .map(|e| (e.object_id, e.fd, e.obj_type, e.access, e.options))
            }
        };

        if let Some((oid, fd, obj_type, access, options)) = src_entry {
            let new_fd = fd.map(|f| unsafe { libc::dup(f) });
            let new_access = if req.options & 2 != 0 { access } else { req.access }; // DUPLICATE_SAME_ACCESS
            let entry = crate::objects::HandleEntry {
                object_id: oid,
                fd: new_fd,
                obj_type,
                access: new_access,
                options,
            };
            // Allocate in the DESTINATION process
            let handle = if let Some(dp) = dst_pid {
                if let Some(process) = self.state.processes.get_mut(&dp) {
                    process.handles.allocate_full(entry)
                } else { 0 }
            } else { 0 };

            // Duplicate ntsync object from SOURCE process into DESTINATION process
            if handle != 0 {
                let mut registered = false;
                if let Some(sp) = src_pid {
                    if let Some(dp) = dst_pid {
                        if let Some((src_obj, sync_type)) = self.ntsync_objects.get(&(sp, req.src_handle)) {
                            let st = *sync_type;
                            if let Some(dup_obj) = src_obj.dup() {
                                self.ntsync_objects.insert((dp, handle), (Arc::new(dup_obj), st));
                                registered = true;
                            }
                        }
                    }
                }
                // Source had no ntsync entry — create one so Select doesn't fallback
                if !registered {
                    if let Some(dp) = dst_pid {
                        if let Some(obj) = self.get_or_create_event(true, false) {
                            self.ntsync_objects.insert((dp, handle), (obj, 1)); // INTERNAL
                        }
                    }
                }
            }

            let reply = DupHandleReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                handle,
                _pad_0: [0; 4],
            };
            reply_fixed(&reply)
        } else {
            let sp = src_pid.unwrap_or(0);
            let dp = dst_pid.unwrap_or(0);
            let table_info = src_pid.and_then(|p| self.state.processes.get(&p))
                .map(|p| format!("slots={}", p.handles.slot_count()))
                .unwrap_or_else(|| "NO_PROCESS".into());
            log_error!("DupHandle FAIL: src_process={:#x} dst_process={:#x} src_handle={:#x} caller_pid={:?} resolved_src={} resolved_dst={} options={:#x} {table_info}",
                req.src_process, req.dst_process, req.src_handle, caller_pid, sp, dp, req.options);
            reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }) // INVALID_HANDLE
        }
    }


    pub(crate) fn handle_get_object_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetObjectInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetObjectInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let access = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.handle))
            .map(|e| e.access)
            .unwrap_or(0x1F0FFF);
        reply_fixed(&GetObjectInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            access,
            ref_count: 1,
            handle_count: 1,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_get_object_name(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let pid = self.client_pid(client_fd as RawFd);
        let max_reply_val = max_reply_vararg(buf);
        let req = if buf.len() >= std::mem::size_of::<GetObjectNameRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetObjectNameRequest) }
        } else {
            return reply_fixed(&GetObjectNameReply {
                header: ReplyHeader { error: 0xc0000023, reply_size: 0 },
                total: 0,
                _pad_0: [0; 4],
            });
        };
        let handle = req.handle;

        let fd = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(handle))
            .and_then(|entry| entry.fd);

        let fd = match fd {
            Some(f) => f,
            None => {
                return reply_fixed(&GetObjectNameReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    total: 0,
                    _pad_0: [0; 4],
                });
            }
        };

        let link = format!("/proc/self/fd/{}", fd);
        let unix_path = match std::fs::read_link(&link) {
            Ok(p) => p,
            Err(_) => {
                return reply_fixed(&GetObjectNameReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    total: 0,
                    _pad_0: [0; 4],
                });
            }
        };

        let path_str = match unix_path.to_str() {
            Some(s) => s,
            None => {
                return reply_fixed(&GetObjectNameReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    total: 0,
                    _pad_0: [0; 4],
                });
            }
        };

        let nt_path = format!("\\??\\Z:{}", path_str.replace('/', "\\"));
        let utf16: Vec<u8> = nt_path.encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .collect();

        let max_reply = max_reply_val as usize;
        let total = utf16.len() as u32;
        let send_len = utf16.len().min(max_reply);

        reply_vararg(
            &GetObjectNameReply {
                header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                total,
                _pad_0: [0; 4],
            },
            &utf16[..send_len],
        )
    }

    pub(crate) fn handle_get_object_type(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_compare_objects(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<CompareObjectsRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const CompareObjectsRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let oid1 = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.first))
            .map(|e| e.object_id);
        let oid2 = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.second))
            .map(|e| e.object_id);
        let error = match (oid1, oid2) {
            (Some(a), Some(b)) if a == b => 0,
            (Some(_), Some(_)) => 0xC0000460, // STATUS_NOT_SAME_OBJECT
            _ => 0xC0000008, // STATUS_INVALID_HANDLE
        };
        reply_fixed(&ReplyHeader { error, reply_size: 0 })
    }

    pub(crate) fn handle_get_handle_unix_name(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetHandleUnixNameRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetHandleUnixNameRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let handle_fd = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.handle))
            .and_then(|e| e.fd);
        if let Some(fd) = handle_fd {
            let link = format!("/proc/self/fd/{fd}");
            if let Ok(path) = std::fs::read_link(&link) {
                if let Some(path_str) = path.to_str() {
                    let path_bytes = path_str.as_bytes();
                    let max = max_reply_vararg(buf) as usize;
                    let send_len = path_bytes.len().min(max);
                    let reply = GetHandleUnixNameReply {
                        header: ReplyHeader { error: 0, reply_size: send_len as u32 },
                        name_len: path_bytes.len() as u32,
                        _pad_0: [0; 4],
                    };
                    return reply_vararg(&reply, &path_bytes[..send_len]);
                }
            }
        }
        reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 })
    }


    // LUID
    pub(crate) fn handle_allocate_locally_unique_id(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Increment a counter for unique LUIDs
        let id = self.state.alloc_object_id();
        let mut luid = [0u8; 8];
        luid[0..4].copy_from_slice(&(id as u32).to_le_bytes());
        reply_fixed(&AllocateLocallyUniqueIdReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            luid,
        })
    }

    // Helper: allocate a simple handle in the client's process
    pub(super) fn alloc_handle_for_client(&mut self, client_fd: i32, object_id: u64) -> obj_handle_t {
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                return process.handles.allocate(object_id);
            }
        }
        0
    }


    /// Allocate a handle backed by an eventfd (for waitable objects like events, keyed events, mutexes, semaphores)
    pub(super) fn alloc_waitable_handle_for_client(&mut self, client_fd: i32) -> obj_handle_t {
        let oid = self.state.alloc_object_id();
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if efd < 0 {
            log_error!("alloc_waitable_handle: eventfd() failed fd={client_fd}");
            return 0;
        }
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
        if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                let entry = crate::objects::HandleEntry::with_fd(oid, efd, crate::objects::FD_TYPE_FILE, 0x001F0003, 0x20);
                return process.handles.allocate_full(entry);
            }
        }
        unsafe { libc::close(efd); }
        log_error!("alloc_waitable_handle: RETURNED 0! fd={client_fd} pid={:?}", pid);
        0
    }


    // Map GENERIC_* access rights to FILE_* specific rights.
    // Stock wineserver does this in alloc_handle → obj->ops->map_access().
    // Without this, Wine's ntdll server_get_unix_fd checks FILE_READ_DATA (0x1)
    // against the stored GENERIC_READ (0x80000000) and returns STATUS_ACCESS_DENIED.
    pub(super) fn map_file_access(access: u32) -> u32 {
        const GENERIC_READ: u32    = 0x80000000;
        const GENERIC_WRITE: u32   = 0x40000000;
        const GENERIC_EXECUTE: u32 = 0x20000000;
        const GENERIC_ALL: u32     = 0x10000000;
        const FILE_GENERIC_READ: u32    = 0x00120089;
        const FILE_GENERIC_WRITE: u32   = 0x00120116;
        const FILE_GENERIC_EXECUTE: u32 = 0x001200A0;
        const FILE_ALL_ACCESS: u32      = 0x001F01FF;
        let mut mapped = access;
        if mapped & GENERIC_READ    != 0 { mapped = (mapped & !GENERIC_READ)    | FILE_GENERIC_READ; }
        if mapped & GENERIC_WRITE   != 0 { mapped = (mapped & !GENERIC_WRITE)   | FILE_GENERIC_WRITE; }
        if mapped & GENERIC_EXECUTE != 0 { mapped = (mapped & !GENERIC_EXECUTE) | FILE_GENERIC_EXECUTE; }
        if mapped & GENERIC_ALL     != 0 { mapped = (mapped & !GENERIC_ALL)     | FILE_ALL_ACCESS; }
        mapped
    }

    // Helper: create a file handle from a unix fd in the client's process
    pub(super) fn create_file_handle(&mut self, client_fd: i32, fd: RawFd, access: u32, options: u32) -> Reply {
        let access = Self::map_file_access(access);
        let oid = self.state.alloc_object_id();
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });

        // Determine fd type from fstat
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let obj_type = if unsafe { libc::fstat(fd, &mut st) } == 0 {
            let mode = st.st_mode & libc::S_IFMT;
            match mode {
                libc::S_IFDIR => crate::objects::FD_TYPE_DIR,
                libc::S_IFREG => crate::objects::FD_TYPE_FILE,
                libc::S_IFSOCK => crate::objects::FD_TYPE_SOCKET,
                libc::S_IFCHR => crate::objects::FD_TYPE_CHAR,
                libc::S_IFIFO => crate::objects::FD_TYPE_FILE, // Unix FIFOs are files, not Wine named pipes
                _ => crate::objects::FD_TYPE_FILE,
            }
        } else {
            crate::objects::FD_TYPE_FILE
        };

        let handle = if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.handles.allocate_full(
                    crate::objects::HandleEntry::with_fd(oid, fd, obj_type, access, options)
                )
            } else { 0 }
        } else { 0 };

        if handle == 0 {
            log_error!("create_file_handle: handle=0! fd={client_fd} pid={:?}", pid);
            unsafe { libc::close(fd); }
            return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 });
        }

        // Register a pre-signaled inproc sync event for this file handle.
        // Wine's ntsync path checks the sync state before server calls on the handle.
        // The event is stored in ntsync_objects but NOT sent via pending_fd here —
        // sending fds for every file handle overflows the msg_fd socket buffer
        // (576+ fds without Wine reading them → send_fd blocks → deadlock).
        // The fd is sent later when Wine calls GET_INPROC_SYNC for this handle.
        if let Some(pid) = pid {
            if !self.ntsync_objects.contains_key(&(pid, handle)) {
                if let Some(obj) = self.get_or_create_event(true, true) {
                    self.ntsync_objects.insert((pid, handle), (obj, 1)); // INTERNAL
                }
            }
        }

        let reply = CreateFileReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }
}
