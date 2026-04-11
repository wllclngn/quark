// In-memory registry tree for triskelion
//
// Empty registry — all reads of uncreated keys return STATUS_OBJECT_NAME_NOT_FOUND.
// Wine/Godot does ~1.2K registry misses at startup which is expected behavior.
// Keys are created on demand by Wine's registry initialization.

use std::collections::HashMap;
use rustc_hash::FxHashMap;

// Case-insensitive key/value name.
// Windows registry is case-insensitive for lookup but preserves original case.
// We store original case for enumeration, and use lowercased form for Hash/Eq.
#[derive(Clone)]
pub(crate) struct RegName {
    original: String, // preserved case (for NtEnumerateKey)
    lower: String,    // lowercase (for case-insensitive matching)
}

impl std::hash::Hash for RegName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.lower.hash(state);
    }
}

impl PartialEq for RegName {
    fn eq(&self, other: &Self) -> bool {
        self.lower == other.lower
    }
}

impl Eq for RegName {}

impl RegName {
    fn new(s: &str) -> Self {
        RegName { original: s.to_string(), lower: s.to_lowercase() }
    }

    fn from_utf16le(bytes: &[u8]) -> Self {
        let chars: Vec<u16> = bytes.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16_lossy(&chars);
        // Strip null terminators — Wine includes them in vararg but they're not part of the name
        let s = s.trim_end_matches('\0').to_string();
        RegName { original: s.clone(), lower: s.to_lowercase() }
    }
}

struct RegistryKey {
    children: HashMap<RegName, RegistryKey>,
    values: HashMap<RegName, RegistryValue>,
    value_names: Vec<RegName>, // insertion order for enum_value
}

impl RegistryKey {
    fn new() -> Self {
        Self {
            children: HashMap::new(),
            values: HashMap::new(),
            value_names: Vec::new(),
        }
    }
}

struct RegistryValue {
    data_type: u32,
    data: Vec<u8>,
}

pub(crate) struct RegNotify {
    pub(crate) path: Vec<RegName>,
    pub(crate) pid: u32,
    pub(crate) event_handle: u32,
    pub(crate) subtree: bool,
    pub(crate) filter: u32,
}

pub struct Registry {
    // Root keys: HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER, etc.
    // Wine uses path strings like "\Registry\Machine\..." — we store the root node.
    root: RegistryKey,
    // Map from open handle → path segments
    open_keys: FxHashMap<u32, Vec<RegName>>,
    next_hkey: u32,
    // Monotonic write counter — incremented on every set_value/create_key.
    // Used as LastWriteTime in NtQueryKey so Wine detects registry changes.
    write_seq: u64,
    // User SID for HKCU paths, stored for apply_display_driver reuse.
    user_sid: String,
    // Registry change notifications: one-shot, fired then removed.
    notify_list: Vec<RegNotify>,
}

impl Registry {
    pub fn new(user_sid: &str) -> Self {
        let mut reg = Self {
            root: RegistryKey::new(),
            open_keys: FxHashMap::default(),
            next_hkey: 1,
            write_seq: 1,
            user_sid: user_sid.to_string(),
            notify_list: Vec::new(),
        };
        // Create symlinks FIRST so .reg loader can resolve CurrentControlSet → ControlSet001
        reg.init_runtime_keys(user_sid);
        reg.load_prefix_registry(user_sid);
        reg
    }

    /// Runtime keys that stock wineserver creates programmatically in init_registry().
    /// These are NOT in the .reg files — the server creates them at startup.
    fn init_runtime_keys(&mut self, user_sid: &str) {
        // CurrentControlSet → ControlSet001 symlink (REG_LINK).
        // Stock wineserver only creates this if system.reg failed to load (line 1935),
        // but it's always present in system.reg from wineboot. Create it as a fallback
        // in case system.reg is missing or doesn't contain it.
        let ccs_path = "Registry\\Machine\\System\\CurrentControlSet";
        let segments: Vec<RegName> = ccs_path.split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| RegName::new(s))
            .collect();
        let node = self.walk_mut_create(&segments);
        let link_name = RegName::new("SymbolicLinkValue");
        if !node.values.contains_key(&link_name) {
            let target = "\\Registry\\Machine\\System\\ControlSet001";
            let target_u16: Vec<u16> = target.encode_utf16().collect();
            let link_data: Vec<u8> = target_u16.iter().flat_map(|c| c.to_le_bytes()).collect();
            node.value_names.push(link_name.clone());
            node.values.insert(link_name, RegistryValue { data_type: 6, data: link_data });
        }

        // KEY_WOWSHARE on HKCU\Software (line 1983)
        // KEY_WOWREFLECT on Software\Classes\Wow6432Node (line 1974)
        // KEY_PREDEF on Perflib\009 (line 1990)
        // These are flag-only operations on the key objects. Our RegistryKey struct
        // doesn't have flags yet — the keys themselves are created by .reg loading.
        // TODO: add key flags when needed for WOW64 compatibility.

        // Display driver: default to winex11.drv (safe fallback).
        // PARALLAX overrides this via apply_display_driver() when shm is available.
        self.apply_display_driver(user_sid, "x11", "winex11.drv");

        // DeviceMap\Video: maps \Device\Video0 → the GPU's registry path.
        // Without this, EnumDisplayDevices returns nothing and SDL2 can't create a window.
        let devmap_path = "Registry\\Machine\\Hardware\\DeviceMap\\Video";
        let devmap_segments: Vec<RegName> = devmap_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let devmap_node = self.walk_mut_create(&devmap_segments);
        let dev_video_name = RegName::new("\\Device\\Video0");
        let dev_video_val: Vec<u8> = "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Video\\{00000000-0000-0000-0000-000000000000}\\0000"
            .encode_utf16().flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
        devmap_node.value_names.push(dev_video_name.clone());
        devmap_node.values.insert(dev_video_name, RegistryValue { data_type: 1, data: dev_video_val });

        // Display adapter class: {4d36e968-e325-11ce-bfc1-08002be10318}\0000
        // Wine reads this to build the display device list.
        let class_path = "Registry\\Machine\\System\\ControlSet001\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}";
        let class_segments: Vec<RegName> = class_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let class_node = self.walk_mut_create(&class_segments);
        let class_name = RegName::new("Class");
        let class_val: Vec<u8> = "Display".encode_utf16()
            .flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
        class_node.value_names.push(class_name.clone());
        class_node.values.insert(class_name, RegistryValue { data_type: 1, data: class_val });

        // GPU info under the class key: 0000 subkey
        let gpu_class_path = format!("{class_path}\\0000");
        let gpu_class_segments: Vec<RegName> = gpu_class_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let gpu_class_node = self.walk_mut_create(&gpu_class_segments);

        // GPU name: set by apply_display_registry when PARALLAX data arrives.
        // Use generic placeholder here; PARALLAX overrides it at startup.
        let gpu_name = "Wine Display Adapter".to_string();
        let gpu_name_val: Vec<u8> = gpu_name.encode_utf16()
            .flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
        let dac_val: Vec<u8> = "Intergrated RAMDAC".encode_utf16()
            .flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();

        // Set GPU values on class node
        for (name, val) in &[
            ("DriverDesc", gpu_name_val.clone()),
            ("HardwareInformation.AdapterString", gpu_name_val.clone()),
            ("HardwareInformation.BiosString", gpu_name_val.clone()),
            ("HardwareInformation.ChipType", gpu_name_val.clone()),
            ("HardwareInformation.DacType", dac_val.clone()),
        ] {
            let rn = RegName::new(name);
            gpu_class_node.value_names.push(rn.clone());
            gpu_class_node.values.insert(rn, RegistryValue { data_type: 1, data: val.clone() });
        }
        let mem_name = RegName::new("HardwareInformation.MemorySize");
        gpu_class_node.value_names.push(mem_name.clone());
        gpu_class_node.values.insert(mem_name, RegistryValue { data_type: 4, data: 0x10000000u32.to_le_bytes().to_vec() });

