// Event loop -- the hub that spins the three legs
//
// Uses epoll for fd readiness notification.
// Accepts client connections, reads requests, dispatches to the appropriate
// leg, writes replies.
//
// Single-threaded for protocol correctness first. Partitioned multithreading
// comes later -- the three legs are designed for it (per-thread queues,
// per-process handles, per-object sync state).

mod dispatch;
mod process;
mod thread;
mod sync;
mod handles;
mod file_io;
mod registry_handlers;
mod pipes;
mod window;
mod token;
mod completion;
mod client;
mod side_tables;

pub(crate) use side_tables::{HandleSideTables, PipeHandle};

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64};
use rustc_hash::{FxHashMap, FxHashSet};
use std::os::unix::io::RawFd;
use std::time::Instant;

use crate::ipc::{Client, Listener};
use crate::objects::ServerState;
use pipes::{NamedPipeInfo, PendingPipeWaiter};
use crate::protocol::*;
use crate::registry::Registry;
use crate::shm::ShmManager;

/// Queued completion port message (matches Wine's comp_msg in server/completion.c).
pub(super) struct CompletionMsg {
    pub ckey: u64,
    pub cvalue: u64,
    pub information: u64,
    pub status: u32,
}

/// A thread blocked in remove_completion waiting for a message to arrive.
pub(super) struct CompletionWaiter {
    pub client_fd: RawFd,
    pub pid: u32,
    pub wait_handle: u32,
}

/// A pending async read waiting for data to become available on a pipe/socket fd.
/// Keyed by (pid, user_arg) so get_async_result can retry the read.
pub(super) struct PendingRead {
    pub fd: RawFd,
    pub max_bytes: usize,
}

/// Completed async pipe read — data ready for get_async_result retrieval.
pub(crate) struct CompletedPipeRead {
    pub client_fd: RawFd,
    pub data: Vec<u8>,       // empty = EOF
    pub pid: u32,
}

/// Async pipe read: broker polls the pipe fd periodically. When data arrives,
/// reads it and signals the wait handle so the client's ntsync wait completes.
/// This replaces BlockingPipeRead which permanently blocked worker threads.
pub(crate) struct AsyncPipeRead {
    pub pipe_fd: RawFd,        // the pipe/socket fd to read from
    pub max_bytes: usize,      // max bytes to read
    pub pid: u32,              // process that owns the wait handle
    pub wait_handle: u32,      // ntsync event to signal when data arrives
    pub client_fd: RawFd,      // client's request_fd (for reply routing)
}

const MAX_OPCODES: usize = 306;

// Wine request VARARG data starts at offset 64 (the fixed request block is
// padded to 64 bytes, i.e. sizeof(union generic_request)), NOT at sizeof(XxxRequest).
const VARARG_OFF: usize = 64;

#[repr(C)]
pub(super) struct WakeUpReply {
    pub cookie: u64,
    pub signaled: i32,
    pub _pad: i32,
}

struct PendingWait {
    deadline: Instant,
    client_fd: RawFd,
    cookie: u64,
}

impl Eq for PendingWait {}
impl PartialEq for PendingWait {
    fn eq(&self, other: &Self) -> bool { self.deadline == other.deadline }
}
impl Ord for PendingWait {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.deadline.cmp(&other.deadline) }
}
impl PartialOrd for PendingWait {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}

pub(super) struct WinTimer {
    #[allow(dead_code)]
    pub(super) when: Instant,
    pub(super) rate_ms: u32,
    pub(super) win: u32,
    pub(super) msg: u32,
    pub(super) id: u64,
    pub(super) lparam: u64,
}

#[allow(dead_code)]
struct WindowState {
    style: u32,
    ex_style: u32,
    is_unicode: i16,
    owner: u32,
    parent: u32,
    tid: u32,
    id: u64,
    instance: u64,
    user_data: u64,
    dpi_context: u32,
    // Window rectangles: [left, top, right, bottom] as i32 LE bytes (16 bytes each)
    window_rect: [u8; 16],
    client_rect: [u8; 16],
    visible_rect: [u8; 16],
    surface_rect: [u8; 16],
    // Paint flags from client (PAINT_HAS_SURFACE=1, PAINT_HAS_PIXEL_FORMAT=2, PAINT_HAS_LAYERED_SURFACE=4, PAINT_NONCLIENT=0x0040)
    paint_flags: u16,
    // Paint tracking: true if this window needs a WM_PAINT
    needs_paint: bool,
    // Window title text (UTF-16LE bytes)
    window_text: Vec<u8>,
    // Per-window class extra bytes (cbWndExtra from WNDCLASS)
    extra_bytes: Vec<u8>,
}

