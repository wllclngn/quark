// Protocol remap layer.
//
// Triskelion is built against a specific Wine source tree (cloned by
// install.py via step_sync_wine_source). build.rs reads SERVER_PROTOCOL_VERSION
// from that tree's include/wine/server_protocol.h and bakes it into
// COMPILED_PROTOCOL_VERSION. The system Wine binaries are also taken from
// that same tree (via step_deploy_wine + step_patch_wine), so both ends of
// the wineserver handshake speak the same protocol by construction.
//
// This file used to contain a runtime "detect the wine version by scanning
// for `MOV EDX, imm32` near a string ref" heuristic that was wrong on x86_64
// SysV ABI (the second printf argument is in RSI / `MOV ESI` 0xBE, not RDX /
// `MOV EDX` 0xBA), so it returned plausible-looking integers from unrelated
// code. That made triskelion announce the wrong version to clients and broke
// the handshake at the first byte. The detection is gone; we trust the build
// constant.

use crate::protocol::RequestCode;

/// Runtime opcode remap table.
pub struct ProtocolRemap {
    /// Protocol version to send during handshake.
    pub version: u32,
    /// Client opcode number → our RequestCode. None = opcode exists in client
    /// but not in our build (e.g. esync opcodes in Proton that we don't have).
    remap: Vec<Option<RequestCode>>,
    /// Whether this is an identity mapping (no remapping needed).
    pub is_identity: bool,
}

impl ProtocolRemap {
    /// Identity mapping — client protocol matches our compiled protocol exactly.
    /// This is the only mapping we ever produce now that build.rs derives the
    /// protocol from the same wine-src that install.py clones.
    pub fn identity() -> Self {
        let count = crate::protocol::OPCODE_META.len();
        let remap: Vec<Option<RequestCode>> = (0..count as i32)
            .map(|i| RequestCode::from_i32(i))
            .collect();

        Self {
            version: crate::ipc::COMPILED_PROTOCOL_VERSION,
            remap,
            is_identity: true,
        }
    }

    /// Resolve a client's opcode number to our RequestCode.
    #[inline]
    pub fn resolve(&self, client_opcode: i32) -> Option<RequestCode> {
        if client_opcode < 0 {
            return None;
        }
        self.remap.get(client_opcode as usize).copied().flatten()
    }
}

/// Build the protocol remap table at daemon startup.
///
/// Returns identity always — both ends of the handshake are produced from
/// the same wine-src tree by install.py + build.rs, so no remap is needed.
pub fn detect_and_remap() -> ProtocolRemap {
    let compiled = crate::ipc::COMPILED_PROTOCOL_VERSION;
    log_info!("protocol: using build version {compiled} (build.rs derived from wine-src)");
    ProtocolRemap::identity()
}
