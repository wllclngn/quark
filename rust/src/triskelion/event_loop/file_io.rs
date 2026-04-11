// File I/O, mappings, and directory cache

use super::*;
#[allow(unused_variables)]


impl EventLoop {
    /// Poll all pending async pipe reads. When data is available, read it,
    /// store the result for get_async_result, and signal the wait handle
    /// so the client's inproc ntsync wait completes.
    pub(crate) fn check_pending_pipe_reads(&mut self) {
        if self.pending_pipe_reads.is_empty() { return; }

        let mut completed = Vec::new();
        for (i, pr) in self.pending_pipe_reads.iter().enumerate() {
            let mut pfd = libc::pollfd { fd: pr.pipe_fd, events: libc::POLLIN, revents: 0 };
            let ret = unsafe { libc::poll(&mut pfd, 1, 0) }; // non-blocking poll
            if ret > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0 {
                completed.push(i);
            }
        }

        // Process completed reads in reverse order (so indices stay valid)
        for &i in completed.iter().rev() {
            let pr = self.pending_pipe_reads.remove(i);
            let mut read_buf = vec![0u8; pr.max_bytes];
            let n = unsafe {
                libc::recv(pr.pipe_fd, read_buf.as_mut_ptr() as *mut libc::c_void,
                           pr.max_bytes, libc::MSG_DONTWAIT)
            };

            if n > 0 {
                read_buf.truncate(n as usize);
                // Store result for get_async_result
                // Signal the wait handle so client's ntsync wait completes
                if let Some((obj, _)) = self.ntsync_objects.get(&(pr.pid, pr.wait_handle)) {
                    let _ = obj.event_set();
                }

                // Store completed read data for get_async_result retrieval
                // Key by (pid, client_fd) so the handler can find it
                self.completed_pipe_reads.push(CompletedPipeRead {
                    client_fd: pr.client_fd,
                    data: read_buf,
                    pid: pr.pid,
                });
            } else if n == 0 {
                // EOF — signal with PIPE_BROKEN
                if let Some((obj, _)) = self.ntsync_objects.get(&(pr.pid, pr.wait_handle)) {
                    let _ = obj.event_set();
                }
                self.completed_pipe_reads.push(CompletedPipeRead {
                    client_fd: pr.client_fd,
                    data: Vec::new(), // empty = EOF
                    pid: pr.pid,
                });
            }
            // else EAGAIN — shouldn't happen since poll said ready, but ignore
        }
    }

    /// Get or create a reusable ntsync wait handle for pipe I/O completion.
    /// Returns an auto-reset event handle that can be signaled on successful read/write.
    /// Cached per (pid, pipe_handle) so it's reused across operations on the same pipe.
    pub(super) fn get_pipe_wait_handle(&mut self, pid: u32, pipe_handle: u32) -> u32 {
        if let Some(&wh) = self.side_tables.io_wait_handles.get(&(pid, pipe_handle)) {
            return wh;
        }
        if let Some(evt) = self.get_or_create_event(false, false) {
            let oid = self.state.alloc_object_id();
            if let Some(process) = self.state.processes.get_mut(&pid) {
                let entry = crate::objects::HandleEntry::with_fd(
                    oid, -1, crate::objects::FD_TYPE_FILE, 0x001F0003, 0x20,
                );
                let h = process.handles.allocate_full(entry);
                if h != 0 {
                    self.insert_recyclable_event(pid, h, evt, 1); // INTERNAL
                    self.side_tables.io_wait_handles.insert((pid, pipe_handle), h);
                    return h;
                }
            }
        }
        0
    }

    /// Signal a pipe wait handle so ntdll's wait_async returns immediately.
    pub(super) fn signal_pipe_wait(&self, pid: u32, wait_handle: u32) {
        if let Some((obj, _)) = self.ntsync_objects.get(&(pid, wait_handle)) {
            let _ = obj.event_set();
        }
    }
}


// Create a memfd for the Windows User Shared Data (USD) section.
// KUSER_SHARED_DATA: 4KB page containing OS version, machine type, etc.
// Wine maps this at a fixed address and reads various fields during init.
// Wine pe_image_info: metadata for SEC_IMAGE mappings
// Must match Wine 11.4's struct pe_image_info exactly (server_protocol.h)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct PeImageInfo {
    base: u64,              // client_ptr_t (preferred load address)
    map_addr: u64,          // client_ptr_t
    stack_size: u64,        // mem_size_t
    stack_commit: u64,      // mem_size_t
    entry_point: u32,
    map_size: u32,          // SizeOfImage
    alignment: u32,         // Wine 11.4+ (SectionAlignment from PE header)
    zerobits: u32,
    subsystem: u32,
    subsystem_minor: u16,
    subsystem_major: u16,
    osversion_major: u16,
    osversion_minor: u16,
    image_charact: u16,
    dll_charact: u16,
    machine: u16,
    image_flags_byte: u8,   // contains_code:1, wine_builtin:1, wine_fakedll:1, is_hybrid:1, pad:4
    image_flags: u8,
    loader_flags: u32,
    header_size: u32,
    header_map_size: u32,   // Wine 11.4+ (aligned header size for mapping)
    file_size: u32,
    checksum: u32,
    dbg_offset: u32,
    dbg_size: u32,
}

const _: () = assert!(std::mem::size_of::<PeImageInfo>() == 96);


/// Resolve an fd to a UTF-16LE encoded NT path for Wine's module loader.
/// Wine's find_builtin_dll extracts the basename (scanning for '/' or '\').
/// But Wine's PE loader uses the FULL nt_name for FullDllName, which
/// GetModuleFileNameW returns. PhysFS calls CreateFileW on this path —
/// it MUST be a valid NT path (\??\Z:\...), not a raw Unix path.
fn fd_to_nt_name(fd: RawFd) -> Option<Vec<u8>> {
    let link = format!("/proc/self/fd/{}", fd);
    let path = std::fs::read_link(&link).ok()?;
    let path_str = path.to_str()?;

    // Convert Unix path to NT path via Z: drive (Z: maps to /)
    let nt_path = if path_str.starts_with('/') {
        format!("\\??\\Z:{}", path_str.replace('/', "\\"))
    } else {
        path_str.to_string()
    };

    // Encode as UTF-16LE (Wine UNICODE_STRING format)
    let utf16: Vec<u8> = nt_path.encode_utf16()
        .flat_map(|c| c.to_le_bytes())
        .collect();

    if utf16.is_empty() { return None; }

    Some(utf16)
}