        // Also set GPU info on the Video GUID 0000 key
        let video_path = "Registry\\Machine\\System\\ControlSet001\\Control\\Video\\{00000000-0000-0000-0000-000000000000}\\0000";
        let video_segments: Vec<RegName> = video_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let gpu_video_node = self.walk_mut_create(&video_segments);
        for (name, val) in &[
            ("DriverDesc", gpu_name_val.clone()),
            ("HardwareInformation.AdapterString", gpu_name_val.clone()),
            ("HardwareInformation.BiosString", gpu_name_val.clone()),
            ("HardwareInformation.ChipType", gpu_name_val.clone()),
            ("HardwareInformation.DacType", dac_val.clone()),
        ] {
            let rn = RegName::new(name);
            gpu_video_node.value_names.push(rn.clone());
            gpu_video_node.values.insert(rn, RegistryValue { data_type: 1, data: val.clone() });
        }
        let mem_name2 = RegName::new("HardwareInformation.MemorySize");
        gpu_video_node.value_names.push(mem_name2.clone());
        gpu_video_node.values.insert(mem_name2, RegistryValue { data_type: 4, data: 0x10000000u32.to_le_bytes().to_vec() });

        // Enum\PCI device entry (needed for enum_device_keys("PCI", ...))
        let pci_path = "Registry\\Machine\\System\\ControlSet001\\Enum\\PCI\\VEN_0000&DEV_0000&SUBSYS_00000000&REV_00\\0000";
        let pci_segments: Vec<RegName> = pci_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let pci_node = self.walk_mut_create(&pci_segments);
        let classguid_name = RegName::new("ClassGUID");
        let classguid_val: Vec<u8> = "{4d36e968-e325-11ce-bfc1-08002be10318}".encode_utf16()
            .flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
        pci_node.value_names.push(classguid_name.clone());
        pci_node.values.insert(classguid_name, RegistryValue { data_type: 1, data: classguid_val });
        let driver_name = RegName::new("Driver");
        let driver_val: Vec<u8> = "{4d36e968-e325-11ce-bfc1-08002be10318}\\0000".encode_utf16()
            .flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
        pci_node.value_names.push(driver_name.clone());
        pci_node.values.insert(driver_name, RegistryValue { data_type: 1, data: driver_val });

        // Enum\DISPLAY\Default_Monitor entry (needed for enum_device_keys("DISPLAY", ...))
        let mon_path = "Registry\\Machine\\System\\ControlSet001\\Enum\\DISPLAY\\Default_Monitor\\0000&0000";
        let mon_segments: Vec<RegName> = mon_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let mon_node = self.walk_mut_create(&mon_segments);
        let mon_classguid_name = RegName::new("ClassGUID");
        let mon_classguid_val: Vec<u8> = "{4d36e96e-e325-11ce-bfc1-08002be10318}".encode_utf16()
            .flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
        mon_node.value_names.push(mon_classguid_name.clone());
        mon_node.values.insert(mon_classguid_name, RegistryValue { data_type: 1, data: mon_classguid_val });
        let mon_driver_name = RegName::new("Driver");
        let mon_driver_val: Vec<u8> = "{4d36e96e-e325-11ce-bfc1-08002be10318}\\0000".encode_utf16()
            .flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
        mon_node.value_names.push(mon_driver_name.clone());
        mon_node.values.insert(mon_driver_name, RegistryValue { data_type: 1, data: mon_driver_val });

        // Monitor class entry
        let mon_class_path = "Registry\\Machine\\System\\ControlSet001\\Control\\Class\\{4d36e96e-e325-11ce-bfc1-08002be10318}\\0000";
        let mon_class_segments: Vec<RegName> = mon_class_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        self.walk_mut_create(&mon_class_segments);