pub struct EventLoop {
    epoll_fd: RawFd,
    timer_fd: RawFd,
    pub(crate) clients: FxHashMap<RawFd, Client>,
    /// Maps msg_fd (connection socket) → request_fd (pipe read end).
    /// When epoll fires on a msg_fd, we route to the right client to
    /// extract SCM_RIGHTS file descriptors.
    pub(crate) msg_fd_map: FxHashMap<RawFd, RawFd>,
    listener: Option<Listener>,
    state: ServerState,
    pub(crate) registry: Registry,
    shm: ShmManager,
    opcode_counts: [u64; MAX_OPCODES],
    opcode_time_ns: [u64; MAX_OPCODES],  // cumulative ns per opcode (CLOCK_MONOTONIC_RAW)
    total_dispatch_ns: u64,               // total ns spent in dispatch handlers
    pub(crate) total_requests: u64,
    pub(crate) idle_ticks: u64,
    pending_waits: BinaryHeap<Reverse<PendingWait>>,
    pub(super) win_timers_pending: FxHashMap<u32, Vec<WinTimer>>,
    pub(super) win_timers_expired: FxHashMap<u32, Vec<WinTimer>>,
    start_time: Instant,
    peak_clients: usize,
    // ntsync: kernel-native NT sync (None = older kernel, fallback to stubs)
    ntsync: Option<crate::ntsync::NtsyncDevice>,
    ntsync_objects: FxHashMap<(u32, obj_handle_t), (Arc<crate::ntsync::NtsyncObj>, u32)>,
    ntsync_objects_created: u64,
    // Keys of ntsync events safe to recycle (not dups of named sync objects)
    ntsync_recyclable: FxHashSet<(u32, obj_handle_t)>,
    // Named sync objects: name → (canonical ntsync fd, sync_type).
    // The canonical fd stays open so the kernel object lives; handles get dup'd fds.
    named_sync: HashMap<String, (RawFd, u32)>,
    // Process exit events: child_pid → [(parent_pid, handle, Arc ntsync obj)].
    // Arc keeps fd alive even if parent closes its handle copy.
    process_exit_events: FxHashMap<u32, Vec<(u32, u32, Arc<crate::ntsync::NtsyncObj>)>>,
    // Thread exit events: request_fd → [(creator_pid, handle, Arc ntsync obj)].
    thread_exit_events: FxHashMap<RawFd, Vec<(u32, u32, Arc<crate::ntsync::NtsyncObj>)>>,
    // get_next_thread: maps returned handles → tid so the `last` parameter can be resolved.
    thread_handle_tids: FxHashMap<u32, u32>,
    last_input_time: u32,
    // Per device-manager kernel object registry: (pid, manager_handle, object_handle) → user_ptr
    kernel_object_ptrs: FxHashMap<(u32, u32, u32), u64>,
    // Per-client alert events: returned to Wine via get_inproc_alert_fd for inproc waits.
    // NEVER signaled by the daemon (stock wineserver only signals for APC_USER).
    client_alerts: FxHashMap<RawFd, Arc<crate::ntsync::NtsyncObj>>,
    // Per-client worker interrupt: auto-reset event used as alert_fd for daemon-side
    // ntsync worker threads. Signaled to interrupt server-side waits for system APCs.
    // Separate from client_alerts to avoid triggering Wine's sync.c:441 assertion.
    client_worker_interrupts: FxHashMap<RawFd, Arc<crate::ntsync::NtsyncObj>>,
    // Per-client APC pending flag: shared with ntsync wait threads.
    client_apc_flags: FxHashMap<RawFd, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    // Deferred event signals: after APC delivery in Select, signal these ntsync events.
    // This ensures the IOSB is written (by APC callback) BEFORE the event fires.
    pub(super) deferred_event_signals: FxHashMap<RawFd, Vec<(u32, u32)>>,
    /// Per-handle side tables. All state keyed by (pid, handle) lives here.
    /// Cleanup goes through HandleSideTables::purge or purge_pid.
    pub(crate) side_tables: HandleSideTables,
    // System process shutdown: tracks pids that called MakeProcessSystem.
    // When all non-system processes exit, shutdown_event is signaled.
    system_pids: HashSet<u32>,
    shutdown_event: Option<Arc<crate::ntsync::NtsyncObj>>,
    // Desktop shared memory locator (written into session memfd)
    desktop_locator_id: u64,
    desktop_offset: u64,
    session_fd: RawFd,
    // Named pipes: pipe name (lowercased) → instances (multiple server instances per name)
    named_pipes: HashMap<String, Vec<NamedPipeInfo>>,
    // Pending PIPE_WAIT waiters: pipe name → clients blocked waiting for a listener.
    // Drained when FSCTL_PIPE_LISTEN or create_named_pipe adds a Listening instance.
    pending_pipe_waiters: HashMap<String, Vec<PendingPipeWaiter>>,
    // Unnamed (anonymous) pipes: server_handle → client_handle (both in same process).
    // The parent DuplicateHandle's the client_handle into the child process for stdio.
    pub(super) unnamed_pipe_client_handles: FxHashMap<u32, u32>,
    // First unparented process flag: only the first process with no new_process parent
    // gets info_size=0 (triggers run_wineboot). Subsequent unparented connections
    // (WoW64 helper) get info_size=1 to skip run_wineboot.
    pub(super) boot_process_claimed: bool,
    // Job objects: object_id → Job state. process_job: pid → job object_id.
    jobs: FxHashMap<u64, crate::objects::Job>,
    process_job: FxHashMap<u32, u64>,
    // Deferred wakes: when send_select_wake fires before wait_fd is available,
    // queue (cookie, signaled) here. Drained when init_first_thread/init_thread
    // sets wait_fd.
    pub(super) pending_wakes: FxHashMap<RawFd, Vec<(u64, i32)>>,
    // Window management: desktop and message window handles (global, not per-process)
    desktop_top_window: u32,
    desktop_msg_window: u32,
    // Desktop readiness: set true when explorer calls create_desktop.
    // get_desktop_window(force=1) defers reply until this is true,
    // preventing the game from racing with explorer's display init.
    desktop_ready: bool,
    next_user_handle_index: u32, // next index into user_entry table
    user_handle_free_list: Vec<u32>, // recycled user handle indices
    // Session shared memory: persistent mmap and bump allocator for shared_object_t entries
    session_map: *mut u8,
    session_size: usize,
    next_shared_offset: u64,
    next_shared_id: u64,
    // Class atom → obj_locator (shared_object_t reference) for window_shm_t.class
    class_locators: FxHashMap<u32, [u8; 16]>,
    // Class atom → client_ptr (client-side pointer, returned by create_window)
    class_client_ptrs: FxHashMap<u32, u64>,
    // Class atom → win_extra bytes (returned by create_window for SetWindowLong)
    class_win_extra: FxHashMap<u32, i32>,
    // Per-window server-side state: handle → WindowState
    window_states: FxHashMap<u32, WindowState>,
    // Window properties: (window_handle, atom) → data (lparam_t)
    window_properties: FxHashMap<(u32, u32), u64>,
    // Monitor serial: incremented each time set_winstation_monitors is called with increment=1
    monitor_serial: u64,
    // Winstation monitor tracking: serial is bumped by set_winstation_monitors(increment=1).
    // Winstation names: object_id → UTF-16LE name bytes.
    // Wine's lock_display_devices checks if the process winstation is "__wineservice_winstation"
    // to trigger the virtual_monitor path, which bumps monitor_serial.
    winstation_names: HashMap<u32, Vec<u8>>,
    // Cross-process SendMessage tracker: senders blocked waiting for reply_message.
    pub(super) sent_messages: crate::sent_messages::SentMessages,
    // Protocol intelligence: per-game learning from auto-stubs
    intel: crate::intel::IntelManager,
    // Per-process idle events: pid → ntsync manual-reset event (initially unsignaled).
    // Signaled when the process first enters a blocking wait (message loop idle).
    // Used by WaitForInputIdle (get_process_idle_event) to synchronize process startup.
    process_idle_events: FxHashMap<u32, Arc<crate::ntsync::NtsyncObj>>,
    // User Shared Data page: persistent mmap for updating TickCount/InterruptTime
    usd_map: *mut u8,
    // Dynamic protocol remapping: client opcode numbers → our RequestCode variants.
    // Built at startup from the client's protocol.def when Wine versions differ.
    protocol_remap: crate::protocol_remap::ProtocolRemap,
    // User SID parsed from prefix's user.reg — used for token and registry lookups
    user_sid: Vec<u8>,
    pub(crate) user_sid_str: String,
    // Completion ports: handle → queue of pending messages
    completion_queues: FxHashMap<u32, Vec<CompletionMsg>>,
    // Completion waiters: port handle → list of threads blocked on remove_completion
    completion_waiters: FxHashMap<u32, Vec<CompletionWaiter>>,
    // Per-thread cached completion message (from wait satisfaction): client_fd → msg
    thread_completion_cache: FxHashMap<RawFd, CompletionMsg>,
    // Pending async reads: (pid, user_arg) → read state for get_async_result retry
    pending_reads: FxHashMap<(u32, u64), PendingRead>,
    // Completed ioctl operations: (pid, user_arg) → status code.
    // Used by get_async_result to return completion for ioctls like FSCTL_PIPE_LISTEN.
    completed_ioctls: FxHashMap<(u32, u64), u32>,
    // Pending APC_ASYNC_IO deliveries per client fd.
    // When a pipe connect completes, we queue the APC here. The next select from
    // that thread checks this queue and returns STATUS_KERNEL_APC with the APC data.
    pending_kernel_apcs: FxHashMap<RawFd, Vec<[u8; 28]>>,
    // Clipboard state: monotonic sequence number (stock: clipboard.c)
    clipboard_seqno: u32,
    // Per-thread WM_QUIT state: tid → (exit_code, pending). Set by PostQuitMessage,
    // consumed by get_message (stock: queue.c quit_message/exit_code)
    thread_quit_state: FxHashMap<u32, (i32, bool)>,
    // Cursor state: handle, show_count, position, clip rect, last_change timestamp
    cursor_handle: u32,
    cursor_count: i32,
    cursor_x: i32,
    cursor_y: i32,
    cursor_clip: [u8; 16],
    cursor_last_change: u32,
    // Per-thread caret state: tid → (window, rect, hide_count, state)
    caret_state: FxHashMap<u32, (u32, [u8; 16], i32, i32)>,
    // Keyboard repeat: (enable, delay_ms, period_ms)
    keyboard_repeat: (i32, i32, i32),
    // Clipboard listeners: set of window handles
    clipboard_listeners: FxHashSet<u32>,
    // Auto-assign timer IDs for thread-wide timers (win==0): tid → next_id (counts down from 0x7fff)
    next_timer_ids: FxHashMap<u32, u64>,
    // winex11.drv + GLX over XWayland.
    // Primary monitor rect from set_winstation_monitors: [left, top, right, bottom] as i32 LE bytes.
    // Used to size top-level game windows to fullscreen.
    pub(crate) monitor_rect: [u8; 16],
    // Linger deadline: when all user processes exit, wait this long before
    // shutting down. Cleared when a new client connects. This prevents the
    // daemon from self-terminating between wineboot exit and game connect.
    pub linger_deadline: Option<Instant>,
    pub linger_secs: u64,
    // Async pipe reads: broker polls these periodically. When data arrives,
    // reads it and signals the wait handle. Replaces BlockingPipeRead.
    // Per-process winstation handle: pid → handle. Processes that call
    // set_process_winstation get their own handle. Others get default.
    pub(crate) process_winstations: FxHashMap<u32, u32>,
    pub(crate) default_winstation_handle: u32,
    pub(crate) pending_pipe_reads: Vec<AsyncPipeRead>,
    pub(crate) completed_pipe_reads: Vec<CompletedPipeRead>,
    // Process-wide inflight fd pool: process_id → VecDeque<(thread_id, fd_number, actual_fd)>
    // All threads in a process share one msg_fd. SCM_RIGHTS fds drain here,
    // tagged with the sending thread's tid for correct routing.
    pub(crate) process_inflight_fds: FxHashMap<u32, VecDeque<(u32, i32, RawFd)>>,
    /// Pending pipe watch requests: (pipe_data_fd, ntsync_fd).
    /// Handlers push to this; process_request drains into effects.
    pub(crate) pending_pipe_watches: Vec<(RawFd, RawFd)>,
    /// Pending queue fd effects: (queue_fd, shm_ptr, queue_handle, ntsync_event_fd).
    /// set_queue_fd pushes WatchQueueFd; set_queue_mask pushes RearmQueueFd.
    pub(crate) pending_queue_fd_watches: Vec<(RawFd, usize, u32, RawFd)>,
    /// Pending queue fd re-arm requests.
    pub(crate) pending_queue_fd_rearms: Vec<RawFd>,
    /// NT kernel timers: (pid, handle) -> deadline + period.
    pub(crate) nt_timers: Vec<(u32, u32, std::time::Instant, u32)>, // (pid, handle, deadline, period_ms)
}


