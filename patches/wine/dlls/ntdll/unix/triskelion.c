// triskelion.c -- Shared memory message queue bypass for Wine
//
// Modeled after fsync (dlls/ntdll/unix/fsync.c). Intercepts GetMessage
// and PostMessage at the server_call_unlocked level, checking a shared
// memory ring buffer before falling through to the wineserver socket.
//
// Same-process posted messages never touch the wineserver. Cross-process
// and non-posted messages fall through to the normal path.
//
// Integration:
//   1. Add triskelion.c to dlls/ntdll/Makefile.in (SOURCES)
//   2. Add triskelion_try_bypass() call in server_call_unlocked()
//   3. Set WINE_TRISKELION=1 to enable

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
    uint32_t sender_tid;
    uint32_t _pad;
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
    struct triskelion_ring sent;
    _Atomic uint32_t wake_bits;
    _Atomic uint32_t changed_bits;
    _Atomic uint32_t wake_mask;
    _Atomic uint32_t changed_mask;
    uint32_t thread_id;
    uint8_t _reserved[44];
};

#define TRISKELION_HEADER_SIZE sizeof(struct triskelion_header)
#define TRISKELION_QUEUE_SIZE  sizeof(struct triskelion_queue)

#define QS_POSTMESSAGE_BIT 0x0008
#define QS_SENDMESSAGE_BIT 0x0040

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
    fd = shm_open(shm_name, O_RDWR, 0644);
    if (fd == -1)
    {
        // Remove stale segment from a crashed prior run, then create fresh
        shm_unlink(shm_name);
        fd = shm_open(shm_name, O_CREAT | O_EXCL | O_RDWR, 0644);
        if (fd == -1 && errno == EEXIST)
        {
            // Race: another process created it between unlink and open
            fd = shm_open(shm_name, O_RDWR, 0644);
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
    // glReserved2 is unused in Wine; win32u reads it to check the ring
    // BEFORE check_queue_bits, avoiding the server call skip when our
    // ring has pending messages.
    NtCurrentTeb()->glReserved2 = (PVOID)q;

    // Register in process-local map
    pthread_mutex_lock(&local_map_lock);
    if (local_map_count < MAX_LOCAL_THREADS)
    {
        local_map[local_map_count].tid = tid;
        local_map[local_map_count].slot = slot;
        local_map_count++;
    }
    else if (local_map_count == MAX_LOCAL_THREADS)
    {
        fprintf(stderr, "[triskelion] local_map full (%d threads), "
                "new threads fall through to wineserver\n", MAX_LOCAL_THREADS);
        local_map_count++; // suppress further warnings
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

// SPSC ring: pop (consumer)
static int ring_pop(struct triskelion_ring *ring, struct triskelion_message *out)
{
    uint64_t rp = atomic_load_explicit(&ring->read_pos.val, memory_order_relaxed);
    uint64_t wp = atomic_load_explicit(&ring->write_pos.val, memory_order_acquire);

    if (rp == wp) return 0;

    *out = ring->buf[rp & TRISKELION_RING_MASK];
    atomic_store_explicit(&ring->read_pos.val, rp + 1, memory_order_release);
    return 1;
}

// SPSC ring: push (producer)
static int ring_push(struct triskelion_ring *ring, const struct triskelion_message *msg)
{
    uint64_t wp = atomic_load_explicit(&ring->write_pos.val, memory_order_relaxed);
    uint64_t rp = atomic_load_explicit(&ring->read_pos.val, memory_order_acquire);

    if ((wp - rp) >= TRISKELION_RING_CAPACITY) return 0;

    ring->buf[wp & TRISKELION_RING_MASK] = *msg;
    atomic_store_explicit(&ring->write_pos.val, wp + 1, memory_order_release);
    return 1;
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
    tmsg.sender_tid = tls_queue ? tls_queue->thread_id : 0;
    tmsg._pad       = 0;

    if (!ring_push(&target->posted, &tmsg))
        return STATUS_NOT_IMPLEMENTED; // ring full, fall through to server

    atomic_fetch_or_explicit(&target->wake_bits, QS_POSTMESSAGE_BIT, memory_order_release);
    atomic_fetch_or_explicit(&target->changed_bits, QS_POSTMESSAGE_BIT, memory_order_release);
    triskelion_futex_wake(&target->wake_bits);

    req->u.reply.reply_header.error = 0;
    req->u.reply.reply_header.reply_size = 0;
    return STATUS_SUCCESS;
}

// Main bypass dispatch. Called from server_call_unlocked before the socket I/O.
// Returns STATUS_NOT_IMPLEMENTED to fall through to the normal path.
unsigned int triskelion_try_bypass(void *req_ptr)
{
    struct __server_request_info *req = req_ptr;
    int code;

    if (!do_triskelion()) return STATUS_NOT_IMPLEMENTED;

    code = req->u.req.request_header.req;

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
