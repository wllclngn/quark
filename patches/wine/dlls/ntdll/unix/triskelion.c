// triskelion.c -- Shared memory message queue bypass + ntsync for Wine
//
// Two independent fast paths, both intercepted at server_call_unlocked:
//
// 1. Message queue bypass: GetMessage/PostMessage via shared memory ring
//    buffer. Same-process posted messages never touch the wineserver.
//
// 2. ntsync: NT sync primitives (mutex, semaphore, event, wait) routed
//    to /dev/ntsync ioctls. Eliminates wineserver roundtrip for the
//    hottest sync paths. Uses a shadow table: server still manages
//    handles, we shadow each sync object with an ntsync fd.
//
// Integration (applied by install.py):
//   - triskelion.c added to dlls/ntdll/Makefile.in (SOURCES)
//   - triskelion_try_bypass() called in server_call_unlocked() (pre-hook)
//   - triskelion_post_call() called after server_call_unlocked() (post-hook)
//   - Set WINE_TRISKELION=1 to enable message bypass
//   - ntsync auto-enables when /dev/ntsync exists

#if 0
#pragma makedep unix
#endif

#include "config.h"

#include <stdint.h>
#include <stdatomic.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <pthread.h>
#include <dirent.h>
#include <limits.h>
#ifdef HAVE_SYS_SYSCALL_H
# include <sys/syscall.h>
#endif
#ifdef HAVE_LINUX_FUTEX_H
# include <linux/futex.h>
#endif
#include <sys/ioctl.h>

// ntsync kernel interface — inline definitions so we don't require the header
// at compile time. These match linux/ntsync.h from kernel 6.14+.
#ifndef NTSYNC_IOC_CREATE_SEM
struct ntsync_sem_args   { uint32_t count; uint32_t max; };
struct ntsync_mutex_args { uint32_t owner; uint32_t count; };
struct ntsync_event_args { uint32_t manual; uint32_t signaled; };
struct ntsync_wait_args  { uint64_t timeout; uint64_t objs; uint32_t count; uint32_t index;
                           uint32_t flags; uint32_t owner; uint32_t alert; uint32_t pad; };
#define NTSYNC_IOC_CREATE_SEM   _IOW ('N', 0x80, struct ntsync_sem_args)
#define NTSYNC_IOC_WAIT_ANY     _IOWR('N', 0x82, struct ntsync_wait_args)
#define NTSYNC_IOC_WAIT_ALL     _IOWR('N', 0x83, struct ntsync_wait_args)
#define NTSYNC_IOC_CREATE_MUTEX _IOW ('N', 0x84, struct ntsync_mutex_args)
#define NTSYNC_IOC_CREATE_EVENT _IOW ('N', 0x87, struct ntsync_event_args)
#define NTSYNC_IOC_SEM_RELEASE  _IOWR('N', 0x81, uint32_t)
#define NTSYNC_IOC_MUTEX_UNLOCK _IOWR('N', 0x85, struct ntsync_mutex_args)
#define NTSYNC_IOC_EVENT_SET    _IOR ('N', 0x88, uint32_t)
#define NTSYNC_IOC_EVENT_RESET  _IOR ('N', 0x89, uint32_t)
#define NTSYNC_IOC_EVENT_PULSE  _IOR ('N', 0x8a, uint32_t)
#define NTSYNC_WAIT_REALTIME    0x1
#endif
#define NTSYNC_MAX_WAIT_COUNT 64

#include "ntstatus.h"
#define WIN32_NO_STATUS
#include "windef.h"
#include "winternl.h"
#include "wine/server.h"
#include "unix_private.h"

// Shared memory layout (must match triskelion's queue.rs and triskelion_shm.h)

#define TRISKELION_SHM_MAGIC   0x54524953
#define TRISKELION_SHM_VERSION 1
#define TRISKELION_MAX_THREADS 256
#define TRISKELION_RING_CAPACITY 256
#define TRISKELION_RING_MASK (TRISKELION_RING_CAPACITY - 1)

struct triskelion_header {
    uint32_t magic;
    uint32_t version;
    uint32_t max_threads;
    uint32_t queue_size;
    _Atomic uint32_t next_slot;
    uint8_t _reserved[44];
} __attribute__((aligned(64)));

struct triskelion_message {
    uint32_t win;
    uint32_t msg;
    uint64_t wparam;
    uint64_t lparam;
    int32_t  msg_type;
    int32_t  x;
    int32_t  y;
    uint32_t time;
    uint32_t _pad[2];
};

struct triskelion_cacheline_u64 {
    _Atomic uint64_t val;
    uint8_t _pad[56];
} __attribute__((aligned(64)));