impl EventLoop {
    pub fn new(listener: Listener, signal_fd: RawFd, shm: ShmManager, protocol_remap: crate::protocol_remap::ProtocolRemap, user_sid: Vec<u8>, user_sid_str: &str) -> Self {
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

        let mut state = ServerState::new();

        // Create session shared memory (\\KernelObjects\\__wine_session)
        // Layout: user_entry[32744] (32 bytes each) followed by shared_object_t pool.
        // Desktop shared_object_t is placed right after the user_entry table.
        // Each shared_object_t is allocated with SHARED_OBJECT_STRIDE spacing (1024 bytes)
        // to accommodate the full object_shm_t union size on the client side.
        const SHARED_OBJECT_STRIDE: usize = 1024;
        let user_entries_size: usize = 32 * 32744; // user_entry table
        let desktop_offset: u64 = user_entries_size as u64;
        let session_size: usize = user_entries_size + 4 * 1024 * 1024; // 4MB for shared objects pool (~4096 objects)
        let session_fd = create_session_memfd(session_size);
        let desktop_locator_id: u64 = 0x100; // fixed non-zero ID for the desktop
        let mut session_map: *mut u8 = std::ptr::null_mut();
        if session_fd >= 0 {
            let oid = state.alloc_object_id();
            state.named_objects.insert("__wine_session".to_string(), crate::objects::NamedObjectEntry {
                object_id: oid, fd: session_fd,
            });
            state.mappings.insert(oid, crate::objects::MappingInfo {
                fd: session_fd, size: session_size as u64, flags: 0x800000, pe_image_info: None, nt_name: None, shared_fd: None, // SEC_COMMIT
            });

            // mmap session memfd persistently — kept alive for shared object allocation
            unsafe {
                let ptr = libc::mmap(
                    std::ptr::null_mut(), session_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED, session_fd, 0,
                );
                if ptr != libc::MAP_FAILED {
                    session_map = ptr as *mut u8;
                    // Write desktop shared_object_t at desktop_offset
                    let base = session_map.add(desktop_offset as usize);
                    // shared_object_t.seq and id with atomic Release stores
                    let seq_ptr = &*(base as *const AtomicI64);
                    let id_ptr = &*(base.add(8) as *const AtomicU64);
                    seq_ptr.store(0, std::sync::atomic::Ordering::Release);
                    id_ptr.store(desktop_locator_id, std::sync::atomic::Ordering::Release);
                    // desktop_shm_t starts at offset 16
                    let shm = base.add(16);
                    // flags = 0
                    *(shm as *mut u32) = 0;
                    // shared_cursor: x=0, y=0, last_change=0
                    // cursor.clip: query X11 display for real resolution at startup.
                    // Services may fail (broken pipe I/O), which prevents PlugPlay
                    // from running display enumeration. Pre-populate here so games
                    // get correct resolution even without set_winstation_monitors.
                    let (screen_w, screen_h) = default_resolution();
                    *(shm.add(24) as *mut i32) = screen_w;  // clip.right
                    *(shm.add(28) as *mut i32) = screen_h;  // clip.bottom
                    // keystate[256] and keystate_serial are zero (from ftruncate).
                    // monitor_serial: start at 1 (matches stock wineserver).
                    // Triggers display driver enumeration on first lock_display_devices call.
                    const DESKTOP_SHM_MONITOR_SERIAL_OFFSET: usize = 288;
                    *(shm.add(DESKTOP_SHM_MONITOR_SERIAL_OFFSET) as *mut u64) = 1;
                }
            }
            log_info!("session shm: created __wine_session ({session_size} bytes, desktop@{desktop_offset})");
        }
        // First shared object after desktop: bump allocator starts here
        let next_shared_offset = desktop_offset + SHARED_OBJECT_STRIDE as u64;
        let next_shared_id = desktop_locator_id + 1; // 0x101

        let mut ev = Self {
            epoll_fd,
            timer_fd,
            clients: FxHashMap::default(),
            msg_fd_map: FxHashMap::default(),
            listener: Some(listener),
            state,
            registry: Registry::new(user_sid_str),
            shm,
            opcode_counts: [0; MAX_OPCODES],
            opcode_time_ns: [0; MAX_OPCODES],
            total_dispatch_ns: 0,
            total_requests: 0,
            idle_ticks: 0,
            pending_waits: BinaryHeap::new(),
            win_timers_pending: FxHashMap::default(),
            win_timers_expired: FxHashMap::default(),

            start_time: Instant::now(),
            peak_clients: 0,
            ntsync,
            ntsync_objects: FxHashMap::default(),
            ntsync_objects_created: 0,
            ntsync_recyclable: FxHashSet::default(),
            named_sync: HashMap::new(),
            process_exit_events: FxHashMap::default(),
            thread_exit_events: FxHashMap::default(),
            last_input_time: 0,
            kernel_object_ptrs: FxHashMap::default(),
            thread_handle_tids: FxHashMap::default(),
            client_alerts: FxHashMap::default(),
            client_worker_interrupts: FxHashMap::default(),
            client_apc_flags: FxHashMap::default(),
            deferred_event_signals: FxHashMap::default(),
            side_tables: HandleSideTables::new(),
            system_pids: HashSet::new(),
            shutdown_event: None,
            desktop_locator_id,
            desktop_offset,
            session_fd,
            named_pipes: HashMap::new(),
            pending_pipe_waiters: HashMap::new(),
            unnamed_pipe_client_handles: FxHashMap::default(),
            pending_wakes: FxHashMap::default(),
            boot_process_claimed: false,
            jobs: FxHashMap::default(),
            process_job: FxHashMap::default(),
            desktop_top_window: 0,
            desktop_msg_window: 0,
            desktop_ready: false,
            next_user_handle_index: 0,
            user_handle_free_list: Vec::new(),
            session_map,
            session_size,
            next_shared_offset,
            next_shared_id,
            class_locators: FxHashMap::default(),
            class_client_ptrs: FxHashMap::default(),
            class_win_extra: FxHashMap::default(),
            window_states: FxHashMap::default(),
            window_properties: FxHashMap::default(),
            monitor_serial: 1, // Start at 1: matches stock wineserver

            winstation_names: HashMap::new(),
            sent_messages: crate::sent_messages::SentMessages::new(),
            intel: {
                let intel = crate::intel::IntelManager::new();
                intel.log_summary();
                intel
            },
            process_idle_events: FxHashMap::default(),
            usd_map: std::ptr::null_mut(),
            protocol_remap,
            user_sid,
            user_sid_str: user_sid_str.to_string(),
            completion_queues: FxHashMap::default(),
            completion_waiters: FxHashMap::default(),
            thread_completion_cache: FxHashMap::default(),
            pending_reads: FxHashMap::default(),
            completed_ioctls: FxHashMap::default(),
            pending_kernel_apcs: FxHashMap::default(),
            clipboard_seqno: 0,
            thread_quit_state: FxHashMap::default(),
            monitor_rect: {
                let (w, h) = default_resolution();
                let mut r = [0u8; 16];
                // [left=0, top=0, right=w, bottom=h]
                r[8..12].copy_from_slice(&w.to_le_bytes());
                r[12..16].copy_from_slice(&h.to_le_bytes());
                r
            },
            cursor_handle: 0,
            cursor_count: 0,
            cursor_x: 0,
            cursor_y: 0,
            cursor_clip: [0u8; 16],
            cursor_last_change: 0,
            caret_state: FxHashMap::default(),
            keyboard_repeat: (1, 500, 33), // enabled, 500ms delay, 33ms period (defaults)
            clipboard_listeners: FxHashSet::default(),
            next_timer_ids: FxHashMap::default(),
            linger_deadline: None,
            linger_secs: {
                // Adaptive: scale linger with game exe size (proxy for load time)
                let hint = std::env::var("QUARK_LINGER_HINT")
                    .ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let exe_mb = hint / (1024 * 1024);
                exe_mb.max(5).min(30)
            },
            process_winstations: FxHashMap::default(),
            default_winstation_handle: 0, // set below after alloc
            pending_pipe_reads: Vec::new(),
            completed_pipe_reads: Vec::new(),
            process_inflight_fds: FxHashMap::default(),
            pending_pipe_watches: Vec::new(),
            pending_queue_fd_watches: Vec::new(),
            pending_queue_fd_rearms: Vec::new(),
            nt_timers: Vec::new(),
        };

        // Create default winstation (WinSta0) so non-service processes
        // get an interactive winstation, not __wineservice_winstation.
        {
            let oid = ev.state.alloc_object_id();
            // Store name "WinSta0" in winstation_names
            let name_bytes: Vec<u8> = "WinSta0".encode_utf16()
                .flat_map(|c| c.to_le_bytes()).collect();
            ev.winstation_names.insert(oid as u32, name_bytes);
            ev.default_winstation_handle = oid as u32; // use oid as handle for simplicity
        }

        // Pre-create desktop window handles at startup with pid=0/tid=0.
        // get_desktop_window returns these immediately so Wine doesn't fork-bomb
        // explorer.exe. When explorer calls create_desktop, we reassign the
        // tid/pid to explorer's thread — this ensures display driver init
        // happens under explorer's identity, preventing the update_display_cache
        // race condition (user_check_not_lock assertion).
        const NTUSER_OBJ_WINDOW: u16 = 0x01;
        const NTUSER_DPI_PER_MONITOR_AWARE: u32 = 0x12;

        let mut desktop_rect = [0u8; 16];
        let (w, h) = default_resolution();
        desktop_rect[8..12].copy_from_slice(&w.to_le_bytes());
        desktop_rect[12..16].copy_from_slice(&h.to_le_bytes());

        // Allocate class locator for the desktop window class.
        // Wine's win32u reads the class via the obj_locator in window_shm_t.
        // A zeroed locator causes find_shared_session_object to return NULL → crash.
        let desktop_class_loc = ev.alloc_shared_object();

        let top_offset = ev.next_shared_offset;
        ev.desktop_top_window = ev.alloc_user_handle(NTUSER_OBJ_WINDOW, 0, 0);
        ev.set_window_shm(top_offset, &desktop_class_loc, NTUSER_DPI_PER_MONITOR_AWARE);
        ev.window_states.insert(ev.desktop_top_window, WindowState {
            style: 0x96000000, ex_style: 0, is_unicode: 1, owner: 0,
            parent: 0, tid: 0, id: 0, instance: 0, user_data: 0,
            dpi_context: NTUSER_DPI_PER_MONITOR_AWARE,
            window_rect: desktop_rect, client_rect: desktop_rect,
            visible_rect: desktop_rect, surface_rect: desktop_rect,
            paint_flags: 0, needs_paint: false, window_text: Vec::new(), extra_bytes: Vec::new(),
        });

        let msg_class_loc = ev.alloc_shared_object();
        let msg_offset = ev.next_shared_offset;
        ev.desktop_msg_window = ev.alloc_user_handle(NTUSER_OBJ_WINDOW, 0, 0);
        ev.set_window_shm(msg_offset, &msg_class_loc, NTUSER_DPI_PER_MONITOR_AWARE);
        ev.window_states.insert(ev.desktop_msg_window, WindowState {
            style: 0, ex_style: 0, is_unicode: 1, owner: 0,
            parent: 0, tid: 0, id: 0, instance: 0, user_data: 0,
            dpi_context: NTUSER_DPI_PER_MONITOR_AWARE,
            window_rect: [0u8; 16], client_rect: [0u8; 16],
            visible_rect: [0u8; 16], surface_rect: [0u8; 16],
            paint_flags: 0, needs_paint: false, window_text: Vec::new(), extra_bytes: Vec::new(),
        });
        log_info!("desktop: pre-created top={:#x} msg={:#x}",
            ev.desktop_top_window, ev.desktop_msg_window);

        // Pre-set __wine_display_device_guid on the desktop window.
        // This tells load_desktop_driver where to find GraphicsDriver in the registry.
        // Must be available BEFORE any thread calls load_desktop_driver — no races.
        {
            const DISPLAY_GUID: &str = "00000000-0000-0000-0000-000000000000";
            let guid_u16: Vec<u16> = DISPLAY_GUID.encode_utf16().collect();
            let guid_bytes: Vec<u8> = guid_u16.iter().flat_map(|c| c.to_le_bytes()).collect();
            let guid_atom = ev.state.next_atom;
            ev.state.next_atom += 1;
            ev.state.atoms.insert(guid_atom, (guid_bytes, 1));
            ev.state.atom_names.insert(guid_u16, guid_atom);

            let prop_name = "__wine_display_device_guid";
            let prop_u16: Vec<u16> = prop_name.encode_utf16().collect();
            let prop_bytes: Vec<u8> = prop_u16.iter().flat_map(|c| c.to_le_bytes()).collect();
            let prop_atom = ev.state.next_atom;
            ev.state.next_atom += 1;
            ev.state.atoms.insert(prop_atom, (prop_bytes, 1));
            ev.state.atom_names.insert(prop_u16, prop_atom);

            ev.window_properties.insert((ev.desktop_top_window, prop_atom), guid_atom as u64);
            log_info!("desktop: pre-set __wine_display_device_guid={DISPLAY_GUID} (atom={guid_atom}) on window={:#x}",
                ev.desktop_top_window);
        }

        // No explorer.exe — set desktop_ready immediately. Triskelion owns the
        // desktop window, GUID property, and GraphicsDriver registry key.
        // winewayland.drv loads directly without the WM_NULL message loop gate.
        ev.desktop_ready = true;
        ev.shm.set_desktop_ready();
        log_info!("desktop: ready (no explorer, daemon-owned)");

        // Let wineboot run naturally — pre-signaling __wineboot_event skips
        // critical x11drv initialization (set_queue_fd never called by game thread).
        // Pre-create mutexes needed by plugplay/font init.
        if let Some(ref ntsync_dev) = ev.ntsync {
            if let Some(obj) = ntsync_dev.create_mutex(0, 0) {
                if let Some(dup) = obj.dup() {
                    ev.named_sync.insert("display_device_init".to_string(), (dup.fd(), 3));
                    std::mem::forget(dup);
                }
            }
            if let Some(obj) = ntsync_dev.create_mutex(0, 0) {
                if let Some(dup) = obj.dup() {
                    ev.named_sync.insert("__wine_font_mutex__".to_string(), (dup.fd(), 3));
                    std::mem::forget(dup);
                }
            }
        }

        // Seed adaptive message routing with profiles from prior launches
        let msg_profiles = ev.intel.take_msg_profiles();
        if !msg_profiles.is_empty() {
            ev.sent_messages.load_profiles(msg_profiles);
        }

        ev
    }