        // UseEGL: disable EGL backend, force GLX. NVIDIA's EGL on Optimus laptops
        // initializes but can't render. GLX works. Wine defaults to EGL when the
        // extension EGL_KHR_client_get_all_proc_addresses is present.
        let x11drv_path = format!("Registry\\User\\{user_sid}\\Software\\Wine\\X11 Driver");
        let x11drv_segments: Vec<RegName> = x11drv_path.split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| RegName::new(s))
            .collect();
        let x11drv_node = self.walk_mut_create(&x11drv_segments);
        let use_egl_name = RegName::new("UseEGL");
        let use_egl_val: Vec<u8> = "N".encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .chain(0u16.to_le_bytes())
            .collect();
        x11drv_node.value_names.push(use_egl_name.clone());
        x11drv_node.values.insert(use_egl_name, RegistryValue { data_type: 1, data: use_egl_val });

        // AppInit_DLLs: force-load drv_init.dll into every process that loads user32.dll.
        // drv_init.dll calls SetCursorPos(0,0) in DllMain which triggers Wine's lazy
        // display driver load (loaderdrv_SetCursorPos → load_driver → winewayland.drv).
        // Without this, SDL games never trigger the driver load because they bypass
        // Win32 input functions, and the pre-created desktop window prevents Wine from
        // spawning explorer.exe.
        let appinit_path = "Registry\\Machine\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Windows";
        let appinit_segments: Vec<RegName> = appinit_path.split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| RegName::new(s))
            .collect();
        let appinit_node = self.walk_mut_create(&appinit_segments);
        let appinit_name = RegName::new("AppInit_DLLs");
        let appinit_val: Vec<u8> = "drv_init.dll".encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .chain(0u16.to_le_bytes())
            .collect();
        appinit_node.value_names.push(appinit_name.clone());
        appinit_node.values.insert(appinit_name, RegistryValue { data_type: 1, data: appinit_val });
        // LoadAppInit_DLLs must be set to 1 for AppInit_DLLs to take effect
        let load_name = RegName::new("LoadAppInit_DLLs");
        let load_val = 1u32.to_le_bytes().to_vec();
        appinit_node.value_names.push(load_name.clone());
        appinit_node.values.insert(load_name, RegistryValue { data_type: 4, data: load_val });

        // SvcHost service groups. svchost.exe reads these to determine which
        // DLLs to load for each "-k group" invocation. Without these, service
        // processes fail with "Failed to load requested group" and exit,
        // cascading into broken device drivers (SharedGpuResources, nsiproxy, etc.)
        let svchost_path = "Registry\\Machine\\Software\\Microsoft\\Windows NT\\CurrentVersion\\SvcHost";
        let svchost_segments: Vec<RegName> = svchost_path.split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| RegName::new(s))
            .collect();
        let svchost_node = self.walk_mut_create(&svchost_segments);
        // REG_MULTI_SZ (type 7): null-separated UTF-16LE strings, double-null terminated
        let multi_sz = |services: &[&str]| -> Vec<u8> {
            let mut data = Vec::new();
            for s in services {
                data.extend(s.encode_utf16().flat_map(|c| c.to_le_bytes()));
                data.extend(&[0u8, 0]); // null terminator for this string
            }
            data.extend(&[0u8, 0]); // final double-null
            data
        };
        let svchost_groups: &[(&str, &[&str])] = &[
            ("netsvcs", &["Themes", "BITS", "AudioEndpointBuilder", "Audiosrv", "CertPropSvc",
                          "LanmanWorkstation", "ProfSvc", "SENS", "ShellHWDetection"]),
            ("LocalServiceNetworkRestricted", &["nsi", "DhcpSvc", "EventLog"]),
            ("LocalService", &["nsi", "WinHttpAutoProxySvc"]),
            ("LocalServiceNoNetwork", &["PLA"]),
            ("LocalSystemNetworkRestricted", &["WPDBusEnum"]),
        ];
        for (group, services) in svchost_groups {
            let name = RegName::new(group);
            let data = multi_sz(services);
            if !svchost_node.values.contains_key(&name) { svchost_node.value_names.push(name.clone()); }
            svchost_node.values.insert(name, RegistryValue { data_type: 7, data });
        }

        // Service entries under ControlSet001\Services.
        // svchost reads ServiceDll from each service's Parameters key.
        // Without these, svchost fails to load service groups and device
        // drivers (nsiproxy, SharedGpuResources, NDIS, etc.) never start.
        let svc_base = "Registry\\Machine\\System\\ControlSet001\\Services";
        let reg_dword = |val: u32| -> RegistryValue {
            RegistryValue { data_type: 4, data: val.to_le_bytes().to_vec() }
        };
        let reg_expand_sz = |s: &str| -> RegistryValue {
            let mut data: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            data.extend(&[0, 0]); // null terminator
            RegistryValue { data_type: 2, data }
        };
        let reg_sz = |s: &str| -> RegistryValue {
            let mut data: Vec<u8> = s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            data.extend(&[0, 0]);
            RegistryValue { data_type: 1, data }
        };
        // (service_name, Type, Start, ImagePath, group, [(param_key, param_val)])
        let services: &[(&str, u32, u32, &str, &str, &[(&str, &str)])] = &[
            ("nsi",              0x20, 2, "%SystemRoot%\\system32\\svchost.exe -k LocalServiceNetworkRestricted",
             "TDI", &[("ServiceDll", "%SystemRoot%\\system32\\nsisvc.dll")]),
            ("nsiproxy",         1,    1, "system32\\drivers\\nsiproxy.sys", "PNP_TDI", &[]),
            ("NDIS",             1,    1, "system32\\drivers\\ndis.sys", "NDIS", &[]),
            ("Dhcp",             0x20, 2, "%SystemRoot%\\system32\\svchost.exe -k LocalServiceNetworkRestricted",
             "TDI", &[("ServiceDll", "%SystemRoot%\\system32\\dhcpsvc.dll")]),
            ("Eventlog",         0x20, 2, "%SystemRoot%\\system32\\svchost.exe -k LocalServiceNetworkRestricted",
             "", &[("ServiceDll", "%SystemRoot%\\system32\\wevtsvc.dll")]),
            ("SharedGpuResources", 1, 2, "C:\\windows\\system32\\drivers\\sharedgpures.sys",
             "System Bus Extender", &[]),
            ("MountMgr",         1,    0, "system32\\drivers\\mountmgr.sys", "System Bus Extender", &[]),
            ("Winedevice1",      0x10, 3, "%SystemRoot%\\system32\\winedevice.exe", "", &[]),
            ("Winedevice2",      0x10, 3, "%SystemRoot%\\system32\\winedevice.exe", "", &[]),
            ("winebus",          1,    2, "system32\\drivers\\winebus.sys", "", &[]),
            ("wineusb",          1,    2, "system32\\drivers\\wineusb.sys", "", &[]),
            ("winebth",          1,    2, "system32\\drivers\\winebth.sys", "", &[]),
        ];
        for &(name, svc_type, start, image_path, group, params) in services {
            let svc_path = format!("{svc_base}\\{name}");
            let segments: Vec<RegName> = svc_path.split('\\')
                .filter(|s| !s.is_empty())
                .map(|s| RegName::new(s))
                .collect();
            let node = self.walk_mut_create(&segments);
            // Always write service entries (overwrite stale prefix data)
            for (vname, val) in [
                ("Type", reg_dword(svc_type)),
                ("Start", reg_dword(start)),
                ("ImagePath", reg_expand_sz(image_path)),
            ] {
                let rn = RegName::new(vname);
                if !node.values.contains_key(&rn) { node.value_names.push(rn.clone()); }
                node.values.insert(rn, val);
            }
            if !group.is_empty() {
                let g_name = RegName::new("Group");
                if !node.values.contains_key(&g_name) { node.value_names.push(g_name.clone()); }
                node.values.insert(g_name, reg_sz(group));
            }
            if !params.is_empty() {
                let params_path = format!("{svc_path}\\Parameters");
                let param_segments: Vec<RegName> = params_path.split('\\')
                    .filter(|s| !s.is_empty())
                    .map(|s| RegName::new(s))
                    .collect();
                let pnode = self.walk_mut_create(&param_segments);
                for &(pname, pval) in params {
                    let pk = RegName::new(pname);
                    if !pnode.values.contains_key(&pk) {
                        pnode.value_names.push(pk.clone());
                        pnode.values.insert(pk, reg_expand_sz(pval));
                    }
                }
            }
        }

        // COM class registrations: 589 entries extracted from Proton 10.0's default_pfx.
        // QUARK_FAST_BOOT skips wineboot's FakeDlls pass, so we inject all CLSIDs here.
        let com_classes = crate::com_classes::COM_CLASSES;
        let clsid_base = "Registry\\Machine\\Software\\Classes\\CLSID";
        for (clsid, dll_path, threading) in com_classes {
            let key_path = format!("{clsid_base}\\{clsid}\\InprocServer32");
            let segments: Vec<RegName> = key_path.split('\\')
                .filter(|s| !s.is_empty())
                .map(|s| RegName::new(s))
                .collect();
            let node = self.walk_mut_create(&segments);
            let default_name = RegName::new("");
            if !node.values.contains_key(&default_name) {
                node.value_names.push(default_name.clone());
            }
            node.values.insert(default_name, reg_sz(dll_path));
            let tm_name = RegName::new("ThreadingModel");
            if !node.values.contains_key(&tm_name) {
                node.value_names.push(tm_name.clone());
            }
            node.values.insert(tm_name, reg_sz(threading));
        }

        log_info!("registry: initialized runtime keys (SID: {user_sid})");
    }

    // Navigate to a key by path segments, optionally creating along the way.
    fn walk(&self, path: &[RegName]) -> Option<&RegistryKey> {
        let mut node = &self.root;
        for (i, seg) in path.iter().enumerate() {
            match node.children.get(seg) {
                Some(child) => node = child,
                None => {
                    // Diagnostic: dump what we have vs what we're looking for
                    let _path_so_far: Vec<&str> = path[..i].iter().map(|p| p.lower.as_str()).collect();
                    let _avail: Vec<(&str, Vec<u8>)> = node.children.keys()
                        .map(|k| (k.lower.as_str(), k.lower.as_bytes().to_vec()))
                        .collect();
                    let _seg_bytes: Vec<u8> = seg.lower.as_bytes().to_vec();
                    let _seg_hash = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        seg.hash(&mut h);
                        h.finish()
                    };
                    // If the segment LOOKS like it should match a child, dump both byte sequences
                    let near_match = node.children.keys()
                        .find(|k| k.lower.starts_with(&seg.lower[..seg.lower.len().min(3)]));
                    if let Some(nm) = near_match {
                        let _nm_bytes: Vec<u8> = nm.lower.as_bytes().to_vec();
                        let _nm_hash = {
                            use std::hash::{Hash, Hasher};
                            let mut h = std::collections::hash_map::DefaultHasher::new();
                            nm.hash(&mut h);
                            h.finish()
                        };
                    } else {
                    }
                    return None;
                }
            }
        }
        // Follow REG_LINK at the final node for value reads.
        // Wine uses this for display device keys: \0000 has SymbolicLinkValue
        // pointing to \Sources\<output_name> so value reads redirect.
        // Guard against circular links (e.g., ControlSet001\...\0000 →
        // CurrentControlSet\...\0000 → back to ControlSet001) with depth limit.
        let link_name = RegName::new("symboliclinkvalue");
        if let Some(val) = node.values.get(&link_name) {
            if val.data_type == 6 { // REG_LINK
                let target = parse_key_path(&val.data);
                let _target_str: Vec<&str> = target.iter().map(|s| s.lower.as_str()).collect();
                let _path_str: Vec<&str> = path.iter().map(|s| s.lower.as_str()).collect();
                if !target.is_empty() && target != path.to_vec() {
                    // Resolve symlinks in the target path (e.g., CurrentControlSet → ControlSet001)
                    // before walking, then depth-limited recursive follow (max 4 hops).
                    let resolved_target = self.follow_symlink(target);
                    static DEPTH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
                    let d = DEPTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let result = if d < 4 {
                        self.walk(&resolved_target)
                    } else {
                        None
                    };
                    DEPTH.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                    if result.is_some() {
                        return result;
                    }
                    // Link target not found — fall through to return the node itself
                }
            }
        }
        Some(node)
    }

    fn walk_mut_create(&mut self, path: &[RegName]) -> &mut RegistryKey {
        let mut node = &mut self.root;
        for seg in path {
            node = node.children.entry(seg.clone()).or_insert_with(RegistryKey::new);
        }
        node
    }

    // Resolve a parent hkey to its path segments (empty for root).
    fn resolve_parent(&self, parent_hkey: u32) -> Vec<RegName> {
        if parent_hkey == 0 {
            Vec::new()
        } else {
            self.open_keys.get(&parent_hkey).cloned().unwrap_or_default()
        }
    }

    // Create or open a key. Returns (hkey, created).
    pub fn create_key(&mut self, parent: u32, name: &[u8]) -> (u32, bool) {
        self.write_seq += 1;
        let mut path = self.resolve_parent(parent);
        let segments = parse_key_path(name);
        path.extend(segments);
        let path = self.follow_symlink(path);

        let existed = self.walk(&path).is_some();
        self.walk_mut_create(&path);

        let hkey = self.next_hkey;
        self.next_hkey += 1;
        self.open_keys.insert(hkey, path);
        (hkey, !existed)
    }

    // Open an existing key. Returns hkey or None.
    // Follows registry symlinks (keys with a SymbolicLinkValue).
    pub fn open_key(&mut self, parent: u32, name: &[u8]) -> Option<u32> {
        let mut path = self.resolve_parent(parent);
        let segments = parse_key_path(name);
        path.extend(segments);

        // Follow symlinks: if the node has a SymbolicLinkValue (REG_LINK), resolve to target.
        let path = self.follow_symlink(path);

        let walk_result = self.walk(&path);
        if walk_result.is_some() {
            let hkey = self.next_hkey;
            self.next_hkey += 1;
            self.open_keys.insert(hkey, path);
            Some(hkey)
        } else {
            let _path_str: Vec<&str> = path.iter().map(|p| p.lower.as_str()).collect();
            None
        }
    }

    // Resolve registry symlinks at EACH intermediate segment along the path.
    // Stock wineserver follows symlinks during key traversal, not just at the leaf.
    // Example: CurrentControlSet has SymbolicLinkValue → \Registry\Machine\System\ControlSet001
    //          so CurrentControlSet\Services\foo → ControlSet001\Services\foo
    fn follow_symlink(&self, path: Vec<RegName>) -> Vec<RegName> {
        let link_name = RegName::new("symboliclinkvalue");
        let mut current = path;
        // Iterate: each symlink resolution restarts the walk from the beginning
        // because the target path may itself contain symlinks (e.g., CurrentControlSet).
        // Max depth prevents infinite loops from circular symlinks.
        for _depth in 0..8 {
            let mut found_link = false;
            let mut node = &self.root;
            for (i, seg) in current.iter().enumerate() {
                node = match node.children.get(seg) {
                    Some(n) => n,
                    None => return current,
                };
                if let Some(val) = node.values.get(&link_name) {
                    if val.data_type == 6 { // REG_LINK
                        let target = parse_key_path(&val.data);
                        if !target.is_empty() {
                            let mut resolved = target;
                            resolved.extend_from_slice(&current[i + 1..]);
                            current = resolved;
                            found_link = true;
                            break; // restart walk from beginning with new path
                        }
                    }
                }
            }
            if !found_link {
                break; // no more symlinks found
            }
        }
        current
    }

    // Get a value by name. Returns (type, data) or None.
    pub fn get_value(&self, hkey: u32, name: &[u8]) -> Option<(u32, &[u8])> {
        let path = self.open_keys.get(&hkey)?;
        let node = self.walk(path)?;
        let vname = RegName::from_utf16le(name);
        node.values.get(&vname).map(|v| (v.data_type, v.data.as_slice()))
    }

    /// Monotonic write counter for LastWriteTime in NtQueryKey.
    pub fn write_counter(&self) -> u64 { self.write_seq }

    // Set a value.
    pub fn set_value(&mut self, hkey: u32, name: &[u8], data_type: u32, data: &[u8]) {
        self.write_seq += 1;
        let path = if let Some(p) = self.open_keys.get(&hkey) {
            p.clone()
        } else {
            return;
        };
        let vname = RegName::from_utf16le(name);
        let node = self.walk_mut_create(&path);
        let val = RegistryValue { data_type, data: data.to_vec() };

        if !node.values.contains_key(&vname) {
            node.value_names.push(vname.clone());
        }
        node.values.insert(vname, val);
    }

    /// Delete a value from a key. Returns true if deleted.
    pub fn delete_value(&mut self, hkey: u32, name: &[u8]) -> bool {
        let path = match self.open_keys.get(&hkey) {
            Some(p) => p.clone(),
            None => return false,
        };
        let vname = RegName::from_utf16le(name);
        if let Some(node) = self.walk_mut(&path) {
            if node.values.remove(&vname).is_some() {
                node.value_names.retain(|n| n != &vname);
                return true;
            }
        }
        false
    }

    // Query key metadata (NtQueryKey). Returns (subkey_count, value_count, max_value_name_len, max_data_len).
    pub fn query_key(&self, hkey: u32) -> Option<(i32, i32, u32, u32)> {
        let path = self.open_keys.get(&hkey)?;
        let node = self.walk(path)?;
        // Calculate max value name length (in bytes, UTF-16LE) and max data length
        let max_value: u32 = node.value_names.iter()
            .map(|n| (n.original.encode_utf16().count() * 2) as u32)
            .max()
            .unwrap_or(0);
        let max_data: u32 = node.values.values()
            .map(|v| v.data.len() as u32)
            .max()
            .unwrap_or(0);
        Some((node.children.len() as i32, node.values.len() as i32, max_value, max_data))
    }

    // Enumerate child key at index. Returns (name_utf16le, subkey_count, value_count).
    pub fn enum_key(&self, hkey: u32, index: usize) -> Option<(Vec<u8>, i32, i32)> {
        let path = self.open_keys.get(&hkey)?;
        let node = self.walk(path)?;
        let mut names: Vec<&RegName> = node.children.keys().collect();
        names.sort_by(|a, b| a.lower.cmp(&b.lower));
        let name = names.get(index)?;
        let child = node.children.get(*name)?;
        let name_u16: Vec<u16> = name.original.encode_utf16().collect();
        let name_bytes: Vec<u8> = name_u16.iter()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        Some((name_bytes, child.children.len() as i32, child.values.len() as i32))
    }

    // Enumerate value at index. Returns (name_utf16le, type, data) or None.
    pub fn enum_value(&self, hkey: u32, index: usize) -> Option<(Vec<u8>, u32, &[u8])> {
        let path = self.open_keys.get(&hkey)?;
        let node = self.walk(path)?;
        let name = node.value_names.get(index)?;
        let val = node.values.get(name)?;
        // Convert name back to UTF-16LE
        let name_u16: Vec<u16> = name.original.encode_utf16().collect();
        let name_bytes: Vec<u8> = name_u16.iter()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        Some((name_bytes, val.data_type, val.data.as_slice()))
    }

    /// Get the path for a handle (for diagnostics).
    /// Delete a key by its handle. Removes it from its parent's children.
    /// Returns true if deleted, false if not found or has subkeys.
    pub fn delete_key(&mut self, hkey: u32) -> bool {
        let path = match self.open_keys.get(&hkey) {
            Some(p) if !p.is_empty() => p.clone(),
            _ => return false,
        };

        // Check if key has subkeys (can't delete non-empty in Windows)
        if let Some(node) = self.walk(&path) {
            if !node.children.is_empty() {
                return false; // STATUS_CANNOT_DELETE — has subkeys
            }
        } else {
            return false;
        }

        // Remove from parent
        if path.len() >= 2 {
            let parent_path = &path[..path.len() - 1];
            let child_name = &path[path.len() - 1];
            if let Some(parent) = self.walk_mut(parent_path) {
                parent.children.remove(child_name);
                return true;
            }
        } else if path.len() == 1 {
            // Top-level key under root
            self.root.children.remove(&path[0]);
            return true;
        }
        false
    }

    /// Mutable walk without creating nodes (for delete).
    fn walk_mut(&mut self, path: &[RegName]) -> Option<&mut RegistryKey> {
        let mut node = &mut self.root;
        for seg in path {
            node = node.children.get_mut(seg)?;
        }
        Some(node)
    }

    pub fn get_handle_path(&self, hkey: u32) -> Option<String> {
        self.open_keys.get(&hkey).map(|p| {
            p.iter().map(|s| s.original.as_str()).collect::<Vec<_>>().join("\\")
        })
    }

    // Register a change notification. Returns true if the hkey is valid.
    pub fn register_notify(&mut self, hkey: u32, pid: u32, event_handle: u32, subtree: bool, filter: u32) -> bool {
        if let Some(path) = self.open_keys.get(&hkey) {
            self.notify_list.push(RegNotify {
                path: path.clone(), pid, event_handle, subtree, filter,
            });
            true
        } else {
            false
        }
    }

    // Remove all pending notifications for a dead process.
    pub fn remove_notifications_for_pid(&mut self, pid: u32) {
        self.notify_list.retain(|n| n.pid != pid);
    }

    // Collect notifications that match a mutation at changed_hkey with the given
    // change type (REG_NOTIFY_CHANGE_NAME=0x01, REG_NOTIFY_CHANGE_LAST_SET=0x04).
    // Returns (pid, event_handle) pairs. Removes fired entries (one-shot).
    pub fn collect_notifications(&mut self, changed_hkey: u32, change: u32) -> Vec<(u32, u32)> {
        let changed_path = match self.open_keys.get(&changed_hkey) {
            Some(p) => p.clone(),
            None => return Vec::new(),
        };
        let mut fired = Vec::new();
        self.notify_list.retain(|n| {
            let matches = if n.path == changed_path {
                (n.filter & change) != 0
            } else if n.subtree && changed_path.starts_with(&n.path) {
                (n.filter & change) != 0
            } else {
                false
            };
            if matches {
                fired.push((n.pid, n.event_handle));
                false
            } else {
                true
            }
        });
        fired
    }

    /// Load prefix .reg files into the in-memory registry.
    /// system.reg → Registry\Machine\..., user.reg → Registry\User\<SID>\..., userdef.reg → Registry\User\.Default\...
    fn load_prefix_registry(&mut self, user_sid: &str) {
        let prefix = std::env::var("WINEPREFIX")
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").expect("HOME not set");
                format!("{home}/.wine")
            });
        let prefix = std::path::Path::new(&prefix);

        let files: &[(&str, &str)] = &[
            ("system.reg", "Registry\\Machine"),
            ("user.reg", &format!("Registry\\User\\{user_sid}")),
            ("userdef.reg", "Registry\\User\\.Default"),
        ];

        let mut total_keys = 0u32;
        let mut total_values = 0u32;

        for &(filename, root_prefix) in files {
            let path = prefix.join(filename);
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let (keys, values) = self.load_reg_file(&content, root_prefix);
                    total_keys += keys;
                    total_values += values;
                    log_info!("registry: loaded {filename}: {keys} keys, {values} values");
                }
                Err(_) => {
                }
            }
        }

        log_info!("registry: loaded {total_keys} keys, {total_values} values from prefix");

        // Verify GPU class key loaded
        let gpu_path: Vec<RegName> = ["registry", "machine", "system", "controlset001", "control", "class",
            "{4d36e968-e325-11ce-bfc1-08002be10318}"]
            .iter().map(|s| RegName::new(s)).collect();
        if let Some(node) = self.walk(&gpu_path) {
            let children: Vec<&str> = node.children.keys().map(|k| k.lower.as_str()).collect();
            log_info!("registry: GPU class children = {:?}", children);
        } else {
            log_info!("registry: GPU class key NOT FOUND");
        }
    }

    /// Parse a Wine .reg file and insert keys/values into the registry tree.
    /// Returns (keys_created, values_set).
    fn load_reg_file(&mut self, content: &str, root_prefix: &str) -> (u32, u32) {
        let root_segments: Vec<RegName> = root_prefix.split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| RegName::new(s))
            .collect();

        let mut current_path: Vec<RegName> = Vec::new();
        let mut keys_created = 0u32;
        let mut values_set = 0u32;
        let mut continuation = String::new();

        for line in content.lines() {
            // Handle multi-line values (continuation with backslash)
            if !continuation.is_empty() {
                let trimmed = line.trim();
                if trimmed.ends_with('\\') {
                    continuation.push_str(&trimmed[..trimmed.len()-1]);
                    continue;
                } else {
                    continuation.push_str(trimmed);
                    let full_line = std::mem::take(&mut continuation);
                    if self.parse_reg_value(&full_line, &current_path) {
                        values_set += 1;
                    }
                    continue;
                }
            }

            let trimmed = line.trim();

            // Skip empty lines, comments, metadata
            if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#')
                || trimmed.starts_with("WINE REGISTRY") {
                continue;
            }

            // Key header: [Key\\Path] optional_timestamp
            if trimmed.starts_with('[') {
                if let Some(end) = trimmed.find(']') {
                    let key_path = &trimmed[1..end];
                    // Split on \\ (literal double backslash in the file = single backslash separator)
                    let segments: Vec<RegName> = key_path.split("\\\\")
                        .filter(|s| !s.is_empty())
                        .map(|s| RegName::new(s))
                        .collect();
                    current_path = root_segments.clone();
                    current_path.extend(segments);
                    current_path = self.follow_symlink(current_path);
                    self.walk_mut_create(&current_path);
                    keys_created += 1;
                }
                continue;
            }

            // Value line — may start continuation
            if trimmed.ends_with('\\') {
                continuation = trimmed[..trimmed.len()-1].to_string();
                continue;
            }

            if self.parse_reg_value(trimmed, &current_path) {
                values_set += 1;
            }
        }

        (keys_created, values_set)
    }

    /// Parse a single value line and insert into the registry.
    /// Returns true if a value was set.
    fn parse_reg_value(&mut self, line: &str, current_path: &[RegName]) -> bool {
        if current_path.is_empty() { return false; }

        // Default value: @="value" or @=dword:...
        // Named value: "name"=...
        let (name_str, rest) = if line.starts_with("@=") {
            (String::new(), &line[2..])
        } else if line.starts_with('"') {
            // Find closing quote for name (handle escaped quotes)
            if let Some((name, remainder)) = parse_quoted_name(line) {
                if remainder.starts_with('=') {
                    (name, &remainder[1..])
                } else {
                    return false;
                }
            } else {
                return false;
            }
        } else {
            return false;
        };

        let (data_type, data) = if rest.starts_with('"') {
            // REG_SZ: "value"
            let val = unescape_reg_string(&rest[1..rest.len().saturating_sub(1)]);
            (1u32, str_to_utf16le_null(&val))
        } else if let Some(hex_str) = rest.strip_prefix("dword:") {
            // REG_DWORD: dword:XXXXXXXX
            let val = u32::from_str_radix(hex_str.trim(), 16).unwrap_or(0);
            (4u32, val.to_le_bytes().to_vec())
        } else if let Some(hex_data) = rest.strip_prefix("hex:") {
            // REG_BINARY: hex:XX,XX,...
            (3u32, parse_hex_bytes(hex_data))
        } else if let Some(rest2) = rest.strip_prefix("str(2):") {
            // REG_EXPAND_SZ: str(2):"value"
            let inner = rest2.trim_start_matches('"').trim_end_matches('"');
            let val = unescape_reg_string(inner);
            (2u32, str_to_utf16le_null(&val))
        } else if let Some(rest2) = rest.strip_prefix("str(6):") {
            // REG_LINK: str(6):"value"
            let inner = rest2.trim_start_matches('"').trim_end_matches('"');
            let val = unescape_reg_string(inner);
            // REG_LINK stores UTF-16LE without null terminator
            let u16s: Vec<u16> = val.encode_utf16().collect();
            (6u32, u16s.iter().flat_map(|c| c.to_le_bytes()).collect())
        } else if let Some(hex_rest) = rest.strip_prefix("hex(") {
            // hex(N):XX,XX,...  (e.g. hex(7) for REG_MULTI_SZ)
            if let Some(colon_pos) = hex_rest.find("):") {
                let type_num = u32::from_str_radix(&hex_rest[..colon_pos], 16).unwrap_or(3);
                let hex_data = &hex_rest[colon_pos+2..];
                (type_num, parse_hex_bytes(hex_data))
            } else {
                return false;
            }
        } else {
            return false;
        };

        let vname = RegName::new(&name_str);
        let node = self.walk_mut_create(current_path);
        if !node.values.contains_key(&vname) {
            node.value_names.push(vname.clone());
        }
        node.values.insert(vname, RegistryValue { data_type, data });
        true
    }

    /// Save in-memory registry back to prefix .reg files.
    /// Called on server shutdown so wineboot's registry changes persist.
    pub fn save_to_prefix(&self, user_sid: &str) {
        let prefix = std::env::var("WINEPREFIX")
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").expect("HOME not set");
                format!("{home}/.wine")
            });
        let prefix = std::path::Path::new(&prefix);

        let files: &[(&str, &str)] = &[
            ("system.reg", "Registry\\Machine"),
            ("user.reg", &format!("Registry\\User\\{user_sid}")),
            ("userdef.reg", "Registry\\User\\.Default"),
        ];

        for &(filename, root_prefix) in files {
            let root_segments: Vec<RegName> = root_prefix.split('\\')
                .filter(|s| !s.is_empty())
                .map(|s| RegName::new(s))
                .collect();

            // Walk to the root node for this file
            let mut node = &self.root;
            let mut found = true;
            for seg in &root_segments {
                if let Some(child) = node.children.get(seg) {
                    node = child;
                } else {
                    found = false;
                    break;
                }
            }
            if !found { continue; }

            // Count keys to decide if worth writing
            fn count_keys(node: &RegistryKey) -> u32 {
                let mut c = 1;
                for child in node.children.values() { c += count_keys(child); }
                c
            }
            let key_count = count_keys(node);
            if key_count <= 1 { continue; } // empty subtree

            // Determine the relative root for the file header
            let rel_root = root_prefix.replacen("Registry\\", "REGISTRY\\\\", 1);
            let mut out = String::with_capacity(64 * 1024);
            out.push_str("WINE REGISTRY Version 2\n");
            out.push_str(&format!(";; All keys relative to {rel_root}\n\n"));
            out.push_str("#arch=win64\n\n");

            // Recursively write keys
            fn write_key(node: &RegistryKey, path: &str, out: &mut String) {
                // Skip CurrentControlSet — it's a symlink to ControlSet001.
                // Saving it creates duplicate entries that confuse the loader.
                if path.to_lowercase().contains("currentcontrolset") && path.to_lowercase().contains("services") {
                    return;
                }
                // Write key header
                out.push_str(&format!("[{path}] 1773790329\n"));

                // Write values
                for name in &node.value_names {
                    if let Some(val) = node.values.get(name) {
                        let name_str = if name.original.is_empty() || name.original == "@" {
                            "@".to_string()
                        } else {
                            format!("\"{}\"", name.original.replace('\\', "\\\\").replace('"', "\\\""))
                        };

                        match val.data_type {
                            1 => { // REG_SZ
                                let s = String::from_utf16_lossy(
                                    &val.data.chunks_exact(2)
                                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                        .collect::<Vec<u16>>()
                                ).trim_end_matches('\0').to_string();
                                out.push_str(&format!("{}=\"{}\"\n", name_str,
                                    s.replace('\\', "\\\\").replace('"', "\\\"")));
                            }
                            2 => { // REG_EXPAND_SZ
                                let s = String::from_utf16_lossy(
                                    &val.data.chunks_exact(2)
                                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                                        .collect::<Vec<u16>>()
                                ).trim_end_matches('\0').to_string();
                                out.push_str(&format!("{}=str(2):\"{}\"\n", name_str,
                                    s.replace('\\', "\\\\").replace('"', "\\\"")));
                            }
                            4 => { // REG_DWORD
                                if val.data.len() >= 4 {
                                    let v = u32::from_le_bytes([val.data[0], val.data[1], val.data[2], val.data[3]]);
                                    out.push_str(&format!("{}=dword:{:08x}\n", name_str, v));
                                }
                            }
                            _ => { // REG_BINARY, REG_MULTI_SZ, etc. — hex encoding
                                let hex: String = val.data.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(",");
                                out.push_str(&format!("{}=hex({:x}):{}\n", name_str, val.data_type, hex));
                            }
                        }
                    }
                }
                out.push('\n');

                // Recurse children (sorted for determinism)
                let mut children: Vec<(&RegName, &RegistryKey)> = node.children.iter().collect();
                children.sort_by(|a, b| a.0.lower.cmp(&b.0.lower));
                for (name, child) in children {
                    let child_path = if path.is_empty() {
                        name.original.clone()
                    } else {
                        format!("{path}\\\\{}", name.original)
                    };
                    write_key(child, &child_path, out);
                }
            }

            // Write children of root (not the root itself — Wine doesn't have an empty [] key)
            let mut children: Vec<(&RegName, &RegistryKey)> = node.children.iter().collect();
            children.sort_by(|a, b| a.0.lower.cmp(&b.0.lower));
            for (name, child) in children {
                write_key(child, &name.original, &mut out);
            }

            let path = prefix.join(filename);
            match std::fs::write(&path, &out) {
                Ok(_) => log_info!("registry: saved {filename} ({key_count} keys, {} bytes)", out.len()),
                Err(e) => log_error!("registry: failed to save {filename}: {e}"),
            }
        }
    }

    /// Dump all registry keys to stderr (debug)
    pub fn dump_keys(&self) {
        fn recurse(node: &RegistryKey, prefix: &str, depth: usize) {
            if depth > 12 { return; } // limit depth
            for (name, child) in &node.children {
                let path = if prefix.is_empty() { name.original.clone() } else { format!("{prefix}\\{}", name.original) };
                let val_count = child.values.len();
                if val_count > 0 || depth <= 3 {
                    let _val_names: Vec<&str> = child.values.keys().map(|k| k.original.as_str()).collect();
                }
                recurse(child, &path, depth + 1);
            }
        }
        recurse(&self.root, "", 0);
    }
}