struct triskelion_ring {
    struct triskelion_cacheline_u64 write_pos;
    struct triskelion_cacheline_u64 read_pos;
    struct triskelion_message buf[TRISKELION_RING_CAPACITY];
};

struct triskelion_queue {
    struct triskelion_ring posted;
    _Atomic uint32_t wake_bits;
    _Atomic uint32_t post_lock; // spinlock for ring_push (multiple producers)
    uint32_t thread_id;
    uint8_t _reserved[52];
};

#define TRISKELION_HEADER_SIZE sizeof(struct triskelion_header)
#define TRISKELION_QUEUE_SIZE  sizeof(struct triskelion_queue)

#define QS_POSTMESSAGE_BIT 0x0008

// Global state (process-wide)
static int triskelion_enabled = -1;
static void *shm_base = NULL;
static size_t shm_size = 0;

// Per-thread state
static __thread uint32_t tls_slot = UINT32_MAX;
static __thread struct triskelion_queue *tls_queue = NULL;

// Process-local Wine TID -> shm slot map for same-process PostMessage.
// Hard cap at 128 threads. Thread 129+ silently falls through to the
// wineserver for all PostMessage/GetMessage operations (safe -- the
// wineserver handles them correctly, just without the shm fast path).
// Linear scan O(n) on every cross-thread PostMessage lookup.
#define MAX_LOCAL_THREADS 128
static struct { uint32_t tid; uint32_t slot; } local_map[MAX_LOCAL_THREADS];
static int local_map_count = 0;
static pthread_mutex_t local_map_lock = PTHREAD_MUTEX_INITIALIZER;

// Check if any process has a given /dev/shm path mapped.
// Returns 1 if at least one process maps it, 0 otherwise.
static int shm_is_mapped(const char *shm_path)
{
    DIR *proc_dir;
    struct dirent *pent;
    char maps_path[280];
    char line[512];
    FILE *fp;
    int found = 0;

    proc_dir = opendir("/proc");
    if (!proc_dir) return 1; // assume mapped if we can't check

    while ((pent = readdir(proc_dir)) != NULL)
    {
        if (pent->d_name[0] < '0' || pent->d_name[0] > '9')
            continue;
        snprintf(maps_path, sizeof(maps_path), "/proc/%s/maps", pent->d_name);
        fp = fopen(maps_path, "r");
        if (!fp) continue;
        while (fgets(line, sizeof(line), fp))
        {
            if (strstr(line, shm_path))
            {
                found = 1;
                fclose(fp);
                goto done;
            }
        }
        fclose(fp);
    }
done:
    closedir(proc_dir);
    return found;
}

// Scan /dev/shm for orphaned triskelion segments and unlink them.
// Skips my_name (the segment we're about to use). Runs once per process.
static void triskelion_cleanup_stale(const char *my_name)
{
    static int done = 0;
    DIR *shm_dir;
    struct dirent *ent;
    char full_path[280];
    const char *skip = my_name + 1; // strip leading '/' for filename comparison

    if (done) return;
    done = 1;

    shm_dir = opendir("/dev/shm");
    if (!shm_dir) return;

    while ((ent = readdir(shm_dir)) != NULL)
    {
        if (strncmp(ent->d_name, "triskelion-", 11) != 0)
            continue;
        if (strcmp(ent->d_name, skip) == 0)
            continue;

        snprintf(full_path, sizeof(full_path), "/dev/shm/%s", ent->d_name);

        if (!shm_is_mapped(full_path))
        {
            char shm_unlink_name[280];
            snprintf(shm_unlink_name, sizeof(shm_unlink_name), "/%s", ent->d_name);
            shm_unlink(shm_unlink_name);
        }
    }

    closedir(shm_dir);
}

static inline void triskelion_futex_wake(_Atomic uint32_t *addr)
{
    syscall(__NR_futex, addr, 1 /* FUTEX_WAKE */, INT_MAX, NULL, NULL, 0);
}

// Environment variable gate (cached, like do_fsync)
int do_triskelion(void)
{
    if (triskelion_enabled == -1)
    {
        const char *env = getenv("WINE_TRISKELION");
        triskelion_enabled = env && atoi(env);
    }
    return triskelion_enabled;
}