    // Allocate a user handle (window/menu/etc) in Wine's format.
    // Handle = (index << 1) + FIRST_USER_HANDLE, where FIRST_USER_HANDLE = 0x0020.
    // Also initializes the user_entry in the session shared memory.
    fn alloc_user_handle(&mut self, obj_type: u16, tid: u32, pid: u32) -> u32 {
        const FIRST_USER_HANDLE: u32 = 0x0020;
        const USER_ENTRY_SIZE: usize = 32;
        const SHARED_OBJECT_STRIDE: usize = 1024;


        let index = if let Some(recycled) = self.user_handle_free_list.pop() {
            recycled
        } else {
            let i = self.next_user_handle_index;
            self.next_user_handle_index += 1;
            i
        };
        let handle = (index << 1) + FIRST_USER_HANDLE;

        // Allocate a shared_object_t in the session memfd pool.
        // The client reads this via find_shared_session_object(id, offset).
        let shared_offset = self.next_shared_offset;
        let shared_id = self.next_shared_id;
        self.next_shared_offset = self.next_shared_offset
            .checked_add(SHARED_OBJECT_STRIDE as u64)
            .expect("session shared memory offset overflow");
        self.next_shared_id += 1;

        // Write shared_object_t: { seq: i64, id: u64, shm: object_shm_t }
        // Uses atomic stores with Release ordering to match Wine's WriteRelease64
        // protocol. The client reads these with acquire semantics via seqlock.
        if !self.session_map.is_null()
            && (shared_offset as usize + SHARED_OBJECT_STRIDE) <= self.session_size
        {
            unsafe {
                let base = self.session_map.add(shared_offset as usize);
                let seq_ptr = base as *const AtomicI64;
                let id_ptr = base.add(8) as *const AtomicU64;
                (*seq_ptr).store(0, std::sync::atomic::Ordering::Release); // seq = 0 (even)
                (*id_ptr).store(shared_id, std::sync::atomic::Ordering::Release); // id
                // shm data at offset 16 is zero-initialized (from ftruncate)
            }
        }

        // Write user_entry into session memfd at index * 32
        // user_entry layout (32 bytes):
        //   u64 offset  → points to shared_object_t in session memfd
        //   u32 tid
        //   u32 pid
        //   u64 id      → matches shared_object_t.id
        //   u16 type
        //   u16 generation
        //   u32 padding
        // pwrite is a syscall (full memory barrier), so the shared_object
        // writes above are guaranteed visible before the user_entry.
        if self.session_fd >= 0 {
            let entry_offset = (index as usize) * USER_ENTRY_SIZE;
            let mut entry = [0u8; USER_ENTRY_SIZE];
            entry[0..8].copy_from_slice(&shared_offset.to_le_bytes());
            entry[8..12].copy_from_slice(&tid.to_le_bytes());
            entry[12..16].copy_from_slice(&pid.to_le_bytes());
            entry[16..24].copy_from_slice(&shared_id.to_le_bytes());
            entry[24..26].copy_from_slice(&obj_type.to_le_bytes());
            entry[26..28].copy_from_slice(&0u16.to_le_bytes()); // generation = 0
            unsafe {
                libc::pwrite(self.session_fd, entry.as_ptr() as *const _, USER_ENTRY_SIZE, entry_offset as i64);
            }
        }

        handle
    }