// Create a memfd backing shared writable PE sections.
// Stock wineserver (mapping.c:635-695) does this so ntdll can mmap shared sections
// from a server-provided fd. Without it, PE images with IMAGE_SCN_MEM_SHARED fail.
fn create_shared_section_fd(pe_fd: RawFd) -> Option<RawFd> {
    let mut buf = [0u8; 4096];
    let n = unsafe { libc::pread(pe_fd, buf.as_mut_ptr() as *mut _, buf.len(), 0) };
    if n < 64 { return None; }
    let n = n as usize;

    if &buf[0..2] != b"MZ" { return None; }
    let e_lfanew = u32::from_le_bytes([buf[0x3C], buf[0x3D], buf[0x3E], buf[0x3F]]) as usize;
    if e_lfanew + 24 > n { return None; }
    if &buf[e_lfanew..e_lfanew+4] != b"PE\0\0" { return None; }

    let num_sections = u16::from_le_bytes([buf[e_lfanew+6], buf[e_lfanew+7]]) as usize;
    let opt_size = u16::from_le_bytes([buf[e_lfanew+20], buf[e_lfanew+21]]) as usize;
    let opt = e_lfanew + 24;
    let magic = u16::from_le_bytes([buf[opt], buf[opt+1]]);
    let section_align = if magic == 0x20b {
        u32::from_le_bytes([buf[opt+32], buf[opt+33], buf[opt+34], buf[opt+35]])
    } else {
        u32::from_le_bytes([buf[opt+32], buf[opt+33], buf[opt+34], buf[opt+35]])
    };
    let align_mask = if section_align > 0 { section_align - 1 } else { 0 };

    let sec_start = opt + opt_size;
    const IMAGE_SCN_MEM_SHARED: u32 = 0x10000000;
    const IMAGE_SCN_MEM_WRITE: u32 = 0x80000000;

    // First pass: compute total size of shared writable sections
    let mut total_size: u64 = 0;
    let mut shared_sections: Vec<(u64, u32, u32)> = Vec::new(); // (map_offset, raw_ptr, raw_size)
    for i in 0..num_sections {
        let off = sec_start + i * 40;
        if off + 40 > n { break; }
        let vsize = u32::from_le_bytes([buf[off+8], buf[off+9], buf[off+10], buf[off+11]]);
        let raw_size = u32::from_le_bytes([buf[off+16], buf[off+17], buf[off+18], buf[off+19]]);
        let raw_ptr = u32::from_le_bytes([buf[off+20], buf[off+21], buf[off+22], buf[off+23]]);
        let chars = u32::from_le_bytes([buf[off+36], buf[off+37], buf[off+38], buf[off+39]]);
        if (chars & IMAGE_SCN_MEM_SHARED != 0) && (chars & IMAGE_SCN_MEM_WRITE != 0) {
            let map_size = ((vsize as u64) + align_mask as u64) & !(align_mask as u64);
            let file_size = raw_size.min(map_size as u32);
            shared_sections.push((total_size, raw_ptr, file_size));
            total_size += map_size;
        }
    }
    if total_size == 0 { return None; }

    // Create memfd and copy section data
    let memfd = unsafe {
        libc::memfd_create(b"wine_shared\0".as_ptr() as *const libc::c_char, 0)
    };
    if memfd < 0 { return None; }
    unsafe { libc::ftruncate(memfd, total_size as libc::off_t); }

    let mut copy_buf = vec![0u8; 65536];
    for &(write_offset, raw_ptr, file_size) in &shared_sections {
        if raw_ptr == 0 || file_size == 0 { continue; }
        let mut remaining = file_size as usize;
        let mut read_pos = raw_ptr as i64;
        let mut write_pos = write_offset as i64;
        while remaining > 0 {
            let chunk = remaining.min(copy_buf.len());
            let rd = unsafe {
                libc::pread(pe_fd, copy_buf.as_mut_ptr() as *mut _, chunk, read_pos)
            };
            if rd <= 0 { break; }
            let rd = rd as usize;
            unsafe {
                libc::pwrite(memfd, copy_buf.as_ptr() as *const _, rd, write_pos);
            }
            remaining -= rd;
            read_pos += rd as i64;
            write_pos += rd as i64;
        }
    }

    Some(memfd)
}