// Open or create the shared memory file
static int triskelion_shm_open(void)
{
    struct stat st;
    char shm_name[64];
    int fd;
    const char *prefix;

    if (shm_base) return 1;

    prefix = getenv("WINEPREFIX");
    if (!prefix)
    {
        const char *home = getenv("HOME");
        if (!home) return 0;
        char buf[256];
        snprintf(buf, sizeof(buf), "%s/.wine", home);
        if (stat(buf, &st) == -1) return 0;
    }
    else
    {
        if (stat(prefix, &st) == -1) return 0;
    }

    snprintf(shm_name, sizeof(shm_name), "/triskelion-%lx%lx",
             (unsigned long)st.st_dev, (unsigned long)st.st_ino);

    triskelion_cleanup_stale(shm_name);

    shm_size = TRISKELION_HEADER_SIZE + TRISKELION_MAX_THREADS * TRISKELION_QUEUE_SIZE;

    // Try open existing first (triskelion server may have created it)
    fd = shm_open(shm_name, O_RDWR, 0600);
    if (fd == -1)
    {
        // Remove stale segment from a crashed prior run, then create fresh
        shm_unlink(shm_name);
        fd = shm_open(shm_name, O_CREAT | O_EXCL | O_RDWR, 0600);
        if (fd == -1 && errno == EEXIST)
        {
            // Race: another process created it between unlink and open
            fd = shm_open(shm_name, O_RDWR, 0600);
        }
        if (fd == -1) return 0;
        if (ftruncate(fd, shm_size) == -1)
        {
            close(fd);
            return 0;
        }
    }

    shm_base = mmap(NULL, shm_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);

    if (shm_base == MAP_FAILED)
    {
        shm_base = NULL;
        return 0;
    }

    // Verify or initialize header
    struct triskelion_header *hdr = (struct triskelion_header *)shm_base;
    if (hdr->magic == 0)
    {
        hdr->magic = TRISKELION_SHM_MAGIC;
        hdr->version = TRISKELION_SHM_VERSION;
        hdr->max_threads = TRISKELION_MAX_THREADS;
        hdr->queue_size = TRISKELION_QUEUE_SIZE;
    }
    else if (hdr->magic != TRISKELION_SHM_MAGIC || hdr->version != TRISKELION_SHM_VERSION)
    {
        munmap(shm_base, shm_size);
        shm_base = NULL;
        return 0;
    }

    return 1;
}

// Claim a shm slot for the current thread. tid is the Wine thread ID
// (from TEB->ClientId.UniqueThread), NOT the kernel TID from gettid().
static struct triskelion_queue *triskelion_claim_slot(uint32_t tid)
{
    struct triskelion_header *hdr;
    uint32_t slot;
    struct triskelion_queue *q;

    if (!shm_base && !triskelion_shm_open()) return NULL;

    hdr = (struct triskelion_header *)shm_base;
    slot = atomic_fetch_add_explicit(&hdr->next_slot, 1, memory_order_relaxed);
    if (slot >= TRISKELION_MAX_THREADS) return NULL;

    q = (struct triskelion_queue *)((char *)shm_base + TRISKELION_HEADER_SIZE
                                    + slot * TRISKELION_QUEUE_SIZE);
    memset(q, 0, TRISKELION_QUEUE_SIZE);
    q->thread_id = tid;

    tls_slot = slot;
    tls_queue = q;

    // Store queue pointer in TEB for win32u peek_message fast path.
    // glReserved2 is unused in Wine; win32u reads it to detect pending
    // messages in the shm ring before falling back to the server.
    NtCurrentTeb()->glReserved2 = (PVOID)q;

    // Register in process-local map
    pthread_mutex_lock(&local_map_lock);
    if (local_map_count < MAX_LOCAL_THREADS)
    {
        local_map[local_map_count].tid = tid;
        local_map[local_map_count].slot = slot;
        local_map_count++;
    }
    else
    {
        static int warned = 0;
        if (!warned)
        {
            fprintf(stderr, "[triskelion] local_map full (%d threads), "
                    "new threads fall through to wineserver\n", MAX_LOCAL_THREADS);
            warned = 1;
        }
    }
    pthread_mutex_unlock(&local_map_lock);

    return q;
}

// Get current thread's queue (lazy init).
// Uses Wine thread ID (from TEB), NOT kernel TID from gettid().
// send_message requests reference threads by Wine TID (sreq->id),
// so local_map must be keyed on Wine TIDs for get_queue_by_tid() to match.
static struct triskelion_queue *get_my_queue(void)
{
    if (tls_queue) return tls_queue;
    if (!do_triskelion()) return NULL;

    uint32_t wine_tid = (uint32_t)(uintptr_t)NtCurrentTeb()->ClientId.UniqueThread;
    if (!wine_tid) return NULL; // too early (before init_thread reply)
    return triskelion_claim_slot(wine_tid);
}

