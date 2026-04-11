// Read PARALLAX shared memory to get real display hardware data.
//
// PARALLAX writes GPU, connector, and mode information to a named
// shm segment. Triskelion reads this at startup to populate Wine's
// display registry keys with real hardware data instead of placeholders.
//
// If PARALLAX is not running (segment doesn't exist), returns None
// and triskelion falls back to hardcoded defaults.

pub struct DisplayData {
    pub gpu: GpuData,
    pub connectors: Vec<ConnectorData>,
    pub driver_choice: u8,  // 0 = auto, 1 = winex11.drv, 2 = winewayland.drv
    pub _session_type: u32,  // 0 = unknown, 1 = x11, 2 = wayland
}

pub struct GpuData {
    pub pci_vendor: u32,
    pub pci_device: u32,
    pub pci_subsys_vendor: u32,
    pub pci_subsys_device: u32,
    pub pci_revision: u32,
    pub driver: String,
    pub _bus_id: String,
    pub gpu_name: String,
    pub vram_bytes: u64,
}

pub struct ConnectorData {
    pub name: String,
    pub _connector_type: u32,
    pub _mm_width: u32,
    pub _mm_height: u32,
    pub edid: Vec<u8>,
    pub modes: Vec<ModeData>,
    pub _current_mode_index: usize,
    pub current_width: u32,
    pub current_height: u32,
    pub current_refresh: u32,
}

pub struct ModeData {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

const SHM_MAGIC: u32 = 0x5359424C; // "SYBL"
const HEADER_SIZE: usize = 64;
const GPU_ENTRY_SIZE: usize = 256;
const CONNECTOR_HEADER_SIZE: usize = 128;
const MODE_ENTRY_SIZE: usize = 16;
const MAX_MODES_PER_CONNECTOR: usize = 64;
const CONNECTOR_ENTRY_SIZE: usize = CONNECTOR_HEADER_SIZE + MAX_MODES_PER_CONNECTOR * MODE_ENTRY_SIZE;
const SHM_SIZE: usize = 16384;

pub fn read_parallax_shm(prefix_hash: &str) -> Option<DisplayData> {
    let shm_name = format!("/parallax-{prefix_hash}");
    let c_name = std::ffi::CString::new(shm_name.as_str()).ok()?;

    let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 { return None; }

    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            SHM_SIZE,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    unsafe { libc::close(fd); }

    if base == libc::MAP_FAILED { return None; }

    let result = unsafe { parse_shm(base as *const u8) };

    unsafe { libc::munmap(base, SHM_SIZE); }

    // Data is in memory now. Unlink the shm segment — PARALLAX already exited.
    unsafe { libc::shm_unlink(c_name.as_ptr()); }

    result
}

unsafe fn parse_shm(base: *const u8) -> Option<DisplayData> {
    unsafe {
        let magic = *(base as *const u32);
        if magic != SHM_MAGIC { return None; }

        let gpu_count = *(base.add(8) as *const u32);
        let connector_count = *(base.add(12) as *const u32);
        let driver_choice = *base.add(20); // u8 at offset 20
        let session_type = *(base.add(24) as *const u32);

        if gpu_count == 0 { return None; }

        // GPU entry at offset 64
        let gpu_base = base.add(HEADER_SIZE);
        let pci_vendor = *(gpu_base as *const u32);
        let pci_device = *(gpu_base.add(4) as *const u32);
        let pci_subsys_vendor = *(gpu_base.add(8) as *const u32);
        let pci_subsys_device = *(gpu_base.add(12) as *const u32);
        let pci_revision = *(gpu_base.add(16) as *const u32);
        let driver = read_cstr(gpu_base.add(20), 63);
        let bus_id = read_cstr(gpu_base.add(84), 63);
        let gpu_name = read_cstr(gpu_base.add(148), 63);
        let vram_bytes = *(gpu_base.add(212) as *const u64);

        let gpu = GpuData {
            pci_vendor,
            pci_device,
            pci_subsys_vendor,
            pci_subsys_device,
            pci_revision,
            driver,
            _bus_id: bus_id,
            gpu_name,
            vram_bytes,
        };

        // Connectors
        let mut connectors = Vec::new();
        let count = (connector_count as usize).min(8);
        for i in 0..count {
            let conn_base = base.add(HEADER_SIZE + GPU_ENTRY_SIZE + i * CONNECTOR_ENTRY_SIZE);
            let connector_type = *(conn_base.add(4) as *const u32);
            let mm_width = *(conn_base.add(12) as *const u32);
            let mm_height = *(conn_base.add(16) as *const u32);
            let mode_count = (*(conn_base.add(20) as *const u32) as usize).min(MAX_MODES_PER_CONNECTOR);
            let cur_mode_idx = *(conn_base.add(24) as *const u32) as usize;
            let edid_len = (*(conn_base.add(28) as *const u32) as usize).min(128);
            let name = read_cstr(conn_base.add(32), 31);

            // EDID data at connector header offset 64
            let mut edid = vec![0u8; edid_len];
            if edid_len > 0 {
                std::ptr::copy_nonoverlapping(conn_base.add(64), edid.as_mut_ptr(), edid_len);
            }

            // Mode list
            let mut modes = Vec::with_capacity(mode_count);
            for j in 0..mode_count {
                let mode_base = conn_base.add(CONNECTOR_HEADER_SIZE + j * MODE_ENTRY_SIZE);
                modes.push(ModeData {
                    width: *(mode_base as *const u32),
                    height: *(mode_base.add(4) as *const u32),
                    refresh: *(mode_base.add(8) as *const u32),
                });
            }

            let (cur_w, cur_h, cur_r) = if cur_mode_idx < modes.len() {
                let m = &modes[cur_mode_idx];
                (m.width, m.height, m.refresh)
            } else if !modes.is_empty() {
                (modes[0].width, modes[0].height, modes[0].refresh)
            } else {
                (1920, 1080, 60)
            };

            connectors.push(ConnectorData {
                name,
                _connector_type: connector_type,
                _mm_width: mm_width,
                _mm_height: mm_height,
                edid,
                modes,
                _current_mode_index: cur_mode_idx,
                current_width: cur_w,
                current_height: cur_h,
                current_refresh: cur_r,
            });
        }

        Some(DisplayData { gpu, connectors, driver_choice, _session_type: session_type })
    }
}

unsafe fn read_cstr(ptr: *const u8, max_len: usize) -> String {
    unsafe {
        let mut len = 0;
        while len < max_len && *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        String::from_utf8_lossy(slice).into_owned()
    }
}

impl DisplayData {
    pub fn primary_resolution(&self) -> (i32, i32) {
        self.connectors.first()
            .map(|c| (c.current_width as i32, c.current_height as i32))
            .unwrap_or((1920, 1080))
    }

