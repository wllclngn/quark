// Gaming-critical DLLs and programs.
// Migrated from amphetamine_common.py.
//
// Triage methodology (2026-02-27):
//   Layer 1: DLLs games load directly (~30)
//   Layer 2: DLLs pulled in by layer 1 via IMPORTS/DELAYIMPORTS
//   Layer 3: DLLs pulled in by layer 2
//   Zero-LOC entries are UUID/GUID libs or forwarder stubs -- kept because
//   the linker needs them even though they contain no real code.

pub const GAMING_DLLS: &[&str] = &[
    // Core chain
    "ntdll", "kernelbase", "kernel32", "advapi32", "sechost",
    "apisetschema",
    // Windowing chain
    "win32u", "gdi32", "user32", "opengl32",
    // Display drivers
    "winex11.drv", "winewayland.drv",
    // Vulkan
    "winevulkan",
    // Direct3D entry points (DXVK/vkd3d-proton override these, stubs needed)
    "d3d8", "d3d9", "d3d10", "d3d10_1", "d3d10core",
    "d3d11", "d3d12", "d3d12core", "dxgi", "wined3d",
    // Input
    "xinput1_3", "xinput1_4", "xinput9_1_0",
    "dinput", "dinput8",
    "hid", "hidclass.sys", "hidparse.sys",
    // Audio
    "winmm", "mmdevapi", "winepulse.drv", "winealsa.drv",
    // Networking
    "ws2_32", "iphlpapi",
    // Crypto (DRM, launchers, online services)
    "bcrypt", "crypt32", "secur32",
    // Device enumeration
    "setupapi", "cfgmgr32",
    // Media (cutscenes)
    "winegstreamer",
    // Layer 2: support DLLs (pulled in by layer 1 via imports/delayimports)
    "ole32", "oleaut32", "rpcrt4", "comctl32", "imm32",
    "msacm32", "dnsapi", "nsi", "strmbase", "msdmo",
    "mfplat", "mf", "version", "cryptnet", "combase",
    // C runtime
    "msvcrt", "ucrtbase", "winecrt0",
    // Additional commonly needed
    "shell32", "shlwapi", "cabinet", "wintrust",
    "comdlg32", "glu32", "dbghelp", "winspool.drv",
    // Layer 3: resolved surprise dependencies (2026-02-27 triage)
    "uxtheme", "urlmon", "wininet", "propsys", "rtworkq",
    "coml2", "mpr", "shcore", "userenv", "imagehlp",
    "netapi32", "evr", "mlang", "windowscodecs", "gdiplus",
    "oleacc", "cryptui", "compstui",
    // Zero-LOC stubs/forwarders/UUID libs (needed by linker)
    "strmiids", "mfuuid", "dmoguids", "hidparse",
    // Kernel driver stubs for HID stack
    "ntoskrnl.exe", "hal",
    // Misc required
    "d3dcompiler_39", "shdocvw", "msimg32", "dxva2", "netutils",
    // Type libraries (needed by widl during build)
    "stdole2.tlb", "stdole32.tlb",
];

pub const GAMING_PROGRAMS: &[&str] = &[
    // Wine runtime (prefix init, services, drivers)
    "wineboot", "winedevice", "explorer", "services",
    "rpcss", "plugplay", "svchost", "conhost",
    // Process/DLL launching (games call these)
    "rundll32", "dllhost", "start", "cmd",
    // Installation/registration (game setup, DRM)
    "msiexec", "regsvr32", "reg", "regedit",
    // Game-facing utilities
    "taskkill", "sc", "dxdiag",
    // Wine tools (development/debugging/configuration)
    "winedbg", "winecfg", "winepath", "wineconsole",
    "winemenubuilder", "winebrowser",
    // Proton needs
    "uninstaller",
];

// Infra DLLs that don't count as "surprise" dependencies
pub const INFRA_DLLS: &[&str] = &["winecrt0", "uuid", "dxguid"];

pub const WINE_CLONE_URL: &str = "https://github.com/ValveSoftware/wine.git";
pub const WINE_CLONE_BRANCH: &str = "proton_10.0";
pub const WINE_CLONE_DIR: &str = "/tmp/proton-wine";
pub const PROTON_CLONE_URL: &str = "https://github.com/ValveSoftware/Proton.git";
pub const PROTON_CLONE_DIR: &str = "/tmp/proton";
pub const LOG_DIR: &str = "/tmp/amphetamine";

use std::collections::HashSet;
use std::sync::OnceLock;

static GAMING_DLLS_SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
static GAMING_PROGRAMS_SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
static INFRA_DLLS_SET: OnceLock<HashSet<&'static str>> = OnceLock::new();

pub fn is_gaming_dll(name: &str) -> bool {
    GAMING_DLLS_SET.get_or_init(|| GAMING_DLLS.iter().copied().collect())
        .contains(name)
}

pub fn is_gaming_program(name: &str) -> bool {
    GAMING_PROGRAMS_SET.get_or_init(|| GAMING_PROGRAMS.iter().copied().collect())
        .contains(name)
}

pub fn is_infra_dll(name: &str) -> bool {
    INFRA_DLLS_SET.get_or_init(|| INFRA_DLLS.iter().copied().collect())
        .contains(name)
}
