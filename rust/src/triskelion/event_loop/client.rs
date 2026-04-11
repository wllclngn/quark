// Client connection lifecycle — accept, message handling, disconnect

use super::*;

#[allow(unused_variables)]
impl EventLoop {


    pub(crate) fn disconnect_client(&mut self, fd: RawFd) {
        // fd is the request_fd (pipe read end)
        epoll_del(self.epoll_fd, fd);
        let (pid, tid, msg_fd) = if let Some(client) = self.clients.get(&fd) {
            let pid = client.process_id;
            let tid = client.thread_id;
            let msg_fd = client.msg_fd;
            (pid, tid, msg_fd)
        } else {
            (0, 0, -1)
        };
        // Clean up per-client state. Stock wineserver's kill_thread does NOT
        // signal alerts or queue APCs for dying/sibling threads — it just abandons
        // mutexes and signals the thread handle. Alert signals for system APCs
        // would trigger Wine's sync.c:441 assertion in the inproc ntsync path.
        self.client_alerts.remove(&fd);
        self.client_worker_interrupts.remove(&fd);
        self.client_apc_flags.remove(&fd);

        // Abandon mutexes owned by this thread. Stock wineserver calls
        // abandon_mutexes(thread) → NTSYNC_IOC_MUTEX_KILL for each owned mutex.
        // Without this, mutexes held by dying threads stay locked forever,
        // hanging any process that tries to acquire them.
        if tid != 0 {
            let mutex_keys: Vec<(u32, u32)> = self.ntsync_objects.keys()
                .filter(|(p, _)| *p == pid)
                .copied()
                .collect();
            for (p, h) in mutex_keys {
                if let Some((obj, sync_type)) = self.ntsync_objects.get(&(p, h)) {
                    // sync_type 3 = MUT (mutex)
                    if *sync_type == 3 {
                        let _ = obj.mutex_kill(tid);
                    }
                }
            }
        }

        self.clients.remove(&fd); // Drop closes all fds
        // Pending waits for this fd are cleaned up lazily in check_pending_waits()

        // Free SHM thread queue slot so it can be reused by future threads
        if tid != 0 {
            self.shm.free_slot(tid);
        }

        // Clean up pending PIPE_WAIT waiters for this process
        if pid != 0 {
            for waiters in self.pending_pipe_waiters.values_mut() {
                waiters.retain(|w: &super::pipes::PendingPipeWaiter| w.pid != pid);
            }
            self.pending_pipe_waiters.retain(|_: &String, v: &mut Vec<super::pipes::PendingPipeWaiter>| !v.is_empty());
        }

        // Remove thread from process thread list
        if pid != 0 && tid != 0 {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.threads.retain(|&t| t != tid);
                // Clean up ghost threads: created by new_thread but never connected.
                // Without this, EARLY_DEATH never fires (remaining_threads > 0).
                let connected_tids: Vec<u32> = self.clients.values()
                    .filter(|c| c.process_id == pid && c.thread_id != 0)
                    .map(|c| c.thread_id)
                    .collect();
                process.threads.retain(|t| connected_tids.contains(t));
            }
        }

        // Clean up pending sent messages for this receiver thread.
        // Any tracked sends that never got reply_message must wake their
        // senders with QS_SMRESULT or those threads block forever.
        if tid != 0 {
            let sender_tids = self.sent_messages.drain_all_for_receiver(tid);
            for sender_tid in sender_tids {
                self.set_queue_bits_for_tid(sender_tid, 0x8000); // QS_SMRESULT
            }
        }

        // Clean up per-thread state that disconnect doesn't otherwise touch
        if tid != 0 {
            self.caret_state.remove(&tid);
            self.next_timer_ids.remove(&tid);
            self.thread_quit_state.remove(&tid);
        }
        self.deferred_event_signals.remove(&fd);
        self.pending_wakes.remove(&fd);
        self.pending_kernel_apcs.remove(&fd);
        self.thread_completion_cache.remove(&fd);
        // Clean stale completion_waiters entries for this client
        for (_, waiters) in self.completion_waiters.iter_mut() {
            waiters.retain(|w| w.client_fd != fd);
        }

        // Signal ntsync events for thread handles (WaitForSingleObject on thread).
        // thread_exit_events owns dup'd fds, so this works even if close_handle ran.
        if let Some(entries) = self.thread_exit_events.remove(&fd) {
            for (creator_pid, handle, obj) in &entries {
                log_info!("thread_exit: signaling handle={handle:#x} pid={creator_pid} for fd={fd} tid={tid}");
                let _ = obj.event_set();
            }
        } else {
            log_info!("thread_exit: NO exit events registered for fd={fd} tid={tid}");
        }