// Find another thread's queue by Wine thread ID (same process only).
// local_map is keyed on Wine TIDs (matching sreq->id in send_message).
static struct triskelion_queue *get_queue_by_tid(uint32_t target_tid)
{
    int i;
    pthread_mutex_lock(&local_map_lock);
    for (i = 0; i < local_map_count; i++)
    {
        if (local_map[i].tid == target_tid)
        {
            pthread_mutex_unlock(&local_map_lock);
            return (struct triskelion_queue *)((char *)shm_base + TRISKELION_HEADER_SIZE
                                               + local_map[i].slot * TRISKELION_QUEUE_SIZE);
        }
    }
    pthread_mutex_unlock(&local_map_lock);
    return NULL;
}

// Ring pop: single consumer (owning thread only)
static int ring_pop(struct triskelion_ring *ring, struct triskelion_message *out)
{
    uint64_t rp = atomic_load_explicit(&ring->read_pos.val, memory_order_relaxed);
    uint64_t wp = atomic_load_explicit(&ring->write_pos.val, memory_order_acquire);

    if (rp == wp) return 0;

    *out = ring->buf[rp & TRISKELION_RING_MASK];
    atomic_store_explicit(&ring->read_pos.val, rp + 1, memory_order_release);
    return 1;
}

// Ring push: multiple producers possible (any thread can PostMessage to any queue).
// Caller MUST hold queue->post_lock.
static int ring_push(struct triskelion_ring *ring, const struct triskelion_message *msg)
{
    uint64_t wp = atomic_load_explicit(&ring->write_pos.val, memory_order_relaxed);
    uint64_t rp = atomic_load_explicit(&ring->read_pos.val, memory_order_acquire);

    if ((wp - rp) >= TRISKELION_RING_CAPACITY) return 0;

    ring->buf[wp & TRISKELION_RING_MASK] = *msg;
    atomic_store_explicit(&ring->write_pos.val, wp + 1, memory_order_release);
    return 1;
}

static inline void spin_lock(_Atomic uint32_t *lock)
{
    while (atomic_exchange_explicit(lock, 1, memory_order_acquire))
        while (atomic_load_explicit(lock, memory_order_relaxed))
            ; // spin on cached value
}

static inline void spin_unlock(_Atomic uint32_t *lock)
{
    atomic_store_explicit(lock, 0, memory_order_release);
}

// Try to bypass get_message via shared memory.
// Returns STATUS_SUCCESS if a message was read, STATUS_NOT_IMPLEMENTED to
// fall through to the normal server call.
static unsigned int try_bypass_get_message(void *req_ptr)
{
    struct __server_request_info *req = req_ptr;
    struct get_message_request *greq = &req->u.req.get_message_request;
    struct get_message_reply *greply = &req->u.reply.get_message_reply;
    struct triskelion_queue *queue;
    struct triskelion_message tmsg;

    queue = get_my_queue();
    if (!queue) return STATUS_NOT_IMPLEMENTED;

    // Only bypass for unfiltered posted messages (the hot path).
    // "Unfiltered" means get_first=0 and get_last=~0 (all message types).
    // peek_message in win32u converts (first=0, last=0) to (first=0, last=~0).
    // Window filter: get_win=0 means any window, which is what we support.
    if (greq->get_win != 0)
        return STATUS_NOT_IMPLEMENTED;
    if (greq->get_first != 0 || (greq->get_last != 0 && greq->get_last != ~0U))
        return STATUS_NOT_IMPLEMENTED;

    if (!ring_pop(&queue->posted, &tmsg))
        return STATUS_NOT_IMPLEMENTED;

    // Fill reply fields the same way the server would
    greply->win    = tmsg.win;
    greply->msg    = tmsg.msg;
    greply->wparam = tmsg.wparam;
    greply->lparam = tmsg.lparam;
    greply->type   = MSG_POSTED;
    greply->x      = tmsg.x;
    greply->y      = tmsg.y;
    greply->time   = tmsg.time;
    greply->total  = 0;
    req->u.reply.reply_header.error = 0;
    req->u.reply.reply_header.reply_size = 0;

    // Clear wake bit if ring is now empty
    uint64_t rp = atomic_load_explicit(&queue->posted.read_pos.val, memory_order_relaxed);
    uint64_t wp = atomic_load_explicit(&queue->posted.write_pos.val, memory_order_acquire);
    if (rp == wp)
        atomic_fetch_and_explicit(&queue->wake_bits, ~QS_POSTMESSAGE_BIT, memory_order_release);

    return STATUS_SUCCESS;
}