    /// Reassign a user handle's tid/pid in the session memfd.
    /// Used to give the desktop window a valid owner so Wine's
    /// get_window_thread returns non-zero (prevents WM_NULL drops).
    pub(crate) fn reassign_user_handle_owner(&self, handle: u32, tid: u32, pid: u32) {
        const FIRST_USER_HANDLE: u32 = 0x0020;
        const USER_ENTRY_SIZE: usize = 32;
        if handle < FIRST_USER_HANDLE { return; }
        let index = (handle - FIRST_USER_HANDLE) >> 1;
        let entry_offset = (index as usize) * USER_ENTRY_SIZE;
        // Overwrite tid (bytes 8-11) and pid (bytes 12-15) in the user_entry
        if self.session_fd >= 0 {
            let mut buf = [0u8; 8];
            buf[0..4].copy_from_slice(&tid.to_le_bytes());
            buf[4..8].copy_from_slice(&pid.to_le_bytes());
            unsafe {
                libc::pwrite(self.session_fd, buf.as_ptr() as *const _, 8, (entry_offset + 8) as i64);
            }
        }
    }

    /// Allocate a shared_object_t in the session memfd pool (for classes, queues, input, etc).
    /// Returns an obj_locator as [u8; 16] = { id: u64, offset: u64 }.
    fn alloc_shared_object(&mut self) -> [u8; 16] {
        const SHARED_OBJECT_STRIDE: usize = 1024;

        let offset = self.next_shared_offset;
        let id = self.next_shared_id;
        self.next_shared_offset = self.next_shared_offset
            .checked_add(SHARED_OBJECT_STRIDE as u64)
            .expect("session shared memory offset overflow");
        self.next_shared_id += 1;

        if !self.session_map.is_null()
            && (offset as usize + SHARED_OBJECT_STRIDE) <= self.session_size
        {
            unsafe {
                let base = self.session_map.add(offset as usize);
                let seq_ptr = base as *const AtomicI64;
                let id_ptr = base.add(8) as *const AtomicU64;
                (*seq_ptr).store(0, std::sync::atomic::Ordering::Release);
                (*id_ptr).store(id, std::sync::atomic::Ordering::Release);
            }
        }

        let mut locator = [0u8; 16];
        locator[0..8].copy_from_slice(&id.to_le_bytes());
        locator[8..16].copy_from_slice(&offset.to_le_bytes());
        locator
    }


