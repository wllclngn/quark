// In-memory registry tree for triskelion
//
// Empty registry — all reads of uncreated keys return STATUS_OBJECT_NAME_NOT_FOUND.
// Wine/Godot does ~1.2K registry misses at startup which is expected behavior.
// Keys are created on demand by Wine's registry initialization.

use std::collections::HashMap;

// Case-insensitive key name (registry keys are case-insensitive in Windows)
#[derive(Clone, Hash, Eq, PartialEq)]
struct RegName(String); // stored lowercase

impl RegName {
    fn new(s: &str) -> Self {
        RegName(s.to_lowercase())
    }

    fn from_utf16le(bytes: &[u8]) -> Self {
        let chars: Vec<u16> = bytes.chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16_lossy(&chars);
        RegName(s.to_lowercase())
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

pub struct Registry {
    // Root keys: HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER, etc.
    // Wine uses path strings like "\Registry\Machine\..." — we store the root node.
    root: RegistryKey,
    // Map from open handle → path segments
    open_keys: HashMap<u32, Vec<RegName>>,
    next_hkey: u32,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            root: RegistryKey::new(),
            open_keys: HashMap::new(),
            next_hkey: 1,
        }
    }

    // Navigate to a key by path segments, optionally creating along the way.
    fn walk(&self, path: &[RegName]) -> Option<&RegistryKey> {
        let mut node = &self.root;
        for seg in path {
            node = node.children.get(seg)?;
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
        let mut path = self.resolve_parent(parent);
        let segments = parse_key_path(name);
        path.extend(segments);

        let existed = self.walk(&path).is_some();
        self.walk_mut_create(&path);

        let hkey = self.next_hkey;
        self.next_hkey += 1;
        self.open_keys.insert(hkey, path);
        (hkey, !existed)
    }

    // Open an existing key. Returns hkey or None.
    pub fn open_key(&mut self, parent: u32, name: &[u8]) -> Option<u32> {
        let mut path = self.resolve_parent(parent);
        let segments = parse_key_path(name);
        path.extend(segments);

        if self.walk(&path).is_some() {
            let hkey = self.next_hkey;
            self.next_hkey += 1;
            self.open_keys.insert(hkey, path);
            Some(hkey)
        } else {
            None
        }
    }

    // Get a value by name. Returns (type, data) or None.
    pub fn get_value(&self, hkey: u32, name: &[u8]) -> Option<(u32, &[u8])> {
        let path = self.open_keys.get(&hkey)?;
        let node = self.walk(path)?;
        let vname = RegName::from_utf16le(name);
        node.values.get(&vname)
            .map(|v| (v.data_type, v.data.as_slice()))
    }

    // Set a value.
    pub fn set_value(&mut self, hkey: u32, name: &[u8], data_type: u32, data: &[u8]) {
        let path = if let Some(p) = self.open_keys.get(&hkey) {
            p.clone()
        } else {
            return;
        };
        let node = self.walk_mut_create(&path);
        let vname = RegName::from_utf16le(name);
        let val = RegistryValue { data_type, data: data.to_vec() };

        if !node.values.contains_key(&vname) {
            node.value_names.push(vname.clone());
        }
        node.values.insert(vname, val);
    }

    // Enumerate value at index. Returns (name_utf16le, type, data) or None.
    pub fn enum_value(&self, hkey: u32, index: usize) -> Option<(Vec<u8>, u32, &[u8])> {
        let path = self.open_keys.get(&hkey)?;
        let node = self.walk(path)?;
        let name = node.value_names.get(index)?;
        let val = node.values.get(name)?;
        // Convert name back to UTF-16LE
        let name_u16: Vec<u16> = name.0.encode_utf16().collect();
        let name_bytes: Vec<u8> = name_u16.iter()
            .flat_map(|c| c.to_le_bytes())
            .collect();
        Some((name_bytes, val.data_type, val.data.as_slice()))
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
