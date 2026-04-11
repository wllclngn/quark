// Shared memory display info for triskelion
//
// PARALLAX enumerates real display hardware (DRM/KMS) and writes
// structured display info to a named shared memory segment.
// Triskelion reads this to populate Wine's display registry keys
// with real GPU, connector, and mode data.
//
// Layout (all little-endian):
//   Offset 0:    DisplayShmHeader (64 bytes)
//   Offset 64:   GpuEntry (256 bytes)
//   Offset 320:  ConnectorEntry[0] (512 bytes)
//   Offset 832:  ConnectorEntry[1] (512 bytes)
//   ...
//
// The shm segment name is: /parallax-<prefix-hash>

use crate::output::{DisplayHardware, GpuInfo, ConnectorInfo};
use std::sync::atomic::{AtomicU32, Ordering};

pub const SHM_MAGIC: u32 = 0x5359424C; // "SYBL"
pub const SHM_VERSION: u32 = 1;
pub const MAX_CONNECTORS: usize = 8;
pub const MAX_MODES_PER_CONNECTOR: usize = 64;

const HEADER_SIZE: usize = 64;
const GPU_ENTRY_SIZE: usize = 256;
const MODE_ENTRY_SIZE: usize = 16;
const CONNECTOR_HEADER_SIZE: usize = 128;
const CONNECTOR_ENTRY_SIZE: usize = CONNECTOR_HEADER_SIZE + MAX_MODES_PER_CONNECTOR * MODE_ENTRY_SIZE;

// Total max: 64 + 256 + 8 * (128 + 64*16) = 64 + 256 + 8*1152 = 9536 bytes
// Round up to page size
const SHM_SIZE: usize = 16384;

#[repr(C)]
pub struct DisplayShmHeader {
    pub magic: u32,
    pub version: u32,
    pub gpu_count: u32,
    pub connector_count: u32,
    pub sequence: AtomicU32,
    pub driver_choice: u8,    // 0 = auto, 1 = winex11.drv, 2 = winewayland.drv
    pub _pad_driver: [u8; 3],
    pub session_type: u32,    // 0 = unknown, 1 = x11, 2 = wayland
    pub _reserved: [u8; 36],
}

pub struct DisplayShm {
    base: *mut u8,
    fd: i32,
    shm_name: String,
}

unsafe impl Send for DisplayShm {}
unsafe impl Sync for DisplayShm {}

impl DisplayShm {
    pub fn create(prefix_hash: &str) -> Option<Self> {
        let shm_name = format!("/parallax-{prefix_hash}");
        let c_name = std::ffi::CString::new(shm_name.as_str()).ok()?;

        unsafe {
            // Remove stale segment
            libc::shm_unlink(c_name.as_ptr());

            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                0o644,
            );
            if fd < 0 { return None; }

            if libc::ftruncate(fd, SHM_SIZE as i64) < 0 {
                libc::close(fd);
                libc::shm_unlink(c_name.as_ptr());
                return None;
            }

            let base = libc::mmap(
                std::ptr::null_mut(),
                SHM_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if base == libc::MAP_FAILED {
                libc::close(fd);
                libc::shm_unlink(c_name.as_ptr());
                return None;
            }

            // Zero entire region
            std::ptr::write_bytes(base as *mut u8, 0, SHM_SIZE);

            Some(DisplayShm {
                base: base as *mut u8,
                fd,
                shm_name,
            })
        }
    }

    pub fn write_hardware(&self, hw: &DisplayHardware) {
        unsafe {
            let hdr = self.base as *mut DisplayShmHeader;
            (*hdr).magic = SHM_MAGIC;
            (*hdr).version = SHM_VERSION;
            (*hdr).gpu_count = hw.gpus.len().min(1) as u32;
            let conn_count = hw.connectors.iter()
                .filter(|c| c.connected)
                .count()
                .min(MAX_CONNECTORS);
            (*hdr).connector_count = conn_count as u32;

            // Detect session type from environment
            let has_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
            let has_x11 = std::env::var("DISPLAY").is_ok();
            let session = match (has_wayland, has_x11) {
                (true, _) => 2u32,  // Wayland (XWayland may also be available)
                (false, true) => 1, // X11 only
                _ => 0,             // unknown
            };
            (*hdr).session_type = session;

            // Determine display driver from GPU vendor + session type
            // Override via QUARK_DISPLAY_DRIVER takes priority
            let gpu_vendor = hw.gpus.first().map(|g| g.pci_vendor).unwrap_or(0);
            (*hdr).driver_choice = match std::env::var("QUARK_DISPLAY_DRIVER").ok().as_deref() {
                Some("winex11.drv") | Some("x11") => 1,
                Some("winewayland.drv") | Some("wayland") => 2,
                _ => {
                    // NVIDIA: EGL_BAD_MATCH on Wayland, must use X11/GLX
                    // AMD/Intel on Wayland: native EGL works
                    // X11-only session: must use X11
                    if gpu_vendor == 0x10de { 1 }         // NVIDIA -> x11
                    else if session == 1 { 1 }            // X11 only -> x11
                    else if session == 2 { 2 }            // Wayland + non-NVIDIA -> wayland
                    else { 1 }                            // unknown -> x11 (safe default)
                }
            };

            // Bump sequence (odd = writing, even = stable)
            (*hdr).sequence.store(1, Ordering::Release);

            // Write GPU
            if let Some(gpu) = hw.gpus.first() {
                self.write_gpu(gpu);
            }

            // Write connected connectors
            let connected: Vec<&ConnectorInfo> = hw.connectors.iter()
                .filter(|c| c.connected)
                .take(MAX_CONNECTORS)
                .collect();

            for (i, conn) in connected.iter().enumerate() {
                self.write_connector(i, conn);
            }

            // Sequence even = stable
            (*hdr).sequence.store(2, Ordering::Release);
        }
    }

