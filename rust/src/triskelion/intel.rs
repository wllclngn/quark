// Runtime protocol intelligence — learns from every game run.
//
// v2 format: unified .quark_cache (10,496 bytes)
//   Section 1: Header (64 B)          — magic, version, appid, engine, flags
//   Section 2: Engine Profile (128 B)  — written by install.py, read-only here
//   Section 3: Opcode Intel (4,896 B)  — hints seeded by install.py, read here
//   Section 4: Learned Data (4,896 B)  — written by Rust on shutdown
//   Section 5: Shader Index (512 B)    — written by install.py
//
// v1 format: .triskelion_learned (4,912 bytes) — auto-migrated on first load

use std::path::PathBuf;

const OPCODE_COUNT: usize = 306;

// ── v2 format ────────────────────────────────────────────────────────────────

const CACHE_MAGIC: [u8; 4] = *b"AMPC";
const CACHE_VERSION: u32 = 3;
const CACHE_V2_SIZE: usize = 10_496;
// v3 adds Section 6: Message routing profiles (4096 bytes = 256 entries x 16 bytes)
const CACHE_SIZE: usize = 14_592;

// Section offsets (64 + 128 + 4896 + 4896 + 512 + 4096 = 14592)
const OPCODE_INTEL_OFF: usize = 0x00C0;    // 192
const LEARNED_OFF: usize = 0x13E0;         // 5088
const MSG_PROFILE_OFF: usize = 0x2900;     // 10496 (start of section 6)
const MSG_PROFILE_ENTRY: usize = 16;
const MAX_MSG_PROFILES: usize = 256;

// Section sizes
const HEADER_SIZE: usize = 64;
const OPCODE_INTEL_ENTRY: usize = 16;
const LEARNED_ENTRY: usize = 16;

// Hint values (from engine profiles, seeded by install.py)
const HINT_UNKNOWN: u32 = 0;
const HINT_CRITICAL: u32 = 1;
const HINT_SAFE_TO_STUB: u32 = 3;
const HINT_NEVER_CALLED: u32 = 4;

// Header flags
const FLAG_HAS_LEARNED: u32 = 0x04;
const FLAG_STABLE: u32 = 0x10;

// ── v1 format (migration) ───────────────────────────────────────────────────

const V1_MAGIC: [u8; 4] = *b"AMPL";
const V1_HEADER_SIZE: usize = 16;
const V1_ENTRY_SIZE: usize = 16;
const V1_FILE_SIZE: usize = V1_HEADER_SIZE + OPCODE_COUNT * V1_ENTRY_SIZE;

// Stability states (shared between v1 and v2)
const STABILITY_UNKNOWN: u8 = 0;
const STABILITY_STABLE: u8 = 1;
const STABILITY_UNSTABLE: u8 = 2;

// Thresholds for stability classification
const STABLE_THRESHOLD: u32 = 10;
const UNSTABLE_THRESHOLD: u32 = 3;

// Compile-time sanity
const _: () = assert!(HEADER_SIZE == 64);
const _: () = assert!(OPCODE_INTEL_ENTRY == 16);
const _: () = assert!(LEARNED_ENTRY == 16);
const _: () = assert!(CACHE_SIZE == 14_592);

#[derive(Clone, Copy)]
struct OpcodeEntry {
    call_count: u32,
    stub_count: u32,
    stability: u8,
}

pub struct IntelManager {
    cache_path: Option<PathBuf>,
    // Prior data (loaded from file)
    prior: [OpcodeEntry; OPCODE_COUNT],
    prior_run_count: u32,
    // Engine hints (from opcode intel section, seeded by install.py)
    hints: [u32; OPCODE_COUNT],
    engine_type: u32,
    cache_flags: u32,
    // Preserved sections (engine profile + opcode intel + shader index)
    // We read-modify-write: only update header + learned data on flush
    preserved_buf: Option<Vec<u8>>,
    // Loaded message routing profiles (seeded into sent_messages at startup)
    loaded_msg_profiles: Vec<(u32, crate::sent_messages::MsgProfile)>,
    // This-run tracking
    run_calls: [u32; OPCODE_COUNT],
    run_stubs: [u32; OPCODE_COUNT],
    post_stub_requests: [u32; OPCODE_COUNT],
    total_requests: u64,
}