    /// Returns ("x11", "winex11.drv") or ("wayland", "winewayland.drv")
    /// based on PARALLAX detection (GPU vendor + session type + override).
    pub fn display_driver(&self) -> (&'static str, &'static str) {
        match self.driver_choice {
            2 => ("wayland", "winewayland.drv"),
            _ => ("x11", "winex11.drv"),
        }
    }

    pub fn gpu_guid(&self) -> String {
        format!("{:04x}{:04x}-0000-0000-0000-000000000000",
            self.gpu.pci_vendor, self.gpu.pci_device)
    }

    /// Parse EDID manufacturer code (bytes 8-9) into 3-letter ID
    pub fn edid_manufacturer(edid: &[u8]) -> String {
        if edid.len() < 10 { return "UNK".to_string(); }
        let raw = ((edid[8] as u16) << 8) | edid[9] as u16;
        let c1 = ((raw >> 10) & 0x1F) as u8 + b'A' - 1;
        let c2 = ((raw >> 5) & 0x1F) as u8 + b'A' - 1;
        let c3 = (raw & 0x1F) as u8 + b'A' - 1;
        format!("{}{}{}", c1 as char, c2 as char, c3 as char)
    }

    /// Parse EDID product code (bytes 10-11) as little-endian u16
    pub fn edid_product(edid: &[u8]) -> u16 {
        if edid.len() < 12 { return 0; }
        (edid[10] as u16) | ((edid[11] as u16) << 8)
    }
}