// Parse a UTF-16LE key path (e.g. "\Registry\Machine\Software") into segments.
fn parse_key_path(name_utf16le: &[u8]) -> Vec<RegName> {
    let chars: Vec<u16> = name_utf16le.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let path = String::from_utf16_lossy(&chars);
    path.split('\\')
        .filter(|s| !s.is_empty())
        .map(|s| RegName::new(s))
        .collect()
}

// Parse Wine's object_attributes VARARG to extract the key name.
// Layout: rootdir (u32) + attributes (u32) + sd_len (u32) + name_len (u32) + sd bytes + name bytes
pub fn parse_objattr_name(vararg: &[u8]) -> (u32, &[u8]) {
    if vararg.len() < 16 {
        return (0, &[]);
    }
    let rootdir = u32::from_le_bytes([vararg[0], vararg[1], vararg[2], vararg[3]]);
    let sd_len = u32::from_le_bytes([vararg[8], vararg[9], vararg[10], vararg[11]]) as usize;
    let name_len = u32::from_le_bytes([vararg[12], vararg[13], vararg[14], vararg[15]]) as usize;
    let name_start = 16 + sd_len;
    let name_end = (name_start + name_len).min(vararg.len());
    if name_start <= vararg.len() {
        (rootdir, &vararg[name_start..name_end])
    } else {
        (rootdir, &[])
    }
}