impl IntelManager {
    pub fn new() -> Self {
        let cache_path = Self::resolve_cache_path();

        let mut mgr = Self {
            cache_path,
            prior: [OpcodeEntry { call_count: 0, stub_count: 0, stability: STABILITY_UNKNOWN }; OPCODE_COUNT],
            prior_run_count: 0,
            hints: [HINT_UNKNOWN; OPCODE_COUNT],
            engine_type: 0,
            cache_flags: 0,
            preserved_buf: None,
            loaded_msg_profiles: Vec::new(),
            run_calls: [0; OPCODE_COUNT],
            run_stubs: [0; OPCODE_COUNT],
            post_stub_requests: [0; OPCODE_COUNT],
            total_requests: 0,
        };

        mgr.load();
        mgr
    }

    /// Record that an opcode was dispatched.
    #[inline]
    pub fn record_call(&mut self, opcode: usize) {
        if opcode < OPCODE_COUNT {
            self.run_calls[opcode] += 1;
            self.total_requests += 1;

            // Advance post-stub counters for ALL previously-stubbed opcodes
            for i in 0..OPCODE_COUNT {
                if self.run_stubs[i] > 0 && self.post_stub_requests[i] < STABLE_THRESHOLD + 1 {
                    self.post_stub_requests[i] += 1;
                }
            }
        }
    }

    /// Record that an opcode was auto-stubbed.
    #[inline]
    pub fn record_stub(&mut self, opcode: usize) {
        if opcode < OPCODE_COUNT {
            self.run_stubs[opcode] += 1;
            self.post_stub_requests[opcode] = 0;
        }
    }

    /// Should the auto-stub engine replace STATUS_NOT_IMPLEMENTED with success?
    /// Combines engine hints (from install.py) with learned stability data.
    #[inline]
    pub fn should_auto_stub(&self, opcode: usize, has_vararg_reply: bool) -> bool {
        // NEVER stub vararg replies — Wine trusts the data and skips fallback
        if has_vararg_reply { return false; }
        if opcode >= OPCODE_COUNT { return false; }

        match self.hints[opcode] {
            HINT_CRITICAL => false,
            HINT_SAFE_TO_STUB | HINT_NEVER_CALLED => true,
            _ => {
                // HINT_UNKNOWN or HINT_NEEDED: use learned stability
                // Unknown stability (new opcode) → try stubbing (current behavior)
                self.prior[opcode].stability != STABILITY_UNSTABLE
            }
        }
    }

    /// Flush learned data to disk. Call on daemon shutdown.
    /// Pass message routing profiles from sent_messages for persistence.
    pub fn flush(&self, msg_profiles: &[(u32, crate::sent_messages::MsgProfile)]) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // Start with existing cache or fresh buffer
        let mut buf = if let Some(ref preserved) = self.preserved_buf {
            preserved.clone()
        } else {
            vec![0u8; CACHE_SIZE]
        };