// Read PE headers from an fd and build PeImageInfo. Returns None on failure.
fn read_pe_image_info(fd: RawFd) -> Option<(PeImageInfo, u64)> {
    let mut buf = [0u8; 4096];
    let n = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut _, buf.len(), 0) };
    if n < 64 { return None; }
    let n = n as usize;

    // DOS header check
    if &buf[0..2] != b"MZ" { return None; }

    // e_lfanew at offset 0x3C
    let e_lfanew = u32::from_le_bytes([buf[0x3C], buf[0x3D], buf[0x3E], buf[0x3F]]) as usize;
    if e_lfanew + 4 > n { return None; }
    if &buf[e_lfanew..e_lfanew+4] != b"PE\0\0" { return None; }

    let coff = e_lfanew + 4;
    if coff + 20 > n { return None; }

    // COFF header
    let machine = u16::from_le_bytes([buf[coff], buf[coff+1]]);
    let _num_sections = u16::from_le_bytes([buf[coff+2], buf[coff+3]]);
    let opt_size = u16::from_le_bytes([buf[coff+16], buf[coff+17]]) as usize;
    let charact = u16::from_le_bytes([buf[coff+18], buf[coff+19]]);

    let opt = coff + 20;
    if opt + opt_size > n { return None; }

    let magic = u16::from_le_bytes([buf[opt], buf[opt+1]]);
    let is_pe32plus = magic == 0x20B; // PE32+ (64-bit)

    let r = |off: usize| -> u32 {
        u32::from_le_bytes([buf[opt+off], buf[opt+off+1], buf[opt+off+2], buf[opt+off+3]])
    };
    let r64 = |off: usize| -> u64 {
        u64::from_le_bytes([
            buf[opt+off], buf[opt+off+1], buf[opt+off+2], buf[opt+off+3],
            buf[opt+off+4], buf[opt+off+5], buf[opt+off+6], buf[opt+off+7],
        ])
    };

    let entry_point = r(16);
    let (image_base, section_align, _file_align, size_of_image, header_size,
         stack_size, stack_commit, subsystem, dll_charact, checksum,
         os_major, os_minor, subsys_major, subsys_minor) = if is_pe32plus {
        (r64(24), r(32), r(36), r(56), r(60),
         r64(72), r64(80),
         u16::from_le_bytes([buf[opt+68], buf[opt+69]]),
         u16::from_le_bytes([buf[opt+70], buf[opt+71]]),
         r(64),
         u16::from_le_bytes([buf[opt+44], buf[opt+45]]),
         u16::from_le_bytes([buf[opt+46], buf[opt+47]]),
         u16::from_le_bytes([buf[opt+48], buf[opt+49]]),
         u16::from_le_bytes([buf[opt+50], buf[opt+51]]))
    } else {
        (r(28) as u64, r(32), r(36), r(56), r(60),
         r(72) as u64, r(76) as u64,
         u16::from_le_bytes([buf[opt+68], buf[opt+69]]),
         u16::from_le_bytes([buf[opt+70], buf[opt+71]]),
         r(64),
         u16::from_le_bytes([buf[opt+44], buf[opt+45]]),
         u16::from_le_bytes([buf[opt+46], buf[opt+47]]),
         u16::from_le_bytes([buf[opt+48], buf[opt+49]]),
         u16::from_le_bytes([buf[opt+50], buf[opt+51]]))
    };

    // Check for CLR/.NET COM descriptor directory (index 14).
    // Stock Wine reads the CLR header to set loader_flags and image_flags
    // for ComPlusILOnly/ComPlusNativeReady (mapping.c:1007,1052-1060).
    // ntdll uses these to route through mscoree.dll instead of native entry.
    let num_data_dirs = if is_pe32plus { r(108) } else { r(92) } as usize;
    let data_dir_base = if is_pe32plus { 112 } else { 96 };
    const IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR: usize = 14;
    let (has_clr, clr_flags) = if IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR < num_data_dirs {
        let dd_off = data_dir_base + IMAGE_DIRECTORY_ENTRY_COM_DESCRIPTOR * 8;
        if opt + dd_off + 8 <= n {
            let clr_va = r(dd_off);
            let clr_sz = r(dd_off + 4);
            if clr_va != 0 && clr_sz != 0 {
                // Read CLR header from file. Resolve VA to file offset via section table.
                let num_sections = u16::from_le_bytes([buf[coff+2], buf[coff+3]]) as usize;
                let sec_table = opt + opt_size;
                let mut clr_file_off = 0u32;
                for s in 0..num_sections {
                    let sh = sec_table + s * 40;
                    if sh + 40 > n { break; }
                    let sec_va = u32::from_le_bytes([buf[sh+12], buf[sh+13], buf[sh+14], buf[sh+15]]);
                    let sec_raw_sz = u32::from_le_bytes([buf[sh+16], buf[sh+17], buf[sh+18], buf[sh+19]]);
                    let sec_raw_ptr = u32::from_le_bytes([buf[sh+20], buf[sh+21], buf[sh+22], buf[sh+23]]);
                    if clr_va >= sec_va && clr_va < sec_va + sec_raw_sz {
                        clr_file_off = sec_raw_ptr + (clr_va - sec_va);
                        break;
                    }
                }
                // Read COR20 header Flags field (offset 16 in IMAGE_COR20_HEADER)
                if clr_file_off > 0 {
                    let mut clr_buf = [0u8; 72];
                    let clr_n = unsafe { libc::pread(fd, clr_buf.as_mut_ptr() as *mut _, 72, clr_file_off as i64) };
                    if clr_n >= 20 {
                        let flags = u32::from_le_bytes([clr_buf[16], clr_buf[17], clr_buf[18], clr_buf[19]]);
                        (true, flags)
                    } else { (true, 0) }
                } else { (true, 0) }
            } else { (false, 0) }
        } else { (false, 0) }
    } else { (false, 0) };
    // COMIMAGE_FLAGS_ILONLY = 0x01, COMIMAGE_FLAGS_32BITREQUIRED = 0x02,
    // COMIMAGE_FLAGS_32BITPREFERRED = 0x20000
    let clr_il_only = has_clr && (clr_flags & 0x01) != 0;
    let clr_native_ready = clr_il_only && (clr_flags & 0x02) == 0; // IL-only AND NOT 32bit-required
    let clr_prefer32 = has_clr && (clr_flags & 0x20000) != 0;

    // File size from fstat
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let file_size = if unsafe { libc::fstat(fd, &mut st) } == 0 { st.st_size as u32 } else { 0 };

    let contains_code = 1u8; // assume code present

    // Wine builtin detection: "Wine builtin DLL" at DOS header offset 0x40
    let wine_builtin = if n > 0x50 && &buf[0x40..0x44] == b"Wine" { 1u8 } else { 0u8 };
    log_info!("PE: machine={machine:#x} wine_builtin={wine_builtin} has_clr={has_clr} clr_il_only={clr_il_only} entry=0x{entry_point:x} size={file_size} charact={charact:#x}");
    // Wine fakedll detection: "Wine placeholder DLL" at offset 0x40
    let wine_fakedll = if n > 0x54 && &buf[0x40..0x50] == b"Wine placehold" { 1u8 } else { 0u8 };

    // image_flags_byte: bit 0 = contains_code, bit 1 = wine_builtin, bit 2 = wine_fakedll, bit 3 = is_hybrid
    let flags_byte = contains_code | (wine_builtin << 1) | (wine_fakedll << 2);

    // image_flags: ComPlus bits from CLR header (stock: mapping.c:1052-1060)
    // IMAGE_FLAGS_ComPlusNativeReady=0x01, IMAGE_FLAGS_ComPlusILOnly=0x02,
    // IMAGE_FLAGS_ComPlusPrefer32bit=0x20
    let mut image_flags: u8 = 0;
    if clr_native_ready { image_flags |= 0x01; }
    if clr_il_only { image_flags |= 0x02; }
    if clr_prefer32 { image_flags |= 0x20; }

    // Round header_size up to section alignment for header_map_size
    let header_map_size = if section_align > 0 {
        (header_size + section_align - 1) & !(section_align - 1)
    } else {
        header_size
    };

    let info = PeImageInfo {
        base: image_base,
        map_addr: 0,
        stack_size,
        stack_commit,
        entry_point,
        map_size: size_of_image,
        alignment: section_align,       // Wine 11.4: SectionAlignment from PE header
        zerobits: 0,
        subsystem: subsystem as u32,
        subsystem_minor: subsys_minor,
        subsystem_major: subsys_major,
        osversion_major: os_major,
        osversion_minor: os_minor,
        image_charact: charact,
        dll_charact,
        machine,
        image_flags_byte: flags_byte, // bit 0 = contains_code, bit 1 = wine_builtin, bit 2 = wine_fakedll
        image_flags,
        loader_flags: if has_clr { 1 } else { 0 },
        header_size,
        header_map_size,                // Wine 11.4: aligned header size for mapping
        file_size,
        checksum,
        dbg_offset: 0,
        dbg_size: 0,
    };

    Some((info, size_of_image as u64))
}

impl EventLoop {

    // ── Phase 1: Survive Wine init ─────────────────────────────────────────

    pub(crate) fn handle_open_mapping(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<OpenMappingRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const OpenMappingRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        let name = utf16le_to_string(vararg);
        let name_lower = name.to_lowercase();

        // Extract just the object name (after last backslash)
        let short_name = name_lower.rsplit('\\').next().unwrap_or(&name_lower);

        if let Some(named) = self.state.named_objects.get(short_name) {
            // Return existing named object as a handle
            let oid = named.object_id;
            let src_fd = named.fd;
            let fd = unsafe { libc::dup(src_fd) };
            let pid = self.clients.get(&(client_fd as RawFd))
                .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
            let handle = if let Some(pid) = pid {
                if let Some(process) = self.state.processes.get_mut(&pid) {
                    process.handles.allocate_full(
                        crate::objects::HandleEntry::with_fd(oid, fd, crate::objects::FD_TYPE_FILE, 0x000F001F, 0)
                    )
                } else { 0 }
            } else { 0 };

            let _om_pid = pid.unwrap_or(0);

            let reply = OpenMappingReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                handle,
                _pad_0: [0; 4],
            };
            return reply_fixed(&reply);
        }

