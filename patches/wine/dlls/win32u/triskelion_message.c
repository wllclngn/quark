// triskelion win32u patch -- reference implementation
//
// This file documents the two modifications made to dlls/win32u/message.c.
// It is NOT compiled directly; the patches are applied by install.py or
// manually per APPLY.md.
//
// Target: Valve Wine 10.0 (proton_10.0 branch), dlls/win32u/message.c
//
// Modification A: triskelion_has_posted() function
// ================================================
// Inserted after WINE_DECLARE_DEBUG_CHANNEL(relay);
//
// Reads the SPSC ring buffer positions from the triskelion shared memory
// region via TEB->glReserved2. Returns TRUE if the posted ring has
// unread messages (write_pos > read_pos).

/* triskelion: check if the shm ring has pending posted messages.
 * queue_ptr is from TEB->glReserved2, set by ntdll triskelion_claim_slot.
 * The ring's write_pos (offset 0) and read_pos (offset 64) are cacheline-aligned uint64_t. */
static inline BOOL triskelion_has_posted( volatile void *queue_ptr )
{
    volatile ULONGLONG *wp, *rp;
    if (!queue_ptr) return FALSE;
    wp = (volatile ULONGLONG *)queue_ptr;
    rp = (volatile ULONGLONG *)((char *)queue_ptr + 64);
    return *wp > *rp;
}

// Modification B: peek_message() condition gate
// ==============================================
// In peek_message(), Proton's check_queue_bits optimization reads
// queue_shm->wake_bits BEFORE calling server_call(get_message). If no
// wake bits match, peek_message returns STATUS_PENDING without reaching
// the server -- and therefore without reaching our GetMessage bypass in
// ntdll's server_call_unlocked.
//
// The fix: prepend triskelion_has_posted() to the check_queue_bits
// condition. If the ring has messages, skip check_queue_bits and force
// the server call path, where ntdll's bypass will pop from the ring.
//
// Original:
//     if (!filter->waited && NtGetTickCount() - thread_info->last_getmsg_time < 3000
//
// Patched:
//     if (!triskelion_has_posted(NtCurrentTeb()->glReserved2) &&
//         !filter->waited && NtGetTickCount() - thread_info->last_getmsg_time < 3000