/// Parse the quoted name from a registry value line like `"name"=...`.
/// Returns (unescaped_name, rest_of_line_after_closing_quote).
fn parse_quoted_name(line: &str) -> Option<(String, &str)> {
    // Skip opening quote
    let rest = &line[1..];
    let mut name = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            if let Some((_, esc)) = chars.next() {
                match esc {
                    '\\' => name.push('\\'),
                    '"' => name.push('"'),
                    _ => { name.push('\\'); name.push(esc); }
                }
            }
        } else if c == '"' {
            return Some((name, &rest[i+1..]));
        } else {
            name.push(c);
        }
    }
    None
}

/// Unescape a Wine registry string value (content between quotes).
fn unescape_reg_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('0') => out.push('\0'),
                Some('a') => out.push('\x07'),
                Some('b') => out.push('\x08'),
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('e') => out.push('\x1b'),
                Some(other) => { out.push('\\'); out.push(other); }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Convert a Rust string to UTF-16LE with null terminator (for REG_SZ / REG_EXPAND_SZ).
fn str_to_utf16le_null(s: &str) -> Vec<u8> {
    let mut u16s: Vec<u16> = s.encode_utf16().collect();
    u16s.push(0); // null terminator
    u16s.iter().flat_map(|c| c.to_le_bytes()).collect()
}