// Try to bypass send_message (PostMessage case) via shared memory.
static unsigned int try_bypass_send_message(void *req_ptr)
{
    struct __server_request_info *req = req_ptr;
    struct send_message_request *sreq = &req->u.req.send_message_request;
    struct triskelion_queue *target;
    struct triskelion_message tmsg;

    // Only bypass posted messages (MSG_POSTED) and notifications (MSG_NOTIFY)
    if (sreq->type != MSG_POSTED && sreq->type != MSG_NOTIFY)
        return STATUS_NOT_IMPLEMENTED;

    target = get_queue_by_tid(sreq->id);
    if (!target) return STATUS_NOT_IMPLEMENTED;

    tmsg.win        = sreq->win;
    tmsg.msg        = sreq->msg;
    tmsg.wparam     = sreq->wparam;
    tmsg.lparam     = sreq->lparam;
    tmsg.msg_type   = sreq->type;
    tmsg.x          = 0;
    tmsg.y          = 0;
    tmsg.time       = 0;
    tmsg._pad[0]    = 0;
    tmsg._pad[1]    = 0;

    spin_lock(&target->post_lock);
    if (!ring_push(&target->posted, &tmsg))
    {
        spin_unlock(&target->post_lock);
        return STATUS_NOT_IMPLEMENTED; // ring full, fall through to server
    }
    spin_unlock(&target->post_lock);

    atomic_fetch_or_explicit(&target->wake_bits, QS_POSTMESSAGE_BIT, memory_order_release);
    triskelion_futex_wake(&target->wake_bits);

    req->u.reply.reply_header.error = 0;
    req->u.reply.reply_header.reply_size = 0;
    return STATUS_SUCCESS;
}

// =========================================================================
// ntsync: kernel-native NT sync via /dev/ntsync
//
// Shadow table approach: the server still manages handles. We mirror each
// sync object (semaphore, mutex, event) with an ntsync kernel fd.
// Signal/release/wait bypass the server entirely via ioctl.
// Create goes to server first (to get handle), then we shadow in post-hook.
// Named objects (cross-process) are NOT shadowed — they fall through.
// =========================================================================

#define NTSYNC_TYPE_SEM   1
#define NTSYNC_TYPE_MUTEX 2
#define NTSYNC_TYPE_EVENT 3

struct ntsync_shadow {
    int fd;    // ntsync object fd, -1 = unused
    int type;  // NTSYNC_TYPE_*
};

// Shadow table indexed by (handle >> 2). Handles are multiples of 4.
#define NTSYNC_TABLE_SIZE 4096
static struct ntsync_shadow ntsync_table[NTSYNC_TABLE_SIZE];
static int ntsync_dev_fd = -1;
static pthread_mutex_t ntsync_lock = PTHREAD_MUTEX_INITIALIZER;

// Per-thread: saved create params for the post-hook (server overwrites
// request fields with reply data, so we stash them here before the call).
enum ntsync_pending_op {
    NTSYNC_PENDING_NONE = 0,
    NTSYNC_PENDING_CREATE_SEM,
    NTSYNC_PENDING_CREATE_MUTEX,
    NTSYNC_PENDING_CREATE_EVENT,
    NTSYNC_PENDING_CLOSE,
};
struct ntsync_pending {
    int op;
    union {
        struct { uint32_t initial; uint32_t max; } sem;
        struct { uint32_t owned; } mutex;
        struct { uint32_t manual_reset; uint32_t initial_state; } event;
        struct { obj_handle_t handle; } close;
    };
};
static __thread struct ntsync_pending tls_ntsync;

// NT epoch (1601-01-01) to Unix epoch (1970-01-01) in 100ns ticks
#define TICKS_1601_TO_1970 116444736000000000LL

static void ntsync_init_once(void)
{
    ntsync_dev_fd = open("/dev/ntsync", O_CLOEXEC | O_RDONLY);
    if (ntsync_dev_fd >= 0)
        memset(ntsync_table, 0xff, sizeof(ntsync_table)); // fd = -1 for all
}

static int ntsync_init(void)
{
    static pthread_once_t once = PTHREAD_ONCE_INIT;
    pthread_once(&once, ntsync_init_once);
    return ntsync_dev_fd >= 0;
}

static inline int handle_to_idx(obj_handle_t handle)
{
    unsigned int idx = handle >> 2;
    return (idx < NTSYNC_TABLE_SIZE) ? (int)idx : -1;
}

static void ntsync_shadow_add(obj_handle_t handle, int fd, int type)
{
    int idx = handle_to_idx(handle);
    if (idx < 0) { close(fd); return; }

    pthread_mutex_lock(&ntsync_lock);
    if (ntsync_table[idx].fd >= 0)
        close(ntsync_table[idx].fd);
    ntsync_table[idx].fd = fd;
    ntsync_table[idx].type = type;
    pthread_mutex_unlock(&ntsync_lock);
}