        // Legacy fallback: check full name for backwards compat
        if let Some(named) = self.state.named_objects.get(&name_lower) {
            let oid = named.object_id;
            let fd = unsafe { libc::dup(named.fd) };
            let pid = self.clients.get(&(client_fd as RawFd))
                .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
            let handle = if let Some(pid) = pid {
                if let Some(process) = self.state.processes.get_mut(&pid) {
                    process.handles.allocate_full(
                        crate::objects::HandleEntry::with_fd(oid, fd, crate::objects::FD_TYPE_FILE, 0x000F001F, 0)
                    )
                } else { 0 }
            } else { 0 };

            let reply = OpenMappingReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                handle,
                _pad_0: [0; 4],
            };
            return reply_fixed(&reply);
        }

        // Check for USD section — Wine exits(1) if this fails
        if short_name.contains("__wine_user_shared_data") {
            // Create USD memfd on first access
            if let Some((usd_fd, usd_ptr)) = create_usd_memfd() {
                self.usd_map = usd_ptr;
                self.update_usd_time(); // set initial TickCount
                let oid = self.state.alloc_object_id();
                let dup_fd = unsafe { libc::dup(usd_fd) };
                self.state.named_objects.insert("__wine_user_shared_data".to_string(), crate::objects::NamedObjectEntry {
                    object_id: oid, fd: usd_fd,
                });
                self.state.mappings.insert(oid, crate::objects::MappingInfo {
                    fd: usd_fd, size: 0x1000, flags: 0x800000, pe_image_info: None, nt_name: None, shared_fd: None, // SEC_COMMIT
                });

                let pid = self.clients.get(&(client_fd as RawFd))
                    .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
                let handle = if let Some(pid) = pid {
                    if let Some(process) = self.state.processes.get_mut(&pid) {
                        process.handles.allocate_full(
                            crate::objects::HandleEntry::with_fd(oid, dup_fd, crate::objects::FD_TYPE_FILE, 0x000F001F, 0)
                        )
                    } else { 0 }
                } else { 0 };

                let reply = OpenMappingReply {
                    header: ReplyHeader { error: 0, reply_size: 0 },
                    handle,
                    _pad_0: [0; 4],
                };
                return reply_fixed(&reply);
            }
        }

        // Not found
        log_warn!("open_mapping NOT_FOUND: name=\"{name}\" short=\"{short_name}\" fd={client_fd}");
        reply_fixed(&ReplyHeader { error: 0xC0000034, reply_size: 0 }) // STATUS_OBJECT_NAME_NOT_FOUND
    }


    pub(crate) fn handle_alloc_file_handle(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<AllocFileHandleRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const AllocFileHandleRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Take inflight fd matching req.fd (from process-wide pool)
        let inflight_fd = self.take_inflight_fd(client_fd as RawFd, req.fd);

        if let Some(fd) = inflight_fd {
            let oid = self.state.alloc_object_id();
            let pid = self.clients.get(&(client_fd as RawFd))
                .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });
            let mapped_access = Self::map_file_access(req.access);
            log_info!("alloc_file_handle: fd={client_fd} unix_fd={fd} req_fd={} pid={:?} access={:#x}→{:#x}", req.fd, pid, req.access, mapped_access);
            let handle = if let Some(pid) = pid {
                if let Some(process) = self.state.processes.get_mut(&pid) {
                    process.handles.allocate_full(
                        crate::objects::HandleEntry::with_fd(oid, fd, crate::objects::FD_TYPE_FILE, Self::map_file_access(req.access), 0)
                    )
                } else { 0 }
            } else { 0 };

            if handle == 0 {
                log_error!("alloc_file_handle: handle=0! fd={client_fd} unix_fd={fd}");
                return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 });
            }
            let reply = AllocFileHandleReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                handle,
                _pad_0: [0; 4],
            };
            reply_fixed(&reply)
        } else {
            reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }) // STATUS_INVALID_HANDLE
        }
    }


    pub(crate) fn handle_create_file(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<CreateFileRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const CreateFileRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // VARARG contains object_attributes (rootdir + attrs + sd + name) then filename
        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        let (_rootdir, _objattr_name) = crate::registry::parse_objattr_name(vararg);

        // Extract the actual file path from after the object attributes
        // objattr header (16 bytes) + aligned sd + aligned name, then rounded to 4-byte boundary
        let filename_start = if vararg.len() >= 16 {
            let sd_len = u32::from_le_bytes([vararg[8], vararg[9], vararg[10], vararg[11]]) as usize;
            let name_len = u32::from_le_bytes([vararg[12], vararg[13], vararg[14], vararg[15]]) as usize;
            // Wine formula: (sizeof(objattr) + (sd_len & ~1) + (name_len & ~1) + 3) & ~3
            (16 + (sd_len & !1) + (name_len & !1) + 3) & !3
        } else {
            0
        };
        let filename_bytes = if filename_start < vararg.len() {
            &vararg[filename_start..]
        } else {
            &[] as &[u8]
        };

        // Convert UTF-8 filename to C string and open
        // Skip leading NUL bytes (padding from VARARG alignment)
        let filename_bytes = &filename_bytes[filename_bytes.iter().position(|&b| b != 0).unwrap_or(filename_bytes.len())..];
        let filename = std::str::from_utf8(filename_bytes).unwrap_or("");
        let filename = filename.trim_end_matches('\0');


        if filename.is_empty() {
            // The Unix filename is empty — check the object_attributes name (NT path)
            let (_rootdir, objattr_name_bytes) = crate::registry::parse_objattr_name(vararg);
            let objattr_name = utf16le_to_string(objattr_name_bytes);

            // Check if this is a named pipe open — match basename against registered pipes
            let objattr_lower = objattr_name.to_lowercase();
            let basename = objattr_lower.rsplit('\\').next().unwrap_or("");
            if !basename.is_empty() {
                if let Some(reply) = self.try_connect_named_pipe(client_fd, basename, req.access, req.options) {
                    return reply;
                }
            }

            return reply_fixed(&ReplyHeader { error: 0xC0000034, reply_size: 0 }); // NOT_FOUND
        }

        // Disposition: 0=SUPERSEDE, 1=OPEN, 2=CREATE, 3=OPEN_IF, 4=OVERWRITE, 5=OVERWRITE_IF
        let is_directory = req.options & 0x1 != 0; // FILE_DIRECTORY_FILE
        let can_create = matches!(req.create, 0 | 2 | 3 | 5); // SUPERSEDE, CREATE, OPEN_IF, OVERWRITE_IF

        let c_path = std::ffi::CString::new(filename).unwrap_or_default();

        // Handle directory operations
        if is_directory {
            let exists = unsafe {
                let mut st: libc::stat = std::mem::zeroed();
                libc::stat(c_path.as_ptr(), &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFDIR
            };

            if !exists && can_create {
                let _ = std::fs::create_dir_all(filename);
            }

            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0) };
            if fd < 0 {
                return reply_fixed(&ReplyHeader { error: 0xC0000034, reply_size: 0 }); // NOT_FOUND
            }
            return self.create_file_handle(client_fd, fd, req.access, req.options);
        }

        // Open the file
        let flags = if req.access & 0x40000000 != 0 { libc::O_RDWR } else { libc::O_RDONLY };
        let flags = flags | libc::O_CLOEXEC;

        let fd = unsafe { libc::open(c_path.as_ptr(), flags, 0) };

        // Diagnostic: log exe file opens with fd details
        if fd >= 0 && filename.contains(".exe") {
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let size = if unsafe { libc::fstat(fd, &mut st) } == 0 { st.st_size } else { -1 };
            let link = format!("/proc/self/fd/{fd}");
            let target = std::fs::read_link(&link).map(|p| p.display().to_string()).unwrap_or_default();
            log_info!("CREATE_FILE_EXE: fd={fd} size={size} type={:#o} path=\"{filename}\" target=\"{target}\"",
                      st.st_mode & libc::S_IFMT);
        }

        if fd < 0 {
            // File doesn't exist — try creating if disposition allows
            if can_create {
                if let Some(parent) = std::path::Path::new(filename).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let create_flags = libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC;
                let fd = unsafe { libc::open(c_path.as_ptr(), create_flags, 0o644) };
                if fd >= 0 {
                    return self.create_file_handle(client_fd, fd, req.access, req.options);
                }
            }
            // Try read-only fallback
            let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC, 0) };
            if fd < 0 {
                return reply_fixed(&ReplyHeader { error: 0xC0000034, reply_size: 0 }); // NOT_FOUND
            }
            return self.create_file_handle(client_fd, fd, req.access, req.options);
        }

        self.create_file_handle(client_fd, fd, req.access, req.options)
    }


    pub(crate) fn handle_create_mapping(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<CreateMappingRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const CreateMappingRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });

        let oid = self.state.alloc_object_id();

        // Create backing: dup file handle's fd, or create memfd
        let mapping_fd = if req.file_handle != 0 {
            // Dup the file's fd
            pid.and_then(|p| self.state.processes.get(&p))
                .and_then(|p| p.handles.get(req.file_handle))
                .and_then(|e| e.fd)
                .map(|fd| unsafe { libc::dup(fd) })
        } else {
            None
        };

        let mapping_fd = mapping_fd.unwrap_or_else(|| {
            // Create anonymous mapping via memfd
            let fd = unsafe {
                libc::memfd_create(b"wine_mapping\0".as_ptr() as *const libc::c_char, 0)
            };
            if fd >= 0 && req.size > 0 {
                unsafe { libc::ftruncate(fd, req.size as i64); }
            }
            fd
        });

        if mapping_fd < 0 {
            return reply_fixed(&ReplyHeader { error: 0xC0000017, reply_size: 0 }); // NO_MEMORY
        }

        // Compute mapping size
        let mut mapping_size = req.size;
        let mapping_flags = req.flags;

        const SEC_IMAGE: u32 = 0x1000000;
        let mut pe_info_bytes: Option<Vec<u8>> = None;

        if mapping_flags & SEC_IMAGE != 0 {
            // PE image section: read full PE header info
            if let Some((info, image_size)) = read_pe_image_info(mapping_fd) {
                mapping_size = image_size;
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        &info as *const PeImageInfo as *const u8,
                        std::mem::size_of::<PeImageInfo>(),
                    )
                };
                pe_info_bytes = Some(bytes.to_vec());
            } else {
                // Not a valid PE — fall back to data mapping (e.g. NLS files with SEC_IMAGE)
                if mapping_size == 0 {
                    let mut st: libc::stat = unsafe { std::mem::zeroed() };
                    if unsafe { libc::fstat(mapping_fd, &mut st) } == 0 {
                        mapping_size = st.st_size as u64;
                    }
                }
            }
        } else if mapping_size == 0 {
            // Get size from file
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(mapping_fd, &mut st) } == 0 {
                mapping_size = st.st_size as u64;
            }
        }

        // For SEC_IMAGE, resolve the file path and encode as UTF-16LE NT name.
        // Wine's find_builtin_dll needs this to locate the matching .so file.
        let nt_name = if mapping_flags & SEC_IMAGE != 0 {
            fd_to_nt_name(mapping_fd)
        } else {
            None
        };

        // For SEC_IMAGE with shared writable sections, create a backing memfd.
        let shared_fd = if mapping_flags & SEC_IMAGE != 0 {
            create_shared_section_fd(mapping_fd)
        } else {
            None
        };

        // Store mapping info
        self.state.mappings.insert(oid, crate::objects::MappingInfo {
            fd: mapping_fd,
            size: mapping_size,
            flags: mapping_flags,
            pe_image_info: pe_info_bytes,
            nt_name,
            shared_fd,
        });

        // Register as named object so open_mapping can find sections by name.
        // Object attributes vararg: rootdir(u32) + attributes(u32) + sd_len(u32) + name_len(u32) + sd + name(UTF-16LE)
        let vararg = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        if vararg.len() >= 16 {
            let sd_len = u32::from_le_bytes(vararg[8..12].try_into().unwrap_or([0;4])) as usize;
            let name_len = u32::from_le_bytes(vararg[12..16].try_into().unwrap_or([0;4])) as usize;
            if name_len > 0 {
                let name_off = 16 + sd_len;
                if name_off + name_len <= vararg.len() {
                    let name_bytes = &vararg[name_off..name_off + name_len];
                    let name: String = name_bytes.chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect::<Vec<u16>>()
                        .iter()
                        .map(|&c| char::from_u32(c as u32).unwrap_or('\0'))
                        .collect();
                    let name_lower = name.to_lowercase();
                    let short = name_lower.rsplit('\\').next().unwrap_or(&name_lower).to_string();
                    let dup_fd = unsafe { libc::dup(mapping_fd) };
                    if dup_fd >= 0 {
                        self.state.named_objects.insert(short, crate::objects::NamedObjectEntry {
                            object_id: oid, fd: dup_fd,
                        });
                    }
                }
            }
        }

        let handle_fd = unsafe { libc::dup(mapping_fd) };
        // Use file_access (not section access) for the handle entry.
        // Wine's server_get_unix_fd checks FILE_READ_DATA against handle access.
        // Section access rights (GENERIC_READ, SECTION_MAP_READ) don't include
        // FILE_READ_DATA, causing STATUS_ACCESS_DENIED on wine_server_handle_to_fd.
        // Stock wineserver maps file_access from page protection in NtCreateSection.
        let fd_access = if req.file_access != 0 { req.file_access } else { req.access };
        let handle = if let Some(pid) = pid {
            if let Some(process) = self.state.processes.get_mut(&pid) {
                process.handles.allocate_full(
                    crate::objects::HandleEntry::with_fd(oid, handle_fd, crate::objects::FD_TYPE_FILE, fd_access, 0)
                )
            } else { 0 }
        } else { 0 };

        if handle == 0 {
            log_error!("create_mapping: handle=0! fd={client_fd} pid={:?}", pid);
            unsafe { libc::close(handle_fd); }
            return reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 });
        }

        let reply = CreateMappingReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle,
            _pad_0: [0; 4],
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_mapping_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetMappingInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetMappingInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None });

        let oid = pid.and_then(|p| self.state.processes.get(&p))
            .and_then(|p| p.handles.get(req.handle))
            .map(|e| e.object_id);

        let max_vararg = max_reply_vararg(buf);

        // Extract mapping data (immutable borrow of self.state.mappings)
        let mapping_data = oid.and_then(|id| self.state.mappings.get(&id)).map(|mapping| {
            let pe_info = mapping.pe_image_info.clone();
            let name = mapping.nt_name.clone();
            let size = mapping.size;
            let flags = mapping.flags;
            let sfd = mapping.shared_fd;
            (pe_info, name, size, flags, sfd)
        });

        if let Some((pe_info, nt_name, size, flags, shared_fd_opt)) = mapping_data {
            // DIAG: dump every get_mapping_info call so we can correlate triskelion's
            // reply with Wine's file lookup behavior. Decodes the NT name as a UTF-16LE
            // suffix (best effort) so file paths are readable in the log.
            let name_preview: String = nt_name.as_deref()
                .map(|b| {
                    let chars: Vec<u16> = b.chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    String::from_utf16_lossy(&chars)
                })
                .unwrap_or_else(|| "<no name>".into());
            log_info!(
                "get_mapping_info: handle={:#x} size={size:#x} flags={flags:#x} pe_info={} name_len={} name=\"{}\"",
                req.handle,
                pe_info.as_ref().map(|p| p.len()).unwrap_or(0),
                nt_name.as_deref().map(|n| n.len()).unwrap_or(0),
                if name_preview.len() > 80 { &name_preview[name_preview.len()-80..] } else { &name_preview }
            );

            // Allocate handle for shared file if present.
            // The client calls server_get_unix_fd(shared_file) separately — that
            // goes through get_handle_fd which sends the fd via pending_fd at the
            // right time. We just allocate the handle with the fd attached.
            let shared_file = if let Some(sfd) = shared_fd_opt {
                // Use F_DUPFD_CLOEXEC with min=256 to avoid fd collisions with
                // inherited handle fds that live in the low range.
                let dup = unsafe { libc::fcntl(sfd, libc::F_DUPFD_CLOEXEC, 256) };
                if dup >= 0 {
                    let h = if let Some(process) = pid.and_then(|p| self.state.processes.get_mut(&p)) {
                        process.handles.allocate_full(
                            crate::objects::HandleEntry::with_fd(0, dup, crate::objects::FD_TYPE_FILE,
                                0x12019F /* FILE_GENERIC_READ|FILE_GENERIC_WRITE */, 0)
                        )
                    } else { 0 };
                    if h == 0 { unsafe { libc::close(dup); } }
                    log_info!("get_mapping_info: shared_file handle={h:#x} dup_fd={dup} canonical_sfd={sfd} pid={pid:?}");
                    h
                } else { 0 }
            } else { 0 };

            if let Some(ref pe_info) = pe_info {
                let name_bytes = nt_name.as_deref().unwrap_or(&[]);
                let total_needed = pe_info.len() + name_bytes.len();
                let total_available = total_needed.min(max_vararg as usize);

                let mut vararg = Vec::with_capacity(total_available);
                let pe_len = pe_info.len().min(total_available);
                vararg.extend_from_slice(&pe_info[..pe_len]);
                let name_len = if total_available > pe_len {
                    let n = name_bytes.len().min(total_available - pe_len);
                    vararg.extend_from_slice(&name_bytes[..n]);
                    n
                } else {
                    0
                };

                let reply = GetMappingInfoReply {
                    header: ReplyHeader { error: 0, reply_size: vararg.len() as u32 },
                    size,
                    flags,
                    shared_file,
                    name_len: name_len as u32,
                    ver_len: 0, // no version resource shipped
                    total: total_needed as u32,
                    _pad_0: [0; 4],
                };
                log_info!(
                    "  reply: vararg={} pe={} name={} total={} max_avail={} shared_file={:#x}",
                    vararg.len(), pe_len, name_len, total_needed, max_vararg, shared_file
                );
                return reply_vararg(&reply, &vararg);
            }
            let reply = GetMappingInfoReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                size,
                flags,
                shared_file,
                name_len: 0,
                ver_len: 0,
                total: 0,
                _pad_0: [0; 4],
            };
            return reply_fixed(&reply);
        }

        // Fallback: return empty mapping info (size from fstat if possible).
        // This path means the handle has no MappingInfo registered — Wine
        // is asking about something we never went through create_mapping for.
        let fd = pid.and_then(|p| self.state.processes.get(&p))
            .and_then(|p| p.handles.get(req.handle))
            .and_then(|e| e.fd);

        if let Some(fd) = fd {
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let ret = unsafe { libc::fstat(fd, &mut st) };
            let size = if ret == 0 { st.st_size as u64 } else { 0x1000 };
            log_info!(
                "get_mapping_info: handle={:#x} FALLBACK (no MappingInfo) fd={fd} size={size:#x}",
                req.handle
            );
            let reply = GetMappingInfoReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                size,
                flags: 0x800000, // SEC_COMMIT
                shared_file: 0,
                name_len: 0,
                ver_len: 0,
                total: 0,
                _pad_0: [0; 4],
            };
            return reply_fixed(&reply);
        }

        reply_fixed(&ReplyHeader { error: 0xC0000008, reply_size: 0 }) // INVALID_HANDLE
    }


    pub(crate) fn handle_map_image_view(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<MapImageViewRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const MapImageViewRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Track the base address for get_image_map_address
        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .unwrap_or(0);
        let oid = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.mapping))
            .map(|e| e.object_id)
            .unwrap_or(0);

        if oid != 0 {
            self.state.image_views.insert((pid, oid), req.base);

            // If this is the first image view for this process, store its pe_image_info
            // as the main executable's image info (used by GetProcessInfo/ProcessImageInformation)
            if let Some(process) = self.state.processes.get_mut(&pid) {
                if process.exe_image_info.is_none() {
                    if let Some(mapping) = self.state.mappings.get(&oid) {
                        if let Some(ref pe_info) = mapping.pe_image_info {
                            process.exe_image_info = Some(pe_info.clone());
                        }
                    }
                }
            }
        }

        let reply = MapImageViewReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_image_map_address(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetImageMapAddressRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetImageMapAddressRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.clients.get(&(client_fd as RawFd))
            .and_then(|c| if c.process_id != 0 { Some(c.process_id) } else { None })
            .unwrap_or(0);
        let oid = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.handle))
            .map(|e| e.object_id)
            .unwrap_or(0);

        let addr = self.state.image_views.get(&(pid, oid)).copied().unwrap_or(0);

        let reply = GetImageMapAddressReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            addr,
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_map_view(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<MapViewRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const MapViewRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Store view info tracked by the client. Server just acks.
        let reply = MapViewReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_unmap_view(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let _req = if buf.len() >= std::mem::size_of::<UnmapViewRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const UnmapViewRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        // Server doesn't actually unmap — client does. Just ack.
        let reply = UnmapViewReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
        };
        reply_fixed(&reply)
    }


    pub(crate) fn handle_get_mapping_committed_range(&mut self, _client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetMappingCommittedRangeRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetMappingCommittedRangeRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        // Return "everything is committed" from the requested offset.
        // Stock Wine tracks per-page commit state for SEC_RESERVE mappings,
        // but game images (SEC_IMAGE, SEC_COMMIT) are always fully committed.
        // Mono's GC depends on this to know which pages are safe to access.
        reply_fixed(&GetMappingCommittedRangeReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            size: 0x7FFF_FFFF_FFFF_u64.saturating_sub(req.offset), // rest of address space
            committed: 1,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_add_mapping_committed_range(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        // Client tells us pages were committed. We don't track per-page state,
        // so just ack. get_mapping_committed_range returns "all committed" anyway.
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    // File I/O
    pub(crate) fn handle_read(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<ReadRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const ReadRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // async field layout: handle(u32@0) event(u32@4) iosb(u64@8) user(u64@16) ...
        let async_handle = u32::from_le_bytes([req.r#async[0], req.r#async[1], req.r#async[2], req.r#async[3]]);
        let _async_event = u32::from_le_bytes([req.r#async[4], req.r#async[5], req.r#async[6], req.r#async[7]]);
        let user_arg = u64::from_le_bytes([
            req.r#async[16], req.r#async[17], req.r#async[18], req.r#async[19],
            req.r#async[20], req.r#async[21], req.r#async[22], req.r#async[23],
        ]);

        let pid = self.client_pid(client_fd as RawFd);
        let entry_info = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(async_handle))
            .map(|e| (e.fd, e.obj_type, e.options));

        let (fd, obj_type, options) = match entry_info {
            Some((Some(fd), obj_type, options)) => (fd, obj_type, options),
            _ => {
                return reply_fixed(&ReadReply {
                    header: ReplyHeader { error: 0xC0000008, reply_size: 0 },
                    wait: 0, options: 0,
                });
            }
        };

        let mut max_size = max_reply_vararg(buf) as usize;
        if max_size == 0 { max_size = 65536; }

        let mut read_buf = vec![0u8; max_size];
        let n = if obj_type == crate::objects::FD_TYPE_PIPE || obj_type == crate::objects::FD_TYPE_DEVICE || obj_type == crate::objects::FD_TYPE_SOCKET {
            unsafe { libc::recv(fd, read_buf.as_mut_ptr() as *mut libc::c_void, max_size, libc::MSG_DONTWAIT) }
        } else {
            unsafe { libc::read(fd, read_buf.as_mut_ptr() as *mut libc::c_void, max_size) }
        };

        if n > 0 {
            read_buf.truncate(n as usize);
            let wait = if obj_type == crate::objects::FD_TYPE_PIPE || obj_type == crate::objects::FD_TYPE_DEVICE {
                let wh = self.get_pipe_wait_handle(pid, async_handle);
                if wh != 0 { self.signal_pipe_wait(pid, wh); }
                wh
            } else { 0 };
            reply_vararg(&ReadReply {
                header: ReplyHeader { error: 0, reply_size: n as u32 },
                wait, options,
            }, &read_buf)
        } else if n == 0 {
            // EOF / pipe closed
            reply_fixed(&ReadReply {
                header: ReplyHeader { error: 0xC000014B, reply_size: 0 }, // STATUS_PIPE_BROKEN
                wait: 0, options,
            })
        } else {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
                // No data yet — store as pending for get_async_result retry
                self.pending_reads.insert((pid, user_arg), PendingRead {
                    fd, max_bytes: max_size,
                });

                // Register async pipe read. Completed by check_pending_pipe_reads
                // which is called by handle_write (when data arrives on the other end)
                // and by the broker's timeout loop (catch-all).
                // This replaces BlockingPipeRead which blocked worker threads permanently.
                if obj_type == crate::objects::FD_TYPE_PIPE || obj_type == crate::objects::FD_TYPE_DEVICE || obj_type == crate::objects::FD_TYPE_SOCKET {
                    let wait = self.get_pipe_wait_handle(pid, async_handle);
                    if wait != 0 {
                        self.pending_pipe_reads.push(AsyncPipeRead {
                            pipe_fd: fd,
                            max_bytes: max_size,
                            pid,
                            wait_handle: wait,
                            client_fd: client_fd as RawFd,
                        });
                        return reply_fixed(&ReadReply {
                            header: ReplyHeader { error: 0x00000103, reply_size: 0 }, // STATUS_PENDING
                            wait, options,
                        });
                    }
                }

                reply_fixed(&ReadReply {
                    header: ReplyHeader { error: 0x00000103, reply_size: 0 }, // STATUS_PENDING
                    wait: 0, options,
                })
            } else {
                reply_fixed(&ReadReply {
                    header: ReplyHeader { error: 0xC0000185, reply_size: 0 }, // STATUS_IO_DEVICE_ERROR
                    wait: 0, options,
                })
            }
        }
    }

    pub(crate) fn handle_write(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<WriteRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const WriteRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let async_handle = u32::from_le_bytes([req.r#async[0], req.r#async[1], req.r#async[2], req.r#async[3]]);

        let pid = self.client_pid(client_fd as RawFd);
        let entry_info = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(async_handle))
            .map(|e| (e.fd, e.obj_type, e.options));

        let (fd, obj_type, options) = match entry_info {
            Some((Some(fd), obj_type, options)) => (fd, obj_type, options),
            _ => {
                return reply_fixed(&WriteReply {
                    header: ReplyHeader { error: 0xC0000008, reply_size: 0 },
                    wait: 0, options: 0, size: 0, _pad_0: [0; 4],
                });
            }
        };

        // VARARG data follows the fixed 64-byte request struct
        let data = if buf.len() > VARARG_OFF { &buf[VARARG_OFF..] } else { &[] as &[u8] };
        if data.is_empty() {
            return reply_fixed(&WriteReply {
                header: ReplyHeader { error: 0, reply_size: 0 },
                wait: 0, options, size: 0, _pad_0: [0; 4],
            });
        }

        // Write all bytes. Named pipes (socketpairs) may need retries if non-blocking
        // or if the kernel does a short write. rpcrt4 asserts io_status.Information == count
        // so we must write everything or return a real error.
        let mut total_written: usize = 0;
        while total_written < data.len() {
            let n = unsafe {
                libc::write(fd, data[total_written..].as_ptr() as *const libc::c_void,
                            data.len() - total_written)
            };
            if n > 0 {
                total_written += n as usize;
            } else if n == 0 {
                break;
            } else {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
                    // Pipe buffer full — brief yield then retry
                    std::thread::yield_now();
                    continue;
                }
                // Real error — return it
                return reply_fixed(&WriteReply {
                    header: ReplyHeader { error: 0xC0000185, reply_size: 0 }, // STATUS_IO_DEVICE_ERROR
                    wait: 0, options, size: total_written as u32, _pad_0: [0; 4],
                });
            }
        }
        let written = total_written as u32;

        let wait = if obj_type == crate::objects::FD_TYPE_PIPE || obj_type == crate::objects::FD_TYPE_DEVICE {
            let wh = self.get_pipe_wait_handle(pid, async_handle);
            if wh != 0 { self.signal_pipe_wait(pid, wh); }
            wh
        } else { 0 };

        // After writing to a pipe, check if there's a pending async read
        // on the OTHER end that can now be completed. This matches stock
        // wineserver's reselect_read_queue pattern — instant completion
        // when data arrives, no polling delay.
        if total_written > 0 && (obj_type == crate::objects::FD_TYPE_PIPE || obj_type == crate::objects::FD_TYPE_DEVICE || obj_type == crate::objects::FD_TYPE_SOCKET) {
            self.check_pending_pipe_reads();
        }

        reply_fixed(&WriteReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            wait, options, size: written, _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_flush(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&FlushReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            event: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_lock_file(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&LockFileReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            handle: 0,
            overlapped: 0,
        })
    }

    pub(crate) fn handle_unlock_file(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_get_file_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<GetFileInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const GetFileInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        let pid = self.client_pid(client_fd as RawFd);
        let handle = req.handle;
        let info_class = req.info_class;
        let max_reply = max_reply_vararg(buf) as usize;

        log_info!("GET_FILE_INFO: handle={handle:#x} class={info_class} pid={pid} fd={client_fd}");

        // Look up the handle's access and options
        let entry_info = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(handle))
            .map(|e| (e.fd, e.access, e.options));


        match info_class {
            // FileStandardInformation = 5
            // PhysFS (LÖVE) uses this to get file size for fused-exe ZIP detection.
            // Layout: AllocationSize(i64) EndOfFile(i64) NumberOfLinks(u32)
            //         DeletePending(u8) Directory(u8) padding(2)
            5 => {
                let fd = entry_info.and_then(|(f, _, _)| f);
                let mut data = [0u8; 24];
                if let Some(fd) = fd {
                    let mut st: libc::stat = unsafe { std::mem::zeroed() };
                    if unsafe { libc::fstat(fd, &mut st) } == 0 {
                        let size = st.st_size as i64;
                        let alloc = (st.st_blocks as i64) * 512;
                        let is_dir = (st.st_mode & libc::S_IFMT) == libc::S_IFDIR;
                        data[0..8].copy_from_slice(&alloc.to_le_bytes());
                        data[8..16].copy_from_slice(&size.to_le_bytes());
                        data[16..20].copy_from_slice(&(st.st_nlink as u32).to_le_bytes());
                        data[20] = 0; // DeletePending
                        data[21] = if is_dir { 1 } else { 0 };
                    }
                }
                let send_len = 24.min(max_reply);
                reply_vararg(
                    &GetFileInfoReply { header: ReplyHeader { error: 0, reply_size: send_len as u32 } },
                    &data[..send_len],
                )
            }
            // FileAccessInformation = 8
            8 => {
                let access = entry_info.map(|(_, a, _)| a).unwrap_or(0x001F01FF);
                let data = access.to_le_bytes();
                if max_reply < 4 {
                    return reply_fixed(&ReplyHeader { error: 0xC0000004, reply_size: 0 }); // STATUS_INFO_LENGTH_MISMATCH
                }
                reply_vararg(
                    &GetFileInfoReply { header: ReplyHeader { error: 0, reply_size: 4 } },
                    &data,
                )
            }
            // FilePositionInformation = 14
            14 => {
                let fd = entry_info.and_then(|(f, _, _)| f);
                let pos: i64 = if let Some(fd) = fd {
                    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) as i64 }
                } else { 0 };
                let data = pos.to_le_bytes();
                let send_len = 8.min(max_reply);
                reply_vararg(
                    &GetFileInfoReply { header: ReplyHeader { error: 0, reply_size: send_len as u32 } },
                    &data[..send_len],
                )
            }
            // FileModeInformation = 16
            16 => {
                let options = entry_info.map(|(_, _, o)| o).unwrap_or(0x20);
                // Mask to mode-relevant bits
                let mode = options & (0x2 | 0x4 | 0x8 | 0x10 | 0x20); // WRITE_THROUGH|SEQUENTIAL|NO_INTERMEDIATE|SYNC_ALERT|SYNC_NONALERT
                let data = mode.to_le_bytes();
                if max_reply < 4 {
                    return reply_fixed(&ReplyHeader { error: 0xC0000004, reply_size: 0 });
                }
                reply_vararg(
                    &GetFileInfoReply { header: ReplyHeader { error: 0, reply_size: 4 } },
                    &data,
                )
            }
            // FileIoCompletionNotificationInformation = 41
            41 => {
                let data = 0u32.to_le_bytes(); // comp_flags = 0
                if max_reply < 4 {
                    return reply_fixed(&ReplyHeader { error: 0xC0000004, reply_size: 0 });
                }
                reply_vararg(
                    &GetFileInfoReply { header: ReplyHeader { error: 0, reply_size: 4 } },
                    &data,
                )
            }
            // WineFileUnixNameInformation = 1000
            1000 => {
                let fd = entry_info.and_then(|(f, _, _)| f);
                if let Some(fd) = fd {
                    let link = format!("/proc/self/fd/{fd}");
                    if let Ok(path) = std::fs::read_link(&link) {
                        if let Some(path_str) = path.to_str() {
                            let path_bytes = path_str.as_bytes();
                            // Wine struct: u32 Length + char Name[]
                            let mut data = Vec::with_capacity(4 + path_bytes.len());
                            data.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
                            data.extend_from_slice(path_bytes);
                            let send_len = data.len().min(max_reply);
                            return reply_vararg(
                                &GetFileInfoReply { header: ReplyHeader { error: 0, reply_size: send_len as u32 } },
                                &data[..send_len],
                            );
                        }
                    }
                }
                reply_fixed(&ReplyHeader { error: 0xC0000024, reply_size: 0 }) // STATUS_OBJECT_TYPE_MISMATCH
            }
            _ => {
                reply_fixed(&ReplyHeader { error: 0xC0000002, reply_size: 0 }) // STATUS_NOT_IMPLEMENTED
            }
        }
    }

    pub(crate) fn handle_get_volume_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&GetVolumeInfoReply {
            header: ReplyHeader { error: 0, reply_size: 0 },
            wait: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_open_file_object(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<OpenFileObjectRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const OpenFileObjectRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };

        // Extract filename from VARARG (unicode_str at offset 64)
        if buf.len() > VARARG_OFF {
            let name = utf16le_to_string(&buf[VARARG_OFF..]);
            let basename = name.rsplit('\\').next().unwrap_or("").to_lowercase();
            if !basename.is_empty() {
                if let Some(reply) = self.try_connect_named_pipe(client_fd, &basename, req.access, req.options) {
                    return reply;
                }
            } else if name.to_lowercase().contains("pipe") {
                // Opening pipe root directory: \\??\pipe\ (basename is empty)
                // Returns a handle that supports FSCTL_PIPE_WAIT.
                // Must have a real fd — Wine's ntdll calls get_handle_fd on it
                // for NtFsControlFile(FSCTL_PIPE_WAIT). Without a valid fd,
                // explorer.exe crashes during startup.
                let oid = self.state.alloc_object_id();
                let pid = self.client_pid(client_fd as RawFd);
                let pipe_root_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
                let handle = if let Some(process) = self.state.processes.get_mut(&pid) {
                    process.handles.allocate_full(
                        crate::objects::HandleEntry::with_fd(oid, pipe_root_fd, crate::objects::FD_TYPE_FILE, req.access, req.options)
                    )
                } else { 0 };
                if handle != 0 {
                    return reply_fixed(&OpenFileObjectReply {
                        header: ReplyHeader { error: 0, reply_size: 0 },
                        handle,
                        _pad_0: [0; 4],
                    });
                }
            }
        }

        reply_fixed(&OpenFileObjectReply {
            header: ReplyHeader { error: 0xc0000034, reply_size: 0 }, // STATUS_OBJECT_NAME_NOT_FOUND
            handle: 0,
            _pad_0: [0; 4],
        })
    }

    pub(crate) fn handle_read_directory_changes(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_read_change(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    // Fd info operations
    pub(crate) fn handle_set_fd_completion_mode(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_set_fd_disp_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_set_fd_eof_info(&mut self, client_fd: i32, buf: &[u8]) -> Reply {
        let req = if buf.len() >= std::mem::size_of::<SetFdEofInfoRequest>() {
            unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const SetFdEofInfoRequest) }
        } else {
            return reply_fixed(&ReplyHeader { error: 0xC000000D, reply_size: 0 });
        };
        let pid = self.client_pid(client_fd as RawFd);
        let handle_fd = self.state.processes.get(&pid)
            .and_then(|p| p.handles.get(req.handle))
            .and_then(|e| e.fd);
        if let Some(fd) = handle_fd {
            unsafe { libc::ftruncate(fd, req.eof as i64); }
        }
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

    pub(crate) fn handle_set_fd_name_info(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }


    // Mapping operations
    pub(crate) fn handle_is_same_mapping(&mut self, _client_fd: i32, _buf: &[u8]) -> Reply {
        reply_fixed(&ReplyHeader { error: 0, reply_size: 0 })
    }

}