/// Parse comma-separated hex bytes like "1f,00,e0,03"
fn parse_hex_bytes(s: &str) -> Vec<u8> {
    s.split(',')
        .filter_map(|b| {
            let b = b.trim();
            if b.is_empty() { return None; }
            u8::from_str_radix(b, 16).ok()
        })
        .collect()
}

impl Registry {
    /// Update display registry keys with a real GPU GUID from PARALLAX.
    /// Creates the Video key under the real GUID and updates DeviceMap to point to it.
    /// Populate the full display device registry chain from PARALLAX data.
    /// Replaces the hardcoded stubs (VEN_0000, Default_Monitor, 256MB VRAM)
    /// with real hardware data. Called from apply_display_data at startup.
    /// Set the display driver in all registry locations.
    /// Called at boot with "x11"/"winex11.drv" default, then again by
    /// apply_display_registry with PARALLAX-detected values.
    pub fn apply_display_driver(&mut self, user_sid: &str, drv_short: &str, drv_dll: &str) {
        // HKCU\Software\Wine\Drivers\Graphics
        let drivers_path = format!("Registry\\User\\{user_sid}\\Software\\Wine\\Drivers");
        let drv_segments: Vec<RegName> = drivers_path.split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| RegName::new(s))
            .collect();
        let drv_node = self.walk_mut_create(&drv_segments);
        let graphics_name = RegName::new("Graphics");
        let graphics_val: Vec<u8> = drv_short.encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .chain(0u16.to_le_bytes())
            .collect();
        if !drv_node.value_names.contains(&graphics_name) {
            drv_node.value_names.push(graphics_name.clone());
        }
        drv_node.values.insert(graphics_name, RegistryValue { data_type: 1, data: graphics_val });

