Triskelion Wine Patches

Apply to Valve's Wine 10.0 fork (proton_10.0 branch).

AUTOMATED: Run `python3 install.py` to clone Wine source to
~/.local/share/amphetamine/wine-src/ and apply all patches below
automatically. The script is idempotent (safe to run multiple times).

Manual steps are documented below for reference.

ntdll patches (PostMessage/GetMessage bypass)

1. Copy triskelion.c into Wine source tree

   cp patches/wine/dlls/ntdll/unix/triskelion.c <wine-src>/dlls/ntdll/unix/

2. Add to dlls/ntdll/Makefile.in

   In the unix_srcs section, add after unix/thread.c:
     unix/triskelion.c

3. Modify dlls/ntdll/unix/server.c

   In server_call_unlocked(), add the bypass before the FTRACE_BLOCK_START:

   unsigned int server_call_unlocked( void *req_ptr )
   {
       struct __server_request_info * const req = req_ptr;
       unsigned int ret;

   +   /* triskelion: shared memory bypass for hot-path messages */
   +   ret = triskelion_try_bypass( req_ptr );
   +   if (ret != STATUS_NOT_IMPLEMENTED)
   +       return ret;
   +
       FTRACE_BLOCK_START("req %s", req->name)
       ...
   }

4. Declare triskelion_try_bypass in dlls/ntdll/unix/unix_private.h

   After the server_call_unlocked declaration, add:
   extern unsigned int triskelion_try_bypass( void *req_ptr );

win32u patches (check_queue_bits fast-path integration)

5. Add triskelion_has_posted() function to dlls/win32u/message.c

   After the WINE_DECLARE_DEBUG_CHANNEL(relay) line, add:

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

6. Modify peek_message() in dlls/win32u/message.c

   Find the check_queue_bits condition:
       if (!filter->waited && NtGetTickCount() - ...

   Prepend triskelion check so it becomes:
       /* triskelion: if the shm ring has pending posted messages,
        * disable check_queue_bits and force the server call path.
        * The bypass in ntdll server_call_unlocked will pop from the ring. */
       if (!triskelion_has_posted(NtCurrentTeb()->glReserved2) &&
           !filter->waited && NtGetTickCount() - ...

   Without this, Proton's check_queue_bits reads queue_shm->wake_bits
   and returns STATUS_PENDING before the GetMessage bypass can fire.

7. Enable at runtime

   Set WINE_TRISKELION=1 in the proton launcher environment.

8. Build Wine normally

   make dlls/ntdll/unix/triskelion.o && make dlls/ntdll/ntdll.so
   make dlls/win32u/message.o && make dlls/win32u/win32u.so

   Or full rebuild: make -j$(nproc)

   The bypass is gated by the environment variable. Without WINE_TRISKELION=1,
   no shared memory is created and all requests go through the server socket
   as usual. Zero overhead when disabled.