    /// Perform a seqlock-protected write to a shared_object_t's data region.
    /// Matches Wine's SHARED_WRITE_BEGIN / SHARED_WRITE_END protocol.
    /// The seq field is at shared_offset+0 (i64). Data starts at shared_offset+16.
    // Seqlock-protected write to a shared_object_t's data region.
    // Matches Wine's SHARED_WRITE_BEGIN / SHARED_WRITE_END protocol:
    //   WriteRelease64(&seq, old_seq + 1)  // odd = writing
    //   ... write data ...
    //   WriteRelease64(&seq, old_seq + 2)  // even, incremented
    fn shared_write(&self, shared_offset: u64, write_fn: impl FnOnce(*mut u8)) {
        if self.session_map.is_null() { return; }
        if (shared_offset as usize + 1024) > self.session_size { return; }
        unsafe {
            let base = self.session_map.add(shared_offset as usize);
            let seq_atomic = &*(base as *const AtomicI64);
            let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
            seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release); // odd = writing
            write_fn(base.add(16)); // object_shm_t union starts at offset 16
            seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release); // even
        }
    }

    /// Write window_shm_t (class locator + dpi_context) with seqlock protection.
    /// window_shm_t layout: class (obj_locator, 16 bytes) + dpi_context (u32).
    fn set_window_shm(&self, shared_offset: u64, class_locator: &[u8; 16], dpi_context: u32) {
        self.shared_write(shared_offset, |shm| unsafe {
            std::ptr::copy_nonoverlapping(class_locator.as_ptr(), shm, 16);
            *(shm.add(16) as *mut u32) = dpi_context;
        });
    }


    /// Set QS_* bits in a thread's queue wake_bits (shared memory) and signal
    /// the queue's fsync handle so MsgWaitForMultipleObjects unblocks.
    pub(crate) fn set_queue_bits_for_tid(&mut self, tid: u32, bits: u32) {
        let client = self.clients.values().find(|c| c.thread_id == tid);
        if let Some(client) = client {
            let locator = client.queue_locator;
            let offset = u64::from_le_bytes([
                locator[8], locator[9], locator[10], locator[11],
                locator[12], locator[13], locator[14], locator[15],
            ]) as usize;
            if !self.session_map.is_null() && (offset + 1024) <= self.session_size {
                unsafe {
                    let base = self.session_map.add(offset);
                    let seq_atomic = &*(base as *const AtomicI64);
                    let shm = base.add(16);
                    let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                    seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                    let wake_bits_ptr = shm.add(12) as *mut u32;
                    let changed_bits_ptr = shm.add(20) as *mut u32;
                    *wake_bits_ptr |= bits;
                    *changed_bits_ptr |= bits;
                    seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                }
            }
            let mut queue_handle = client.queue_handle;
            let pid = client.process_id;
            let client_fd_for_alloc = self.clients.iter()
                .find(|(_, c)| c.thread_id == tid)
                .map(|(&fd, _)| fd);

            // Auto-create queue handle if needed. Wine's send_message can
            // signal QS_SMRESULT before the thread calls get_msg_queue_handle.
            // Without a queue handle, the ntsync event doesn't exist and
            // the thread blocks forever in wait_message_reply.
            if queue_handle == 0 {
                if let Some(cfd) = client_fd_for_alloc {
                    let h = self.alloc_waitable_handle_for_client(cfd as i32);
                    if h != 0 {
                        if let Some(c) = self.clients.get_mut(&cfd) {
                            c.queue_handle = h;
                        }
                        queue_handle = h;
                    }
                }
            }

            if queue_handle != 0 {
                if let Some((obj, _)) = self.ntsync_objects.get(&(pid, queue_handle)) {
                    let _ = obj.event_set();
                }
            }
        }
    }

    /// Clear specific wake_bits AND changed_bits for a thread's queue shared memory.
    /// Used by handle_get_message when the requested categories are empty, so the
    /// client's check_queue_masks() doesn't keep returning "wake" on stale state.
    /// Both bits must be cleared together — see set_queue_bits_for_tid which sets both.
    pub(crate) fn clear_queue_bits_for_tid(&mut self, tid: u32, bits: u32) {
        let client = self.clients.values().find(|c| c.thread_id == tid);
        if let Some(client) = client {
            let locator = client.queue_locator;
            let offset = u64::from_le_bytes([
                locator[8], locator[9], locator[10], locator[11],
                locator[12], locator[13], locator[14], locator[15],
            ]) as usize;
            if !self.session_map.is_null() && (offset + 1024) <= self.session_size {
                unsafe {
                    let base = self.session_map.add(offset);
                    let seq_atomic = &*(base as *const AtomicI64);
                    let shm = base.add(16);
                    let old_seq = seq_atomic.load(std::sync::atomic::Ordering::Relaxed);
                    seq_atomic.store(old_seq | 1, std::sync::atomic::Ordering::Release);
                    let wake_bits_ptr = shm.add(12) as *mut u32;
                    let changed_bits_ptr = shm.add(20) as *mut u32;
                    *wake_bits_ptr &= !bits;
                    *changed_bits_ptr &= !bits;
                    seq_atomic.store((old_seq & !1) + 2, std::sync::atomic::Ordering::Release);
                }
            }
        }
    }

    /// Update TickCount and InterruptTime in the User Shared Data page.
    /// Wine's get_tick_count() reads TickCount from this shared page.
    /// Without updates, check_queue_masks() freshness check always fails → spin.
    pub(crate) fn update_usd_time(&self) {
        if self.usd_map.is_null() { return; }
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        // Use CLOCK_MONOTONIC (Wine server uses CLOCK_BOOTTIME with CLOCK_MONOTONIC fallback)
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
        let monotonic_ticks = (ts.tv_sec as u64) * 10_000_000 + (ts.tv_nsec as u64) / 100; // 100ns units
        let tick_count = monotonic_ticks / 10_000; // milliseconds
        unsafe {
            let d = self.usd_map;
            // InterruptTime (0x008): KSYSTEM_TIME { LowPart(+0), High1Time(+4), High2Time(+8) }
            // Write order: High2Time, LowPart, High1Time (seqlock pattern for readers).
            // Platform assumption: x86-64 guarantees aligned 32-bit stores are atomic and
            // visible in program order to other cores (TSO). On non-TSO architectures
            // (ARM64), these would need explicit store-release fences.
            *(d.add(0x010) as *mut i32) = (monotonic_ticks >> 32) as i32; // High2Time
            *(d.add(0x008) as *mut u32) = monotonic_ticks as u32;         // LowPart
            *(d.add(0x00C) as *mut i32) = (monotonic_ticks >> 32) as i32; // High1Time
            // TickCount (0x320): KSYSTEM_TIME
            *(d.add(0x328) as *mut i32) = (tick_count >> 32) as i32;      // High2Time
            *(d.add(0x320) as *mut u32) = tick_count as u32;              // LowPart
            *(d.add(0x324) as *mut i32) = (tick_count >> 32) as i32;      // High1Time
            // TickCountLowDeprecated (0x000)
            *(d.add(0x000) as *mut u32) = tick_count as u32;
        }
    }





    // Wine protocol: deferred Select replies go to wait_fd as a 16-byte
    // wake_up_reply { cookie: u64, signaled: i32, __pad: i32 }.
    fn send_wake_up(&self, pw: &PendingWait, signaled: i32) {
        if let Some(client) = self.clients.get(&pw.client_fd) {
            if let Some(wait_fd) = client.wait_fd {
                let reply = WakeUpReply {
                    cookie: pw.cookie,
                    signaled,
                    _pad: 0,
                };
                unsafe {
                    libc::write(
                        wait_fd,
                        &reply as *const _ as *const _,
                        std::mem::size_of::<WakeUpReply>(),
                    );
                }
                // DO NOT signal alert here — non-APC wake. See send_select_wake.
            }
        }
    }






    /// Create a fresh ntsync event. Always creates new — never reuses from
    /// freelist. Recycled events share kernel objects with client caches,
    /// causing cross-waiter corruption.
    pub(super) fn get_or_create_event(&mut self, manual: bool, signaled: bool) -> Option<Arc<crate::ntsync::NtsyncObj>> {
        // FREELIST DISABLED: recycled events share kernel objects with clients
        // who cached the fd via get_inproc_sync_fd. Reusing them causes:
        // - Event reset wakes wrong waiter
        // - Correct waiter blocks forever
        // Always create fresh kernel objects instead.
        self.ntsync.as_ref()?.create_event(manual, signaled).map(Arc::new)
    }

    /// Insert an ntsync event and mark it as recyclable.
    pub(super) fn insert_recyclable_event(&mut self, pid: u32, handle: obj_handle_t, obj: Arc<crate::ntsync::NtsyncObj>, sync_type: u32) {
        self.ntsync_objects.insert((pid, handle), (obj, sync_type));
        self.ntsync_recyclable.insert((pid, handle));
        self.ntsync_objects_created += 1;
    }

    /// Remove an ntsync object. DO NOT recycle to freelist — the client may
    /// still hold a cached fd to this kernel object. Recycling resets the event
    /// and reuses the kernel object for a different handle, causing cross-waiter
    /// corruption (signals wake wrong waiters, correct waiters block forever).
    /// Just drop the server's fd. The kernel object stays alive if the client
    /// still holds a dup'd fd.
    pub(super) fn remove_ntsync_obj(&mut self, pid: u32, handle: obj_handle_t) {
        // Keep the ntsync object alive — the client may still hold a cached
        // dup'd fd from get_inproc_sync_fd. Dropping the Arc here would close
        // the server's fd reference. If the client later calls get_inproc_sync_fd
        // for the same handle (e.g. after ResetEvent re-caches), we need the
        // entry to still exist. The kernel object stays alive via the client's
        // cached fd anyway, but the server-side map lookup must succeed.
        //
        // Only remove from recyclable set (so the event isn't reused for other
        // handles), but keep the (pid, handle) → Arc entry.
        self.ntsync_recyclable.remove(&(pid, handle));
    }

    /// Take the listener out of EventLoop (for the acceptor thread).
    pub fn take_listener(&mut self) -> Listener {
        self.listener.take().expect("listener already taken")
    }

    /// Get or create a per-client alert event for cancelling blocked wait threads.
    fn get_or_create_alert(&mut self, client_fd: RawFd) -> RawFd {
        if let Some(alert) = self.client_alerts.get(&client_fd) {
            return alert.fd();
        }
        if let Some(obj) = self.get_or_create_event(true, false) {
            let fd = obj.fd();
            self.client_alerts.insert(client_fd, obj);
            return fd;
        }
        -1 // no ntsync available — alert won't work, but wait still functions
    }

    /// Get or create a per-client worker interrupt event for daemon-side wait threads.
    /// Auto-reset (manual=false) so the ntsync ioctl consumes the signal atomically.
    fn get_or_create_worker_interrupt(&mut self, client_fd: RawFd) -> RawFd {
        if let Some(obj) = self.client_worker_interrupts.get(&client_fd) {
            return obj.fd();
        }
        if let Some(obj) = self.get_or_create_event(false, false) {
            let fd = obj.fd();
            self.client_worker_interrupts.insert(client_fd, obj);
            return fd;
        }
        -1
    }

    /// Get or create a per-client APC flag shared with ntsync wait threads.
    pub(super) fn get_or_create_apc_flag(&mut self, client_fd: RawFd) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.client_apc_flags.entry(client_fd)
            .or_insert_with(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .clone()
    }
}