        // Check if this was the LAST thread of the process
        let remaining_threads = if pid != 0 {
            self.state.processes.get(&pid).map(|p| p.threads.len()).unwrap_or(0)
        } else { 0 };

        if pid != 0 && tid != 0 && remaining_threads == 0 {
            // Last thread died — process is truly dead.
            // msg_fd is owned by the I/O thread and closed in disconnect_io_client()
            // when the last client sharing it disconnects. Do NOT close it here.
            self.msg_fd_map.remove(&msg_fd);

            // Clean up process-wide inflight fd pool
            if let Some(pool) = self.process_inflight_fds.remove(&pid) {
                for (_, _, fd) in pool {
                    unsafe { libc::close(fd); }
                }
            }

            // Purge all per-process state from event loop collections.
            // After the last thread exits, no client holds cached fds for
            // this process's ntsync objects — safe to drop the Arcs now.
            self.ntsync_objects.retain(|(p, _), _| *p != pid);
            self.ntsync_recyclable.retain(|(p, _)| *p != pid);
            self.pending_reads.retain(|(p, _), _| *p != pid);
            self.completed_ioctls.retain(|(p, _), _| *p != pid);
            self.kernel_object_ptrs.retain(|(p, _, _), _| *p != pid);
            self.process_winstations.remove(&pid);
            // All per-handle side tables for this pid (pipe_handles closes
            // data fds, plus io_wait_handles, fd_sent, completion_bindings).
            self.side_tables.purge_pid(pid);
            // nt_timers: remove entries for dead process
            self.nt_timers.retain(|(p, _, _, _)| *p != pid);
            // Async pipe reads: close pipe fds and remove for dead process
            for r in self.pending_pipe_reads.iter() {
                if r.pid == pid { unsafe { libc::close(r.pipe_fd); } }
            }
            self.pending_pipe_reads.retain(|r| r.pid != pid);
            self.completed_pipe_reads.retain(|r| r.pid != pid);
            // Named pipes: remove instances owned by dead process, close their fds
            for instances in self.named_pipes.values_mut() {
                for info in instances.iter() {
                    if info.server_pid == pid {
                        unsafe { libc::close(info.client_data_fd); }
                    }
                }
                instances.retain(|info| info.server_pid != pid);
            }
            self.named_pipes.retain(|_, v| !v.is_empty());
            // Windows owned by threads of the dead process
            let dead_tids: Vec<u32> = self.state.threads.iter()
                .filter(|(_, t)| t.pid == pid)
                .map(|(&tid, _)| tid)
                .collect();
            let dead_windows: Vec<u32> = self.window_states.iter()
                .filter(|(_, ws)| dead_tids.contains(&ws.tid))
                .map(|(&h, _)| h)
                .collect();
            for wh in &dead_windows {
                self.window_states.remove(wh);
                self.clipboard_listeners.remove(wh);
                self.window_properties.retain(|(h, _), _| h != wh);
                self.win_timers_pending.remove(wh);
                self.win_timers_expired.remove(wh);
            }
            // Registry notifications for dead process
            self.registry.remove_notifications_for_pid(pid);

            let did_init = self.state.processes.get(&pid)
                .map(|p| p.startup_done).unwrap_or(false);

            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.exit_code = 0;
                process.startup_done = true;
            }

            // Phase 1a: If child died before init_process_done, signal info
            // handles so the parent's Select on the info handle wakes up.
            // Without this, the parent hangs forever on INFINITE timeout.
            if !did_init {
                log_warn!("EARLY_DEATH: pid={pid} died before init_process_done");
                let info_entries: Vec<(u32, u32)> = self.state.process_info_handles.iter()
                    .filter(|(_, v)| v.target_pid == pid)
                    .map(|(&handle, v)| (v.parent_pid, handle))
                    .collect();
                for (parent_pid, ih) in &info_entries {
                    if let Some((obj, _)) = self.ntsync_objects.get(&(*parent_pid, *ih)) {
                        let result = obj.event_set();
                        log_warn!("EARLY_DEATH: signaled info handle {ih:#x} (parent_pid={parent_pid}) for dead pid={pid} result={result:?}");
                    }
                }
            }