        // HKLM\...\Control\Video\{null GUID}\0000\GraphicsDriver
        let video_path = "Registry\\Machine\\System\\ControlSet001\\Control\\Video\\{00000000-0000-0000-0000-000000000000}\\0000";
        let video_segments: Vec<RegName> = video_path.split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| RegName::new(s))
            .collect();
        let video_node = self.walk_mut_create(&video_segments);
        let gfx_drv_name = RegName::new("GraphicsDriver");
        let gfx_drv_val: Vec<u8> = drv_dll.encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .chain(0u16.to_le_bytes())
            .collect();
        if !video_node.value_names.contains(&gfx_drv_name) {
            video_node.value_names.push(gfx_drv_name.clone());
        }
        video_node.values.insert(gfx_drv_name, RegistryValue { data_type: 1, data: gfx_drv_val });
    }

    pub fn apply_display_registry(&mut self, dd: &crate::display::DisplayData) {
        let gpu = &dd.gpu;
        let guid = dd.gpu_guid();

        // Real PCI enum path: replace VEN_0000&DEV_0000 with actual IDs
        let pci_key = format!(
            "Registry\\Machine\\System\\ControlSet001\\Enum\\PCI\\VEN_{:04X}&DEV_{:04X}&SUBSYS_{:04X}{:04X}&REV_{:02X}\\0000",
            gpu.pci_vendor, gpu.pci_device,
            gpu.pci_subsys_device, gpu.pci_subsys_vendor,
            gpu.pci_revision
        );
        let pci_segments: Vec<RegName> = pci_key.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let pci_node = self.walk_mut_create(&pci_segments);
        set_reg_sz(pci_node, "ClassGUID", "{4d36e968-e325-11ce-bfc1-08002be10318}");
        set_reg_sz(pci_node, "Driver", "{4d36e968-e325-11ce-bfc1-08002be10318}\\0000");

        // VRAM from PARALLAX (detected via sysfs/nvidia-smi in PARALLAX)
        let vram = if gpu.vram_bytes > 0 { (gpu.vram_bytes.min(u32::MAX as u64)) as u32 } else { 0x10000000 };
        let class_path = "Registry\\Machine\\System\\ControlSet001\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\0000";
        let class_segs: Vec<RegName> = class_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let class_node = self.walk_mut_create(&class_segs);
        let mem_name = RegName::new("HardwareInformation.MemorySize");
        class_node.values.insert(mem_name, RegistryValue { data_type: 4, data: vram.to_le_bytes().to_vec() });

        // Override GPU name with real hardware name from PARALLAX
        if !gpu.gpu_name.is_empty() {
            for name in ["DriverDesc", "HardwareInformation.AdapterString",
                         "HardwareInformation.BiosString", "HardwareInformation.ChipType"] {
                set_reg_sz(class_node, name, &gpu.gpu_name);
            }
        }

        // Per-connector: Sources + DefaultSettings + Monitor EDID
        for (i, conn) in dd.connectors.iter().enumerate() {
            // Source entry under Control\Video\{GUID}\Sources\{name}
            let source_path = format!(
                "Registry\\Machine\\System\\ControlSet001\\Control\\Video\\{{{guid}}}\\Sources\\{}",
                conn.name
            );
            let source_segs: Vec<RegName> = source_path.split('\\')
                .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
            let source_node = self.walk_mut_create(&source_segs);
            set_reg_dword(source_node, "DefaultSettings.BitsPerPel", 32);
            set_reg_dword(source_node, "DefaultSettings.XResolution", conn.current_width);
            set_reg_dword(source_node, "DefaultSettings.YResolution", conn.current_height);
            set_reg_dword(source_node, "DefaultSettings.VRefresh", conn.current_refresh);
            set_reg_dword(source_node, "DefaultSettings.Flags", 0);
            let state_flags: u32 = if i == 0 { 0x5 } else { 0x1 };

            // DEVMODEW mode array — Wine's NtUserEnumDisplaySettings reads from here
            let modes_data: Vec<u8> = conn.modes.iter()
                .map(|m| build_devmodew(m.width, m.height, m.refresh, 32))
                .flat_map(|dm| dm.to_vec())
                .collect();
            let current_dm = build_devmodew(conn.current_width, conn.current_height, conn.current_refresh, 32);
            let mode_tail = current_dm[0x48..].to_vec();
            let gpu_path = format!(
                "\\Registry\\Machine\\System\\CurrentControlSet\\Enum\\PCI\\VEN_{:04X}&DEV_{:04X}&SUBSYS_{:04X}{:04X}&REV_{:02X}\\0000",
                dd.gpu.pci_vendor, dd.gpu.pci_device,
                dd.gpu.pci_subsys_device, dd.gpu.pci_subsys_vendor,
                dd.gpu.pci_revision
            );

            // Write mode data to source node
            set_reg_dword(source_node, "ModeCount", conn.modes.len() as u32);
            set_reg_binary(source_node, "Modes", modes_data.clone());
            set_reg_binary(source_node, "Current", mode_tail.clone());
            set_reg_binary(source_node, "Registry", mode_tail.clone());
            set_reg_sz(source_node, "GPUID", &gpu_path);
            set_reg_dword(source_node, "StateFlags", state_flags);
            set_reg_dword(source_node, "Dpi", 96);
            set_reg_dword(source_node, "Depth", 32);

            // Write same data to Hardware Profiles\Current path (Wine reads via config_key)
            let hp_source_path = format!(
                "Registry\\Machine\\System\\CurrentControlSet\\Hardware Profiles\\Current\\System\\CurrentControlSet\\Control\\Video\\{{{guid}}}\\Sources\\{}",
                conn.name
            );
            let hp_segs: Vec<RegName> = hp_source_path.split('\\')
                .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
            let hp_node = self.walk_mut_create(&hp_segs);
            set_reg_dword(hp_node, "ModeCount", conn.modes.len() as u32);
            set_reg_binary(hp_node, "Modes", modes_data);
            set_reg_binary(hp_node, "Current", mode_tail.clone());
            set_reg_binary(hp_node, "Registry", mode_tail);
            set_reg_sz(hp_node, "GPUID", &gpu_path);
            set_reg_dword(hp_node, "StateFlags", state_flags);
            set_reg_dword(hp_node, "Dpi", 96);
            set_reg_dword(hp_node, "Depth", 32);

            // Monitor with EDID
            let mfr = crate::display::DisplayData::edid_manufacturer(&conn.edid);
            let prod = crate::display::DisplayData::edid_product(&conn.edid);
            let mon_path = format!(
                "Registry\\Machine\\System\\ControlSet001\\Enum\\DISPLAY\\{}\\{:04X}&{:04X}",
                mfr, prod, i
            );
            let mon_segs: Vec<RegName> = mon_path.split('\\')
                .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
            let mon_node = self.walk_mut_create(&mon_segs);
            set_reg_sz(mon_node, "ClassGUID", "{4d36e96e-e325-11ce-bfc1-08002be10318}");
            let mon_driver = format!("{{4d36e96e-e325-11ce-bfc1-08002be10318}}\\{i:04}");
            set_reg_sz(mon_node, "Driver", &mon_driver);
            if !conn.edid.is_empty() {
                let edid_name = RegName::new("EDID");
                if !mon_node.values.contains_key(&edid_name) {
                    mon_node.value_names.push(edid_name.clone());
                }
                mon_node.values.insert(edid_name, RegistryValue { data_type: 3, data: conn.edid.clone() }); // REG_BINARY
            }

            // DeviceMap\Video entry for this output
            if i > 0 {
                let devmap_path = "Registry\\Machine\\Hardware\\DeviceMap\\Video";
                let devmap_segs: Vec<RegName> = devmap_path.split('\\')
                    .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
                let devmap_node = self.walk_mut_create(&devmap_segs);
                let dev_name = RegName::new(&format!("\\Device\\Video{i}"));
                let dev_val: Vec<u8> = format!(
                    "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Video\\{{{guid}}}\\Sources\\{}",
                    conn.name
                ).encode_utf16().flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
                devmap_node.value_names.push(dev_name.clone());
                devmap_node.values.insert(dev_name, RegistryValue { data_type: 1, data: dev_val });
            }
        }

        // Apply PARALLAX-detected display driver to all registry locations
        let (drv_short, drv_dll) = dd.display_driver();
        self.apply_display_driver(&self.user_sid.clone(), drv_short, drv_dll);

        log_info!("registry: applied PARALLAX display data — {} connectors, VRAM={vram:#x}, driver={drv_dll}",
            dd.connectors.len());
    }



    pub fn update_display_guid(&mut self, guid: &str, drv_dll: &str) {
        let video_path = format!("Registry\\Machine\\System\\ControlSet001\\Control\\Video\\{{{guid}}}\\0000");
        let video_segments: Vec<RegName> = video_path.split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| RegName::new(s))
            .collect();
        let video_node = self.walk_mut_create(&video_segments);
        let gfx_drv_name = RegName::new("GraphicsDriver");
        let gfx_drv_val: Vec<u8> = drv_dll.encode_utf16()
            .flat_map(|c| c.to_le_bytes())
            .chain(0u16.to_le_bytes())
            .collect();
        if !video_node.values.contains_key(&gfx_drv_name) {
            video_node.value_names.push(gfx_drv_name.clone());
        }
        video_node.values.insert(gfx_drv_name, RegistryValue { data_type: 1, data: gfx_drv_val });

        // Update DeviceMap\Video to point to the real GUID path
        let devmap_path = "Registry\\Machine\\Hardware\\DeviceMap\\Video";
        let devmap_segments: Vec<RegName> = devmap_path.split('\\')
            .filter(|s| !s.is_empty()).map(|s| RegName::new(s)).collect();
        let devmap_node = self.walk_mut_create(&devmap_segments);
        let dev_video_name = RegName::new("\\Device\\Video0");
        let dev_video_val: Vec<u8> = format!("\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Video\\{{{guid}}}\\0000")
            .encode_utf16().flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
        devmap_node.values.insert(dev_video_name, RegistryValue { data_type: 1, data: dev_video_val });
    }
}