        // Ensure correct size
        buf.resize(CACHE_SIZE, 0);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);

        let run_count = self.prior_run_count + 1;

        // Update header (preserve install_epoch, engine_type from install.py)
        buf[0..4].copy_from_slice(&CACHE_MAGIC);
        buf[4..8].copy_from_slice(&CACHE_VERSION.to_le_bytes());
        // appid at offset 8 — preserve from existing
        let mut flags = self.cache_flags | FLAG_HAS_LEARNED;
        // Check if stable (5+ runs)
        if run_count >= 5 {
            flags |= FLAG_STABLE;
        }
        buf[12..16].copy_from_slice(&flags.to_le_bytes());
        // engine_type at offset 16 — preserve from existing
        buf[20..24].copy_from_slice(&run_count.to_le_bytes());
        buf[24..28].copy_from_slice(&now.to_le_bytes());
        // install_epoch at offset 28 — preserve from existing
        // Compute opcode coverage
        let known = self.prior.iter().enumerate()
            .filter(|(i, e)| e.call_count.saturating_add(self.run_calls[*i]) > 0)
            .count();
        let coverage = (known as u32 * 100) / OPCODE_COUNT as u32;
        buf[32..36].copy_from_slice(&coverage.to_le_bytes());

        // Write learned data section (section 4 @ LEARNED_OFF)
        for i in 0..OPCODE_COUNT {
            let stability = if self.run_stubs[i] > 0 {
                if self.post_stub_requests[i] >= STABLE_THRESHOLD {
                    STABILITY_STABLE
                } else if self.post_stub_requests[i] <= UNSTABLE_THRESHOLD {
                    if self.prior[i].stability == STABILITY_STABLE {
                        STABILITY_STABLE // don't downgrade on one bad run
                    } else {
                        STABILITY_UNSTABLE
                    }
                } else {
                    self.prior[i].stability
                }
            } else {
                self.prior[i].stability
            };

            let off = LEARNED_OFF + i * LEARNED_ENTRY;
            let call_count = self.prior[i].call_count.saturating_add(self.run_calls[i]);
            let stub_count = self.prior[i].stub_count.saturating_add(self.run_stubs[i]);

            buf[off..off + 4].copy_from_slice(&call_count.to_le_bytes());
            buf[off + 4..off + 8].copy_from_slice(&stub_count.to_le_bytes());
            buf[off + 8] = stability;
            // bytes 9-11: pad
            // bytes 12-15: last_run_calls (this session)
            buf[off + 12..off + 16].copy_from_slice(&self.run_calls[i].to_le_bytes());
        }

        // Write message routing profiles (section 6 @ MSG_PROFILE_OFF)
        let profile_count = msg_profiles.len().min(MAX_MSG_PROFILES);
        for (i, (msg_code, profile)) in msg_profiles.iter().take(MAX_MSG_PROFILES).enumerate() {
            let off = MSG_PROFILE_OFF + i * MSG_PROFILE_ENTRY;
            buf[off..off + 4].copy_from_slice(&msg_code.to_le_bytes());
            buf[off + 4..off + 8].copy_from_slice(&profile.fast_votes.to_le_bytes());
            buf[off + 8..off + 12].copy_from_slice(&profile.tracked_votes.to_le_bytes());
            let flags: u32 = profile.observations | if profile.promoted { 0x80000000 } else { 0 };
            buf[off + 12..off + 16].copy_from_slice(&flags.to_le_bytes());
        }
        // Store profile count in header at offset 36 (was unused padding)
        buf[36..40].copy_from_slice(&(profile_count as u32).to_le_bytes());

        if let Err(e) = std::fs::write(path, &buf) {
            log_error!("intel: failed to write {}: {e}", path.display());
        } else {
            log_info!("intel: saved cache to {} (run #{run_count}, {coverage}% coverage, {profile_count} msg profiles)",
                path.display());
        }
    }

    /// Take loaded message profiles to seed sent_messages at startup.
    pub fn take_msg_profiles(&mut self) -> Vec<(u32, crate::sent_messages::MsgProfile)> {
        std::mem::take(&mut self.loaded_msg_profiles)
    }

    /// Print a summary of loaded intelligence.
    pub fn log_summary(&self) {
        let total_prior_stubs: u32 = self.prior.iter().map(|e| e.stub_count).sum();
        let stable_count = self.prior.iter().filter(|e| e.stability == STABILITY_STABLE).count();
        let unstable_count = self.prior.iter().filter(|e| e.stability == STABILITY_UNSTABLE).count();
        let known_count = self.prior.iter().filter(|e| e.call_count > 0).count();
        let hinted = self.hints.iter().filter(|&&h| h != HINT_UNKNOWN).count();

        if self.prior_run_count > 0 {
            log_info!("intel: loaded: {} prior runs | {} opcodes seen | {} stubs ({} stable, {} unstable) | {} hints",
                self.prior_run_count, known_count, total_prior_stubs, stable_count, unstable_count, hinted);
        } else if hinted > 0 {
            log_info!("intel: first run — {hinted} engine hints loaded, no prior runtime data");
        } else {
            log_info!("intel: cold start — no prior data, no engine hints");
        }
    }

    // ── Private ──────────────────────────────────────────────────────────────

    fn resolve_cache_path() -> Option<PathBuf> {
        let prefix = std::env::var("WINEPREFIX").ok()?;
        let pfx = std::path::Path::new(&prefix);
        let compat_dir = pfx.parent()?;
        Some(compat_dir.join(".quark_cache"))
    }

    fn load(&mut self) {
        let path = match &self.cache_path {
            Some(p) => p.clone(),
            None => return,
        };

        // Try v2/v3 cache
        if let Ok(data) = std::fs::read(&path) {
            if data.len() >= CACHE_V2_SIZE && data[0..4] == CACHE_MAGIC {
                let version = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                if version == 2 || version == 3 {
                    self.load_v2(&data);
                    // v3: also load message routing profiles (section 6)
                    if data.len() >= CACHE_SIZE {
                        self.load_msg_profiles(&data);
                    }
                    return;
                }
            }
        }

        // Try v1 migration (.triskelion_learned in same dir)
        let v1_path = path.with_file_name(".triskelion_learned");
        if let Ok(data) = std::fs::read(&v1_path) {
            if data.len() >= V1_FILE_SIZE && data[0..4] == V1_MAGIC {
                self.load_v1(&data);
                log_info!("intel: migrated v1 .triskelion_learned → v2 .quark_cache");
            }
        }
    }

    fn load_v2(&mut self, data: &[u8]) {
        // Preserve full buffer for read-modify-write on flush.
        // v2 files are smaller than v3; take what exists, flush() will resize.
        self.preserved_buf = Some(data[..data.len().min(CACHE_SIZE)].to_vec());

        // Header
        self.cache_flags = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
        self.engine_type = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
        self.prior_run_count = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);

        // Opcode intel hints (section 3)
        for i in 0..OPCODE_COUNT {
            let off = OPCODE_INTEL_OFF + i * OPCODE_INTEL_ENTRY;
            self.hints[i] = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        }

        // Learned data (section 4)
        for i in 0..OPCODE_COUNT {
            let off = LEARNED_OFF + i * LEARNED_ENTRY;
            self.prior[i] = OpcodeEntry {
                call_count: u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]),
                stub_count: u32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]),
                stability: data[off + 8],
            };
        }
    }

    fn load_v1(&mut self, data: &[u8]) {
        self.prior_run_count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);

        for i in 0..OPCODE_COUNT {
            let off = V1_HEADER_SIZE + i * V1_ENTRY_SIZE;
            self.prior[i] = OpcodeEntry {
                call_count: u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]),
                stub_count: u32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]),
                stability: data[off + 8],
            };
        }
        // No hints from v1 — all HINT_UNKNOWN (default)
    }

    fn load_msg_profiles(&mut self, data: &[u8]) {
        let profile_count = u32::from_le_bytes([data[36], data[37], data[38], data[39]]) as usize;
        let count = profile_count.min(MAX_MSG_PROFILES);
        for i in 0..count {
            let off = MSG_PROFILE_OFF + i * MSG_PROFILE_ENTRY;
            if off + MSG_PROFILE_ENTRY > data.len() { break; }
            let msg_code = u32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]]);
            let fast_votes = u32::from_le_bytes([data[off+4], data[off+5], data[off+6], data[off+7]]);
            let tracked_votes = u32::from_le_bytes([data[off+8], data[off+9], data[off+10], data[off+11]]);
            let flags = u32::from_le_bytes([data[off+12], data[off+13], data[off+14], data[off+15]]);
            let observations = flags & 0x7FFFFFFF;
            let promoted = flags & 0x80000000 != 0;
            if msg_code != 0 || fast_votes != 0 || tracked_votes != 0 {
                self.loaded_msg_profiles.push((msg_code, crate::sent_messages::MsgProfile {
                    fast_votes, tracked_votes, observations, promoted,
                }));
            }
        }
        if !self.loaded_msg_profiles.is_empty() {
            log_info!("intel: loaded {} message routing profiles", self.loaded_msg_profiles.len());
        }
    }

    /// Create an IntelManager with no file backing (for tests).
    #[cfg(test)]
    fn new_test() -> Self {
        Self {
            cache_path: None,
            prior: [OpcodeEntry { call_count: 0, stub_count: 0, stability: STABILITY_UNKNOWN }; OPCODE_COUNT],
            prior_run_count: 0,
            hints: [HINT_UNKNOWN; OPCODE_COUNT],
            engine_type: 0,
            cache_flags: 0,
            preserved_buf: None,
            loaded_msg_profiles: Vec::new(),
            run_calls: [0; OPCODE_COUNT],
            run_stubs: [0; OPCODE_COUNT],
            post_stub_requests: [0; OPCODE_COUNT],
            total_requests: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Cache format ────────────────────────────────────────────────────────

    #[test]
    fn v2_round_trip() {
        // Build a cache buffer with known data
        let mut buf = vec![0u8; CACHE_SIZE];
        buf[0..4].copy_from_slice(&CACHE_MAGIC);
        buf[4..8].copy_from_slice(&CACHE_VERSION.to_le_bytes());
        buf[12..16].copy_from_slice(&(FLAG_HAS_LEARNED).to_le_bytes());
        buf[16..20].copy_from_slice(&42u32.to_le_bytes()); // engine_type
        buf[20..24].copy_from_slice(&7u32.to_le_bytes());  // run_count

        // Set hint for opcode 5 = HINT_SAFE_TO_STUB
        let hint_off = OPCODE_INTEL_OFF + 5 * OPCODE_INTEL_ENTRY;
        buf[hint_off..hint_off + 4].copy_from_slice(&HINT_SAFE_TO_STUB.to_le_bytes());

        // Set learned data for opcode 10: call_count=100, stub_count=50, stability=STABLE
        let learn_off = LEARNED_OFF + 10 * LEARNED_ENTRY;
        buf[learn_off..learn_off + 4].copy_from_slice(&100u32.to_le_bytes());
        buf[learn_off + 4..learn_off + 8].copy_from_slice(&50u32.to_le_bytes());
        buf[learn_off + 8] = STABILITY_STABLE;

        // Load it
        let mut mgr = IntelManager::new_test();
        mgr.load_v2(&buf);

        assert_eq!(mgr.engine_type, 42);
        assert_eq!(mgr.prior_run_count, 7);
        assert_eq!(mgr.cache_flags, FLAG_HAS_LEARNED);
        assert_eq!(mgr.hints[5], HINT_SAFE_TO_STUB);
        assert_eq!(mgr.hints[0], HINT_UNKNOWN);
        assert_eq!(mgr.prior[10].call_count, 100);
        assert_eq!(mgr.prior[10].stub_count, 50);
        assert_eq!(mgr.prior[10].stability, STABILITY_STABLE);
    }

    #[test]
    fn v1_migration_loads_prior_data() {
        let mut buf = vec![0u8; V1_FILE_SIZE];
        buf[0..4].copy_from_slice(&V1_MAGIC);
        buf[4..8].copy_from_slice(&1u32.to_le_bytes()); // version
        buf[8..12].copy_from_slice(&3u32.to_le_bytes()); // run_count

        // Set opcode 20: call_count=55, stub_count=10, stability=UNSTABLE
        let off = V1_HEADER_SIZE + 20 * V1_ENTRY_SIZE;
        buf[off..off + 4].copy_from_slice(&55u32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&10u32.to_le_bytes());
        buf[off + 8] = STABILITY_UNSTABLE;

        let mut mgr = IntelManager::new_test();
        mgr.load_v1(&buf);

        assert_eq!(mgr.prior_run_count, 3);
        assert_eq!(mgr.prior[20].call_count, 55);
        assert_eq!(mgr.prior[20].stub_count, 10);
        assert_eq!(mgr.prior[20].stability, STABILITY_UNSTABLE);
        // v1 has no hints
        assert_eq!(mgr.hints[20], HINT_UNKNOWN);
    }

    // ── Auto-stub logic ─────────────────────────────────────────────────────

    #[test]
    fn never_stub_vararg_replies() {
        let mut mgr = IntelManager::new_test();
        mgr.hints[5] = HINT_SAFE_TO_STUB;
        assert!(!mgr.should_auto_stub(5, true));
    }

    #[test]
    fn stub_safe_to_stub_hint() {
        let mut mgr = IntelManager::new_test();
        mgr.hints[5] = HINT_SAFE_TO_STUB;
        assert!(mgr.should_auto_stub(5, false));
    }

    #[test]
    fn stub_never_called_hint() {
        let mut mgr = IntelManager::new_test();
        mgr.hints[5] = HINT_NEVER_CALLED;
        assert!(mgr.should_auto_stub(5, false));
    }

    #[test]
    fn never_stub_critical_hint() {
        let mut mgr = IntelManager::new_test();
        mgr.hints[5] = HINT_CRITICAL;
        assert!(!mgr.should_auto_stub(5, false));
    }

    #[test]
    fn unknown_hint_stubs_unless_unstable() {
        let mut mgr = IntelManager::new_test();
        // Unknown hint, unknown stability → stub
        assert!(mgr.should_auto_stub(5, false));

        // Unknown hint, stable → stub
        mgr.prior[5].stability = STABILITY_STABLE;
        assert!(mgr.should_auto_stub(5, false));

        // Unknown hint, unstable → don't stub
        mgr.prior[5].stability = STABILITY_UNSTABLE;
        assert!(!mgr.should_auto_stub(5, false));
    }

    #[test]
    fn record_call_increments() {
        let mut mgr = IntelManager::new_test();
        mgr.record_call(10);
        mgr.record_call(10);
        mgr.record_call(10);
        assert_eq!(mgr.run_calls[10], 3);
        assert_eq!(mgr.total_requests, 3);
    }

    #[test]
    fn record_stub_resets_post_stub() {
        let mut mgr = IntelManager::new_test();
        mgr.record_call(5);
        mgr.record_call(5);
        mgr.record_stub(5);
        // post_stub_requests should be reset to 0 after stub
        assert_eq!(mgr.post_stub_requests[5], 0);
        assert_eq!(mgr.run_stubs[5], 1);
    }

}