impl EventLoop {
    pub(crate) fn track_peak_clients(&mut self) {
        if self.clients.len() > self.peak_clients {
            self.peak_clients = self.clients.len();
        }
    }

    pub(crate) fn dump_ntsync_state(&self) {
        log_info!("NTSYNC STATE DUMP ({} objects):", self.ntsync_objects.len());
        // Group by pid, show named events specially
        let mut by_pid: std::collections::HashMap<u32, Vec<String>> = std::collections::HashMap::new();
        for ((pid, handle), (obj, sync_type)) in &self.ntsync_objects {
            let state = match *sync_type {
                1 | 2 => obj.event_read()
                    .map(|(m,s)| format!("manual={m} signaled={s}"))
                    .unwrap_or("read_err".into()),
                3 => obj.mutex_read()
                    .map(|(o,c)| format!("owner={o} count={c}"))
                    .unwrap_or("read_err".into()),
                4 => obj.sem_read()
                    .map(|(c,mx)| format!("count={c} max={mx}"))
                    .unwrap_or("read_err".into()),
                _ => "?".into(),
            };
            let tn = match *sync_type { 1 => "INT", 2 => "EVT", 3 => "MUT", 4 => "SEM", _ => "?" };
            // Only log non-signaled events and non-internal types (reduce noise)
            let interesting = match *sync_type {
                2 => true, // all EVT
                3 => true, // all MUT
                4 => true, // all SEM
                1 => {
                    // INT: only if NOT signaled (these should be signaled)
                    obj.event_read().map(|(_, s)| s == 0).unwrap_or(true)
                }
                _ => true,
            };
            if interesting {
                by_pid.entry(*pid).or_default()
                    .push(format!("  {handle:#06x} {tn} [{state}]"));
            }
        }
        for (pid, entries) in &by_pid {
            log_info!("  pid={pid} ({} interesting):", entries.len());
            for e in entries.iter().take(20) {
                log_info!("{e}");
            }
            if entries.len() > 20 {
                log_info!("  ... and {} more", entries.len() - 20);
            }
        }
        // Named events
        log_info!("NAMED SYNC ({}):", self.named_sync.len());
        for (name, &(fd, _st)) in &self.named_sync {
            let obj = crate::ntsync::NtsyncObj::from_raw_fd(unsafe { libc::dup(fd) });
            let state = obj.event_read()
                .map(|(m,s)| format!("manual={m} signaled={s}"))
                .unwrap_or("read_err".into());
            log_info!("  \"{name}\" [{state}]");
        }
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        if self.total_requests > 0 {
            let profiles = self.sent_messages.snapshot_profiles();
            self.intel.flush(&profiles);
            self.registry.dump_keys();
            self.dump_opcode_stats();
            // Prometheus generation removed — montauk handles tracing now.
            // Dump pending thread/process exit events that never fired
            if !self.thread_exit_events.is_empty() {
                log_warn!("STALE thread_exit_events ({} fds):", self.thread_exit_events.len());
                for (fd, entries) in &self.thread_exit_events {
                    let client_info = self.clients.get(fd)
                        .map(|c| format!("pid={} tid={}", c.process_id, c.thread_id))
                        .unwrap_or_else(|| "NO CLIENT".to_string());
                    for (creator_pid, handle, obj) in entries {
                        log_warn!("  fd={fd} {client_info} → handle={handle:#x} creator_pid={creator_pid} exit_obj_fd={}", obj.fd());
                    }
                }
            }
            if !self.process_exit_events.is_empty() {
                log_warn!("STALE process_exit_events ({} pids):", self.process_exit_events.len());
                for (pid, entries) in &self.process_exit_events {
                    for (parent_pid, handle, _) in entries {
                        log_warn!("  pid={pid} → handle={handle:#x} parent_pid={parent_pid}");
                    }
                }
            }
        }
        // Kill Wine child processes. Without this, orphaned Wine processes
        // spin on reconnect after the daemon exits, pegging CPU.
        let mut killed_pids = std::collections::HashSet::new();
        for client in self.clients.values() {
            if client.unix_pid > 0 && killed_pids.insert(client.unix_pid) {
                unsafe { libc::kill(client.unix_pid, libc::SIGKILL); }
            }
        }
        if !killed_pids.is_empty() {
            log_info!("shutdown: killed {} Wine child processes", killed_pids.len());
        }
        // Close named_sync canonical fds (created via mem::forget)
        for (_, (fd, _)) in &self.named_sync {
            unsafe { libc::close(*fd); }
        }
        unsafe {
            libc::close(self.timer_fd);
            libc::close(self.epoll_fd);
        }
    }
}

fn epoll_add(epoll_fd: RawFd, fd: RawFd, events: u32) {
    let mut ev = libc::epoll_event {
        events,
        u64: fd as u64,
    };
    let rc = unsafe {
        libc::epoll_ctl(epoll_fd, libc::EPOLL_CTL_ADD, fd, &mut ev)
    };
    if rc < 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        log_warn!("epoll_add FAILED: fd={fd} errno={errno}");
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
}

#[inline]
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
    // Wine reads sizeof(union generic_reply) = 64 bytes for every reply.
    // Always write the full 64-byte block (zero-padded).
    Reply::Fixed { buf, len: 64 }
}