            // NOTE: previously drained ALL named_sync on any early death.
            // This was wrong — it destroyed __wineboot_event and __wine_svcctlstarted
            // when an unrelated WoW64 helper died, breaking event sharing between
            // wineboot and services.exe. Named sync objects are global and must
            // persist across process lifetimes.

            // Signal + clean up idle event so WaitForInputIdle waiters don't hang
            if let Some(idle_event) = self.process_idle_events.remove(&pid) {
                let _ = idle_event.event_set(); // signal before drop
            }

            // Signal ntsync events for process handles held by parents.
            // process_exit_events owns dup'd NtsyncObj fds, so this works even
            // if the parent already closed its handle via close_handle.
            if let Some(entries) = self.process_exit_events.remove(&pid) {
                for (parent_pid, handle, obj) in &entries {
                    let result = obj.event_set();
                }
            }

            // Job notifications: post JOB_OBJECT_MSG_EXIT_PROCESS to completion port
            if let Some(job_oid) = self.process_job.remove(&pid) {
                let completion_info = self.jobs.get(&job_oid)
                    .and_then(|j| j.completion_port_handle.map(|port| (port, j.completion_key)));
                if let Some(job) = self.jobs.get_mut(&job_oid) {
                    job.processes.retain(|&p| p != pid);
                    job.num_processes = job.num_processes.saturating_sub(1);
                }
                if let Some((port, ckey)) = completion_info {
                    // JOB_OBJECT_MSG_EXIT_PROCESS = 4
                    self.completion_queues.entry(port).or_default().push(CompletionMsg {
                        ckey, cvalue: pid as u64, information: 0, status: 4,
                    });
                    let zero = self.jobs.get(&job_oid).map(|j| j.num_processes == 0).unwrap_or(false);
                    if zero {
                        // JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO = 8
                        self.completion_queues.entry(port).or_default().push(CompletionMsg {
                            ckey, cvalue: 0, information: 0, status: 8,
                        });
                    }
                    // Wake any thread waiting on the completion port
                    if let Some(waiters) = self.completion_waiters.get_mut(&port) {
                        while let Some(waiter) = waiters.pop() {
                            if let Some(queue) = self.completion_queues.get_mut(&port) {
                                if !queue.is_empty() {
                                    let msg = queue.remove(0);
                                    self.thread_completion_cache.insert(waiter.client_fd, msg);
                                    if let Some((obj, _)) = self.ntsync_objects.get(&(waiter.pid, waiter.wait_handle)) {
                                        let _ = obj.event_set();
                                    }
                                }
                            }
                        }
                        if waiters.is_empty() {
                            self.completion_waiters.remove(&port);
                        }
                    }
                }
            }

            // Check if this was a user (non-system) process dying.
            // If no user processes remain, signal shutdown_event to wake system processes
            // and start the linger timer — don't exit yet, new processes may connect
            // (e.g., game launching after wineboot finishes).
            if !self.system_pids.contains(&pid) {
                let user_processes_alive = self.state.processes.iter()
                    .filter(|(ppid, p)| !self.system_pids.contains(ppid) && !p.threads.is_empty())
                    .count();
                if user_processes_alive == 0 {
                    if let Some(ref evt) = self.shutdown_event {
                        let _ = evt.event_set();
                        log_info!("disconnect: ALL user processes gone, signaled shutdown_event!");
                    }
                    // Save registry NOW — the process might be killed during linger.
                    self.registry.save_to_prefix(&self.user_sid_str);

                    // Start linger: wait 5s for new connections before exiting.
                    // This bridges the gap between wineboot exit and game connect.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    self.linger_deadline = Some(deadline);
                    log_info!("disconnect: linger started (5s deadline)");
                }
            }

            // Final cleanup: remove process and thread entries from ServerState.
            // This MUST be last — earlier code reads state.processes/threads.
            self.system_pids.remove(&pid);
            self.state.image_views.retain(|(p, _), _| *p != pid);
            // Close dup'd ntsync fds before removing entries
            for (_, v) in self.state.process_info_handles.iter() {
                if v.target_pid == pid && v.ntsync_obj_fd >= 0 {
                    unsafe { libc::close(v.ntsync_obj_fd); }
                }
            }
            self.state.process_info_handles.retain(|_, v| v.target_pid != pid);
            for &t in &dead_tids {
                self.state.threads.remove(&t);
            }
            self.state.processes.remove(&pid);
        } else if pid != 0 && tid != 0 {
        } else if pid != 0 && tid == 0 {
        }
    }
}