fn set_reg_sz(node: &mut RegistryKey, name: &str, val: &str) {
    let rn = RegName::new(name);
    let data: Vec<u8> = val.encode_utf16()
        .flat_map(|c| c.to_le_bytes()).chain(0u16.to_le_bytes()).collect();
    if !node.values.contains_key(&rn) {
        node.value_names.push(rn.clone());
    }
    node.values.insert(rn, RegistryValue { data_type: 1, data });
}

fn set_reg_dword(node: &mut RegistryKey, name: &str, val: u32) {
    let rn = RegName::new(name);
    if !node.values.contains_key(&rn) {
        node.value_names.push(rn.clone());
    }
    node.values.insert(rn, RegistryValue { data_type: 4, data: val.to_le_bytes().to_vec() });
}

fn set_reg_binary(node: &mut RegistryKey, name: &str, data: Vec<u8>) {
    let rn = RegName::new(name);
    if !node.values.contains_key(&rn) {
        node.value_names.push(rn.clone());
    }
    node.values.insert(rn, RegistryValue { data_type: 3, data }); // REG_BINARY
}

/// Build a 188-byte DEVMODEW struct for a display mode.
fn build_devmodew(width: u32, height: u32, refresh: u32, bpp: u32) -> [u8; 188] {
    let mut dm = [0u8; 188];
    // dmSpecVersion at 0x40, dmDriverVersion at 0x42
    dm[0x40..0x42].copy_from_slice(&0x0401u16.to_le_bytes());
    dm[0x42..0x44].copy_from_slice(&0x0401u16.to_le_bytes());
    // dmSize at 0x44
    dm[0x44..0x46].copy_from_slice(&188u16.to_le_bytes());
    // dmFields at 0x48: DM_DISPLAYORIENTATION|DM_BITSPERPEL|DM_PELSWIDTH|DM_PELSHEIGHT|DM_DISPLAYFREQUENCY
    let fields: u32 = 0x1 | 0x100 | 0x80000 | 0x100000 | 0x400000;
    dm[0x48..0x4C].copy_from_slice(&fields.to_le_bytes());
    // dmBitsPerPel at 0x6C
    dm[0x6C..0x70].copy_from_slice(&bpp.to_le_bytes());
    // dmPelsWidth at 0x70
    dm[0x70..0x74].copy_from_slice(&width.to_le_bytes());
    // dmPelsHeight at 0x74
    dm[0x74..0x78].copy_from_slice(&height.to_le_bytes());
    // dmDisplayFrequency at 0x7C
    dm[0x7C..0x80].copy_from_slice(&refresh.to_le_bytes());
    dm
}

/// Convert UTF-16LE bytes to a String (for diagnostics).
pub fn utf16le_to_string_pub(data: &[u8]) -> String {
    let chars: Vec<u16> = data.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&chars)
}