// Copy shadow entry under lock. Caller gets a snapshot — safe from concurrent remove.
static inline int ntsync_shadow_get(obj_handle_t handle, struct ntsync_shadow *out)
{
    int idx = handle_to_idx(handle);
    if (idx < 0) return 0;

    pthread_mutex_lock(&ntsync_lock);
    if (ntsync_table[idx].fd < 0) {
        pthread_mutex_unlock(&ntsync_lock);
        return 0;
    }
    *out = ntsync_table[idx];
    pthread_mutex_unlock(&ntsync_lock);
    return 1;
}

static void ntsync_shadow_remove(obj_handle_t handle)
{
    int idx = handle_to_idx(handle);
    if (idx < 0) return;

    pthread_mutex_lock(&ntsync_lock);
    if (ntsync_table[idx].fd >= 0)
    {
        close(ntsync_table[idx].fd);
        ntsync_table[idx].fd = -1;
    }
    pthread_mutex_unlock(&ntsync_lock);
}

// Check if a create request has a name (named = shared cross-process).
// Named objects must NOT be shadowed — different processes need the same
// kernel object, which our per-process shadow table can't provide.
static int create_request_is_named(struct __server_request_info *req)
{
    const struct object_attributes *attr;
    if (req->data_count < 1 || req->data[0].size < sizeof(*attr))
        return 0;
    attr = (const struct object_attributes *)req->data[0].ptr;
    return attr->name_len > 0;
}

static uint32_t get_wine_tid(void)
{
    return (uint32_t)(uintptr_t)NtCurrentTeb()->ClientId.UniqueThread;
}

// --- Bypass functions for signal/release/wait (pre-hook) ---

static unsigned int try_bypass_release_semaphore(void *req_ptr)
{
    struct __server_request_info *req = req_ptr;
    struct release_semaphore_request *sreq = &req->u.req.release_semaphore_request;
    struct release_semaphore_reply *reply = &req->u.reply.release_semaphore_reply;
    struct ntsync_shadow shadow;
    uint32_t count;

    if (!ntsync_init()) return STATUS_NOT_IMPLEMENTED;

    if (!ntsync_shadow_get(sreq->handle, &shadow) || shadow.type != NTSYNC_TYPE_SEM)
        return STATUS_NOT_IMPLEMENTED;

    count = sreq->count;
    if (ioctl(shadow.fd, NTSYNC_IOC_SEM_RELEASE, &count) < 0)
        return STATUS_NOT_IMPLEMENTED;

    reply->prev_count = count;
    req->u.reply.reply_header.error = 0;
    req->u.reply.reply_header.reply_size = 0;
    return STATUS_SUCCESS;
}

static unsigned int try_bypass_release_mutex(void *req_ptr)
{
    struct __server_request_info *req = req_ptr;
    struct release_mutex_request *mreq = &req->u.req.release_mutex_request;
    struct release_mutex_reply *reply = &req->u.reply.release_mutex_reply;
    struct ntsync_shadow shadow;
    struct ntsync_mutex_args args;

    if (!ntsync_init()) return STATUS_NOT_IMPLEMENTED;

    if (!ntsync_shadow_get(mreq->handle, &shadow) || shadow.type != NTSYNC_TYPE_MUTEX)
        return STATUS_NOT_IMPLEMENTED;

    args.owner = get_wine_tid();
    args.count = 0;
    if (ioctl(shadow.fd, NTSYNC_IOC_MUTEX_UNLOCK, &args) < 0)
        return STATUS_NOT_IMPLEMENTED;

    reply->prev_count = args.count;
    req->u.reply.reply_header.error = 0;
    req->u.reply.reply_header.reply_size = 0;
    return STATUS_SUCCESS;
}

static unsigned int try_bypass_event_op(void *req_ptr)
{
    struct __server_request_info *req = req_ptr;
    struct event_op_request *ereq = &req->u.req.event_op_request;
    struct event_op_reply *reply = &req->u.reply.event_op_reply;
    struct ntsync_shadow shadow;
    unsigned long ioctl_cmd;
    uint32_t signaled = 0;

    if (!ntsync_init()) return STATUS_NOT_IMPLEMENTED;

    if (!ntsync_shadow_get(ereq->handle, &shadow) || shadow.type != NTSYNC_TYPE_EVENT)
        return STATUS_NOT_IMPLEMENTED;

    switch (ereq->op)
    {
    case SET_EVENT:   ioctl_cmd = NTSYNC_IOC_EVENT_SET;   break;
    case RESET_EVENT: ioctl_cmd = NTSYNC_IOC_EVENT_RESET; break;
    case PULSE_EVENT: ioctl_cmd = NTSYNC_IOC_EVENT_PULSE; break;
    default: return STATUS_NOT_IMPLEMENTED;
    }

    if (ioctl(shadow.fd, ioctl_cmd, &signaled) < 0)
        return STATUS_NOT_IMPLEMENTED;

    reply->state = signaled;
    req->u.reply.reply_header.error = 0;
    req->u.reply.reply_header.reply_size = 0;
    return STATUS_SUCCESS;
}