// Serialize a fixed reply struct + variable-length data (VARARG).
// The caller must set header.reply_size = vararg.len() before calling.
#[inline]
fn reply_vararg<T>(reply: &T, vararg: &[u8]) -> Reply {
    let fixed_size = std::mem::size_of::<T>();
    // Wine reads 64 bytes (sizeof(union generic_reply)) then reply_size bytes.
    // Pad the fixed part to 64 bytes before appending VARARG data.
    let mut out = vec![0u8; 64 + vararg.len()];
    unsafe {
        std::ptr::copy_nonoverlapping(
            reply as *const T as *const u8,
            out.as_mut_ptr(),
            fixed_size,
        );
    }
    out[64..].copy_from_slice(vararg);
    Reply::Vararg(out)
}

// Read the client's max accepted VARARG reply size from the request header.
// RequestHeader layout: req (i32) + request_size (u32) + reply_size (u32)
#[inline]
fn max_reply_vararg(buf: &[u8]) -> u32 {
    if buf.len() >= 12 {
        u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]])
    } else {
        0
    }
}

// Convert UTF-16LE bytes to a Rust String
fn utf16le_to_string(bytes: &[u8]) -> String {
    let chars: Vec<u16> = bytes.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&chars)
}

// Convert VARARG bytes (UTF-16LE) to Vec<u16> for atom name lookup
fn vararg_to_u16(bytes: &[u8]) -> Vec<u16> {
    bytes.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

// Extract the object name from an object_attributes VARARG.
// Layout: struct object_attributes { rootdir: u32, attributes: u32, sd_len: u32, name_len: u32 }
//         followed by sd[sd_len] bytes, then name[name_len] bytes (UTF-16LE).
fn extract_objattr_name(buf: &[u8]) -> Option<String> {
    if buf.len() < VARARG_OFF + 16 { return None; }
    let oa = &buf[VARARG_OFF..];
    let sd_len = u32::from_le_bytes([oa[8], oa[9], oa[10], oa[11]]) as usize;
    let name_len = u32::from_le_bytes([oa[12], oa[13], oa[14], oa[15]]) as usize;
    if name_len == 0 { return None; }
    let name_start = VARARG_OFF + 16 + sd_len;
    let name_end = name_start + name_len;
    if name_end > buf.len() { return None; }
    let name = utf16le_to_string(&buf[name_start..name_end]);
    // Normalize: extract basename after last backslash, lowercase
    let short = name.rsplit('\\').next().unwrap_or(&name);
    if short.is_empty() { return None; }
    Some(short.to_lowercase())
}

// Extract object name from open_* VARARG (bare unicode_str, no object_attributes wrapper).
fn extract_open_name(buf: &[u8]) -> Option<String> {
    if buf.len() <= VARARG_OFF { return None; }
    let name = utf16le_to_string(&buf[VARARG_OFF..]);
    let short = name.rsplit('\\').next().unwrap_or(&name);
    if short.is_empty() { return None; }
    Some(short.to_lowercase())
}

// Convert Wine's absolute timeout (FILETIME: 100ns since 1601-01-01) to Duration from now.
const TICKS_1601_TO_1970: i64 = 116_444_736_000_000_000;

fn create_usd_memfd() -> Option<(RawFd, *mut u8)> {
    let fd = unsafe {
        libc::memfd_create(b"wine_usd\0".as_ptr() as *const libc::c_char, 0)
    };
    if fd < 0 { return None; }

    if unsafe { libc::ftruncate(fd, 0x1000) } < 0 {
        unsafe { libc::close(fd); }
        return None;
    }

    // Memory-map persistently — kept alive to update TickCount/InterruptTime
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(), 0x1000,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, fd, 0,
        )
    };
    if ptr == libc::MAP_FAILED {
        unsafe { libc::close(fd); }
        return None;
    }
    let data = ptr as *mut u8;
    unsafe {
        // TickCountMultiplier (0x004): standard Windows value
        *(data.add(0x004) as *mut u32) = 0x0FA00000;
        // ImageNumberLow (0x02C): I386
        *(data.add(0x02C) as *mut u16) = 0x014C;
        // ImageNumberHigh (0x02E): AMD64
        *(data.add(0x02E) as *mut u16) = 0x8664;
        // NtBuildNumber (0x260)
        *(data.add(0x260) as *mut u32) = 0xF000 | 19044;
        // NtProductType (0x264): VER_NT_WORKSTATION
        *(data.add(0x264) as *mut u32) = 1;
        // ProductTypeIsValid (0x268)
        *(data.add(0x268) as *mut u8) = 1;
        // NtMajorVersion (0x26C)
        *(data.add(0x26C) as *mut u32) = 10;
        // NtMinorVersion (0x270)
        *(data.add(0x270) as *mut u32) = 0;
        // NumberOfPhysicalPages (0x2E8): ~4GB worth of 4KB pages
        *(data.add(0x2E8) as *mut u32) = 1024 * 1024;
        // NativeProcessorArchitecture (0x33A): PROCESSOR_ARCHITECTURE_AMD64 = 9
        *(data.add(0x33A) as *mut u16) = 9;
    }

    Some((fd, data))
}

// Create a memfd for the session shared memory (\KernelObjects\__wine_session)
// Contains user_entries table for window/desktop handles. All zeroed initially.
fn create_session_memfd(size: usize) -> RawFd {
    let fd = unsafe {
        libc::memfd_create(b"wine_session\0".as_ptr() as *const libc::c_char, 0)
    };
    if fd < 0 { return -1; }

    if unsafe { libc::ftruncate(fd, size as libc::off_t) } < 0 {
        unsafe { libc::close(fd); }
        return -1;
    }

    fd
}

/// Default screen resolution when PARALLAX is not running.
/// The real resolution comes from PARALLAX shared memory or the display driver
/// via set_winstation_monitors at runtime.
fn default_resolution() -> (i32, i32) {
    (1920, 1080)
}

impl EventLoop {
    /// Apply real display hardware data from PARALLAX shared memory.
    /// Updates desktop window rect and display GUID to match real hardware.
    pub fn apply_display_data(&mut self, dd: &crate::display::DisplayData) {
        let (w, h) = dd.primary_resolution();

        // Update desktop window rect to real primary resolution
        let mut desktop_rect = [0u8; 16];
        desktop_rect[8..12].copy_from_slice(&w.to_le_bytes());
        desktop_rect[12..16].copy_from_slice(&h.to_le_bytes());

        if let Some(ws) = self.window_states.get_mut(&self.desktop_top_window) {
            ws.window_rect = desktop_rect;
            ws.client_rect = desktop_rect;
            ws.visible_rect = desktop_rect;
            ws.surface_rect = desktop_rect;
        }

        // Update __wine_display_device_guid to real GPU-based GUID
        let real_guid = dd.gpu_guid();
        let guid_u16: Vec<u16> = real_guid.encode_utf16().collect();
        let guid_bytes: Vec<u8> = guid_u16.iter().flat_map(|c| c.to_le_bytes()).collect();
        let guid_atom = self.state.next_atom;
        self.state.next_atom += 1;
        self.state.atoms.insert(guid_atom, (guid_bytes, 1));
        self.state.atom_names.insert(guid_u16, guid_atom);

        // Find and update the existing property
        let prop_name = "__wine_display_device_guid";
        let prop_u16: Vec<u16> = prop_name.encode_utf16().collect();
        if let Some(&prop_atom) = self.state.atom_names.get(&prop_u16) {
            self.window_properties.insert((self.desktop_top_window, prop_atom), guid_atom as u64);
        }

        // Update registry: GraphicsDriver under the real GUID path
        let (_, drv_dll) = dd.display_driver();
        self.registry.update_display_guid(&real_guid, drv_dll);

        // Populate full display device registry chain from PARALLAX data
        self.registry.apply_display_registry(dd);

        log_info!("display: applied PARALLAX data — {}x{} GUID={real_guid} connectors={}",
            w, h, dd.connectors.len());
    }
}