    fn write_gpu(&self, gpu: &GpuInfo) {
        unsafe {
            let base = self.base.add(HEADER_SIZE);

            // Offset 0: pci_vendor (u32)
            *(base as *mut u32) = gpu.pci_vendor;
            // Offset 4: pci_device (u32)
            *(base.add(4) as *mut u32) = gpu.pci_device;
            // Offset 8: pci_subsys_vendor (u32)
            *(base.add(8) as *mut u32) = gpu.pci_subsys_vendor;
            // Offset 12: pci_subsys_device (u32)
            *(base.add(12) as *mut u32) = gpu.pci_subsys_device;
            // Offset 16: pci_revision (u32)
            *(base.add(16) as *mut u32) = gpu.pci_revision;

            // Offset 20: driver name (64 bytes, null-terminated UTF-8)
            let driver_bytes = gpu.driver.as_bytes();
            let len = driver_bytes.len().min(63);
            std::ptr::copy_nonoverlapping(driver_bytes.as_ptr(), base.add(20), len);

            // Offset 84: PCI bus ID (64 bytes, null-terminated UTF-8)
            let bus_bytes = gpu.pci_bus_id.as_bytes();
            let blen = bus_bytes.len().min(63);
            std::ptr::copy_nonoverlapping(bus_bytes.as_ptr(), base.add(84), blen);

            // Offset 148: GPU name (64 bytes, null-terminated UTF-8)
            let name_bytes = gpu.gpu_name.as_bytes();
            let nlen = name_bytes.len().min(63);
            std::ptr::copy_nonoverlapping(name_bytes.as_ptr(), base.add(148), nlen);

            // Offset 212: VRAM bytes (u64)
            *(base.add(212) as *mut u64) = gpu.vram_bytes;
        }
    }

    fn write_connector(&self, index: usize, conn: &ConnectorInfo) {
        let conn_offset = HEADER_SIZE + GPU_ENTRY_SIZE + index * CONNECTOR_ENTRY_SIZE;
        unsafe {
            let base = self.base.add(conn_offset);

            // Offset 0: connector_id (u32)
            *(base as *mut u32) = conn.connector_id;
            // Offset 4: connector_type (u32)
            *(base.add(4) as *mut u32) = conn.connector_type;
            // Offset 8: connector_type_id (u32)
            *(base.add(8) as *mut u32) = conn.connector_type_id;
            // Offset 12: mm_width (u32)
            *(base.add(12) as *mut u32) = conn.mm_width;
            // Offset 16: mm_height (u32)
            *(base.add(16) as *mut u32) = conn.mm_height;
            // Offset 20: mode_count (u32)
            let mode_count = conn.modes.len().min(MAX_MODES_PER_CONNECTOR);
            *(base.add(20) as *mut u32) = mode_count as u32;
            // Offset 24: current_mode_index (u32, 0xFFFFFFFF if none)
            let cur_idx = conn.current_mode.as_ref().and_then(|cur| {
                conn.modes.iter().position(|m| m.width == cur.width && m.height == cur.height && m.refresh == cur.refresh)
            }).map(|i| i as u32).unwrap_or(0xFFFFFFFF);
            *(base.add(24) as *mut u32) = cur_idx;
            // Offset 28: edid_len (u32)
            let edid_len = conn.edid.len().min(128) as u32;
            *(base.add(28) as *mut u32) = edid_len;

            // Offset 32: connector name (32 bytes, null-terminated UTF-8)
            let name_bytes = conn.name.as_bytes();
            let nlen = name_bytes.len().min(31);
            std::ptr::copy_nonoverlapping(name_bytes.as_ptr(), base.add(32), nlen);

            // Offset 64: EDID (up to 128 bytes inline, rest in extended area if needed)
            if edid_len > 0 {
                std::ptr::copy_nonoverlapping(
                    conn.edid.as_ptr(),
                    base.add(64),
                    edid_len as usize,
                );
            }

            // Offset CONNECTOR_HEADER_SIZE: mode entries
            for (j, mode) in conn.modes.iter().take(mode_count).enumerate() {
                let mode_base = base.add(CONNECTOR_HEADER_SIZE + j * MODE_ENTRY_SIZE);
                // Offset 0: width (u32)
                *(mode_base as *mut u32) = mode.width;
                // Offset 4: height (u32)
                *(mode_base.add(4) as *mut u32) = mode.height;
                // Offset 8: refresh (u32)
                *(mode_base.add(8) as *mut u32) = mode.refresh;
                // Offset 12: flags (u32)
                *(mode_base.add(12) as *mut u32) = mode.flags;
            }
        }
    }

    pub fn shm_name(&self) -> &str {
        &self.shm_name
    }
}

impl Drop for DisplayShm {
    fn drop(&mut self) {
        unsafe {
            if !self.base.is_null() {
                libc::munmap(self.base as *mut libc::c_void, SHM_SIZE);
            }
            if self.fd >= 0 {
                libc::close(self.fd);
            }
            // Do NOT shm_unlink here — triskelion reads the segment after
            // PARALLAX exits and handles cleanup via shm_unlink in
            // parallax_display::read_parallax_shm().
        }
    }
}