static unsigned int try_bypass_select(void *req_ptr)
{
    struct __server_request_info *req = req_ptr;
    struct select_request *sreq = &req->u.req.select_request;
    struct select_reply *reply = &req->u.reply.select_reply;
    const union select_op *op;
    struct ntsync_wait_args args;
    uint32_t objs[NTSYNC_MAX_WAIT_COUNT];
    int count, i;

    if (!ntsync_init()) return STATUS_NOT_IMPLEMENTED;

    // data[0] = apc_result, data[1] = select_op
    if (req->data_count < 2) return STATUS_NOT_IMPLEMENTED;

    op = (const union select_op *)req->data[1].ptr;

    // Only handle basic waits — no signal-and-wait, no keyed events
    if (op->op != SELECT_WAIT && op->op != SELECT_WAIT_ALL)
        return STATUS_NOT_IMPLEMENTED;

    // Alertable waits involve APCs — too complex for bypass
    if (sreq->flags & SELECT_ALERTABLE)
        return STATUS_NOT_IMPLEMENTED;

    count = ((int)sreq->size - (int)sizeof(int)) / (int)sizeof(obj_handle_t);
    if (count <= 0 || count > NTSYNC_MAX_WAIT_COUNT)
        return STATUS_NOT_IMPLEMENTED;

    // Resolve all handles to ntsync fds. If ANY handle isn't shadowed
    // (e.g. file, process, thread objects), fall through to server.
    for (i = 0; i < count; i++)
    {
        struct ntsync_shadow shadow;
        if (!ntsync_shadow_get(op->wait.handles[i], &shadow))
            return STATUS_NOT_IMPLEMENTED;
        objs[i] = (uint32_t)shadow.fd;
    }

    // Convert timeout
    memset(&args, 0, sizeof(args));
    if (sreq->timeout == TIMEOUT_INFINITE)
    {
        args.timeout = UINT64_MAX;
        args.flags = 0;
    }
    else if (sreq->timeout < 0)
    {
        // Relative: negate and convert 100ns ticks → nanoseconds
        args.timeout = (uint64_t)(-sreq->timeout) * 100;
        args.flags = 0;
    }
    else
    {
        // Absolute: convert NT epoch → Unix epoch nanoseconds
        int64_t unix_100ns = (int64_t)sreq->timeout - TICKS_1601_TO_1970;
        if (unix_100ns < 0) unix_100ns = 0;
        args.timeout = (uint64_t)unix_100ns * 100;
        args.flags = NTSYNC_WAIT_REALTIME;
    }

    args.objs = (uint64_t)(uintptr_t)objs;
    args.count = (uint32_t)count;
    args.owner = get_wine_tid();
    args.alert = 0;

    if (ioctl(ntsync_dev_fd,
              op->op == SELECT_WAIT_ALL ? NTSYNC_IOC_WAIT_ALL : NTSYNC_IOC_WAIT_ANY,
              &args) < 0)
    {
        if (errno == ETIMEDOUT)
        {
            reply->signaled = STATUS_TIMEOUT;
            reply->apc_handle = 0;
            req->u.reply.reply_header.error = STATUS_TIMEOUT;
            req->u.reply.reply_header.reply_size = 0;
            return STATUS_TIMEOUT;
        }
        return STATUS_NOT_IMPLEMENTED;
    }

    reply->signaled = args.index;
    reply->apc_handle = 0;
    req->u.reply.reply_header.error = 0;
    req->u.reply.reply_header.reply_size = 0;
    return STATUS_SUCCESS;
}

// Save create params in TLS before server call (request fields get
// overwritten by reply). Returns STATUS_NOT_IMPLEMENTED so the server
// handles the actual object creation and handle allocation.
static void ntsync_pre_create(struct __server_request_info *req, int code)
{
    if (!ntsync_init()) return;

    switch (code)
    {
    case REQ_create_semaphore:
        if (create_request_is_named(req)) break;
        tls_ntsync.op = NTSYNC_PENDING_CREATE_SEM;
        tls_ntsync.sem.initial = req->u.req.create_semaphore_request.initial;
        tls_ntsync.sem.max = req->u.req.create_semaphore_request.max;
        return;

    case REQ_create_mutex:
        if (create_request_is_named(req)) break;
        tls_ntsync.op = NTSYNC_PENDING_CREATE_MUTEX;
        tls_ntsync.mutex.owned = req->u.req.create_mutex_request.owned;
        return;

    case REQ_create_event:
        if (create_request_is_named(req)) break;
        tls_ntsync.op = NTSYNC_PENDING_CREATE_EVENT;
        tls_ntsync.event.manual_reset = req->u.req.create_event_request.manual_reset;
        tls_ntsync.event.initial_state = req->u.req.create_event_request.initial_state;
        return;

    case REQ_close_handle:
        tls_ntsync.op = NTSYNC_PENDING_CLOSE;
        tls_ntsync.close.handle = req->u.req.close_handle_request.handle;
        return;
    }

    tls_ntsync.op = NTSYNC_PENDING_NONE;
}

// Main bypass dispatch. Called from server_call_unlocked before the socket I/O.
// Returns STATUS_NOT_IMPLEMENTED to fall through to the normal path.
unsigned int triskelion_try_bypass(void *req_ptr)
{
    struct __server_request_info *req = req_ptr;
    int code = req->u.req.request_header.req;

    // ntsync: save create params + handle close ops (always, independent of
    // WINE_TRISKELION message bypass). The post-hook does the actual shadow.
    tls_ntsync.op = NTSYNC_PENDING_NONE;
    ntsync_pre_create(req, code);

    // ntsync: bypass signal/release/wait operations
    switch (code)
    {
    case REQ_release_semaphore:
        return try_bypass_release_semaphore(req_ptr);
    case REQ_release_mutex:
        return try_bypass_release_mutex(req_ptr);
    case REQ_event_op:
        return try_bypass_event_op(req_ptr);
    case REQ_select:
        return try_bypass_select(req_ptr);
    default:
        break;
    }

    // Message queue bypass (requires WINE_TRISKELION=1)
    if (!do_triskelion()) return STATUS_NOT_IMPLEMENTED;

    switch (code)
    {
    case REQ_get_message:
        return try_bypass_get_message(req_ptr);
    case REQ_send_message:
        return try_bypass_send_message(req_ptr);
    default:
        return STATUS_NOT_IMPLEMENTED;
    }
}

// Post-call hook. Called from server_call_unlocked AFTER the server processes
// the request. Creates ntsync shadow objects for newly created sync handles
// and cleans up shadows on close.
void triskelion_post_call(void *req_ptr, unsigned int ret)
{
    struct __server_request_info *req = req_ptr;
    int fd;

    if (tls_ntsync.op == NTSYNC_PENDING_NONE) return;

    switch (tls_ntsync.op)
    {
    case NTSYNC_PENDING_CREATE_SEM:
        if (ret != STATUS_SUCCESS) break;
        {
            struct ntsync_sem_args args = {
                .count = tls_ntsync.sem.initial,
                .max = tls_ntsync.sem.max,
            };
            fd = ioctl(ntsync_dev_fd, NTSYNC_IOC_CREATE_SEM, &args);
            if (fd >= 0)
                ntsync_shadow_add(req->u.reply.create_semaphore_reply.handle, fd, NTSYNC_TYPE_SEM);
        }
        break;

    case NTSYNC_PENDING_CREATE_MUTEX:
        if (ret != STATUS_SUCCESS) break;
        {
            struct ntsync_mutex_args args = {
                .owner = tls_ntsync.mutex.owned ? get_wine_tid() : 0,
                .count = tls_ntsync.mutex.owned ? 1u : 0u,
            };
            fd = ioctl(ntsync_dev_fd, NTSYNC_IOC_CREATE_MUTEX, &args);
            if (fd >= 0)
                ntsync_shadow_add(req->u.reply.create_mutex_reply.handle, fd, NTSYNC_TYPE_MUTEX);
        }
        break;

    case NTSYNC_PENDING_CREATE_EVENT:
        if (ret != STATUS_SUCCESS) break;
        {
            struct ntsync_event_args args = {
                .manual = tls_ntsync.event.manual_reset,
                .signaled = tls_ntsync.event.initial_state,
            };
            fd = ioctl(ntsync_dev_fd, NTSYNC_IOC_CREATE_EVENT, &args);
            if (fd >= 0)
                ntsync_shadow_add(req->u.reply.create_event_reply.handle, fd, NTSYNC_TYPE_EVENT);
        }
        break;

    case NTSYNC_PENDING_CLOSE:
        if (ret != STATUS_SUCCESS) break;
        ntsync_shadow_remove(tls_ntsync.close.handle);
        break;

    default:
        break;
    }

    tls_ntsync.op = NTSYNC_PENDING_NONE;
}
