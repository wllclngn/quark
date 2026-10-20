// launcher against fixture trees: environment assembly, the deploy cache,
// env_config parsing, prefix copying, registry seeding, DLL deployment, Wine
// binary discovery, the S: drive and save-data protection.

// The launcher compiles whole into this crate while the tests reach part of
// it, so the rest reads as dead code here and only here.
#[allow(dead_code)]
#[path = "../../rust/src/quark/log.rs"]
mod log;
#[allow(dead_code)]
#[path = "../../rust/src/quark/graph.rs"]
mod graph;
#[allow(dead_code)]
#[path = "../../rust/src/quark/launcher.rs"]
mod launcher;

use launcher::*;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("quark-launcher-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, rel: &str, content: &str) -> PathBuf {
        let path = self.0.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn dir(&self, rel: &str) -> PathBuf {
        let path = self.0.join(rel);
        fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn var<'a>(vars: &'a [(&'static str, String)], key: &str) -> Option<&'a str> {
    vars.iter().find(|(k, _)| *k == key).map(|(_, v)| v.as_str())
}

#[test]
fn wineserver_comes_from_the_wine_tree_it_serves() {
    let t = TempDir::new("wineserver");
    assert_eq!(wineserver_exe(t.path()), PathBuf::from("/usr/bin/wineserver"));
    let own = t.file("bin/wineserver", "");
    assert_eq!(wineserver_exe(t.path()), own);
}

#[test]
fn dll_path_lists_tree_unix_then_windows_and_skips_missing() {
    let t = TempDir::new("dllpath");
    let win = t.dir("lib/wine/x86_64-windows");
    let unix = t.path().join("lib/wine/x86_64-unix");
    let only_win = wine_dll_path(t.path());
    assert_eq!(only_win.split(':').next(), Some(win.to_str().unwrap()));
    assert!(!only_win.split(':').any(|p| Path::new(p) == unix), "missing dir listed");

    fs::create_dir_all(&unix).unwrap();
    let both = wine_dll_path(t.path());
    let parts: Vec<&str> = both.split(':').collect();
    assert_eq!(&parts[..2], [unix.to_str().unwrap(), win.to_str().unwrap()]);
    assert!(parts.iter().all(|p| Path::new(p).exists()));
}

#[test]
fn ld_path_resolves_ntdll_dir_first_then_inherits() {
    let t = TempDir::new("ldpath");
    let unix = t.dir("lib/wine/x86_64-unix");
    let lib = t.path().join("lib");
    let ld = wine_ld_path(t.path());
    let parts: Vec<&str> = ld.split(':').collect();
    assert_eq!(&parts[..2], [unix.to_str().unwrap(), lib.to_str().unwrap()]);
    if let Ok(inherited) = std::env::var("LD_LIBRARY_PATH") {
        assert!(ld.ends_with(&inherited));
    }
}

#[test]
fn env_vars_share_one_definition_of_the_search_paths() {
    let t = TempDir::new("envvars");
    t.dir("lib/wine/x86_64-unix");
    t.dir("lib/wine/x86_64-windows");
    let pfx = t.dir("pfx");
    let vars = build_env_vars(t.path(), &pfx, &["d3d11", "dxgi"], &["d3d12"], &[], &[]);

    // The tree's own search paths, then the EAC runtime's where it is installed.
    let dll_path = var(&vars, "WINEDLLPATH").unwrap();
    let rest = dll_path.strip_prefix(wine_dll_path(t.path()).as_str())
        .expect("WINEDLLPATH starts with the tree's search paths");
    assert!(rest.split(':').filter(|p| !p.is_empty()).all(|p| p.contains("Proton EasyAntiCheat Runtime/v2/")),
            "WINEDLLPATH carries more than the EAC runtime after the tree: {rest}");
    assert_eq!(var(&vars, "LD_LIBRARY_PATH"), Some(wine_ld_path(t.path()).as_str()));
    assert_eq!(var(&vars, "WINESERVER"), Some(wineserver_exe(t.path()).to_str().unwrap()));
    assert_eq!(var(&vars, "WINEPREFIX"), Some(pfx.to_str().unwrap()));
    // ntdll filters plain TEMP/TMP; WINETEMP/WINETMP carry them at launch.
    assert_eq!(var(&vars, "TEMP"), None);
    assert_eq!(var(&vars, "TMP"), None);
}

#[test]
fn env_vars_set_what_proton_sets_and_nothing_wine_ignores() {
    let t = TempDir::new("envparity");
    let pfx = t.dir("pfx");
    let vars = build_env_vars(t.path(), &pfx, &[], &[], &[], &[]);
    for (key, default) in [("VKD3D_SHADER_DEBUG", "none"), ("__GLVND_DISALLOW_PATCHING", "1")] {
        let want = std::env::var(key).unwrap_or_else(|_| default.into());
        assert_eq!(var(&vars, key), Some(want.as_str()));
    }
    for key in ["WINE_NTSYNC", "WINEESYNC", "WINEFSYNC", "WINE_LARGE_ADDRESS_AWARE", "DXVK_ASYNC", "VKD3D_CONFIG"] {
        assert_eq!(var(&vars, key), None, "{key} is set, and no Wine reads it");
    }
}

#[test]
fn steam_utilities_are_named_before_the_graph() {
    use std::ffi::OsStr;
    for exe in ["/home/u/.local/share/Steam/legacycompat/iscriptevaluator.exe",
                "C:\\Program Files (x86)\\Steam\\d3ddriverquery64.exe"] {
        assert!(is_steam_utility(OsStr::new(exe)), "{exe} would start a session");
    }
    assert!(!is_steam_utility(OsStr::new("/lib/steamapps/common/Balatro/Balatro.exe")));
}

#[test]
fn env_vars_leave_the_shader_caches_to_steam() {
    let t = TempDir::new("envcache");
    let pfx = t.dir("pfx");
    let vars = build_env_vars(t.path(), &pfx, &["d3d11"], &["d3d12"], &[], &[]);
    for key in ["__GL_SHADER_DISK_CACHE_PATH", "MESA_SHADER_CACHE_DIR", "DXVK_STATE_CACHE_PATH",
                "DXVK_SHADER_CACHE_PATH", "VKD3D_SHADER_CACHE_PATH", "STEAM_FOSSILIZE_DUMP_PATH"] {
        assert_eq!(var(&vars, key), None, "{key} is set over the per-game cache Steam points it at");
    }
}

#[test]
fn game_drive_is_the_library_root_holding_the_install() {
    let t = TempDir::new("gamedrive");
    let steamapps = t.dir("lib/steamapps");
    let other = t.dir("other/steamapps");
    let game = t.dir("lib/steamapps/common/Game");
    let libraries = format!("{}::{}", other.display(), steamapps.display());
    let root = fs::canonicalize(t.path().join("lib")).unwrap();
    assert_eq!(game_library_dir(game.to_str().unwrap(), &libraries), Some(root));
    // A library that is not a steamapps directory maps as passed.
    let plain = t.dir("plain");
    let in_plain = t.dir("plain/common/Game");
    assert_eq!(game_library_dir(in_plain.to_str().unwrap(), plain.to_str().unwrap()), Some(plain));
    assert_eq!(game_library_dir(game.to_str().unwrap(), ""), None);
    assert_eq!(game_library_dir(game.to_str().unwrap(), other.to_str().unwrap()), None);
}

#[test]
fn env_vars_set_each_key_once() {
    let t = TempDir::new("envunique");
    let pfx = t.dir("pfx");
    let vars = build_env_vars(t.path(), &pfx, &["d3d11"], &[], &["nvapi64"], &[]);
    let mut keys: Vec<&str> = vars.iter().map(|(k, _)| *k).collect();
    keys.sort_unstable();
    let before = keys.len();
    keys.dedup();
    assert_eq!(keys.len(), before, "a key is pushed twice; the later value silently wins");
}

#[test]
fn dll_overrides_mark_deployed_translation_layers_native() {
    let t = TempDir::new("overrides");
    let pfx = t.dir("pfx");
    let vars = build_env_vars(t.path(), &pfx, &["d3d11", "dxgi"], &["d3d12"], &["nvapi64", "nvapi"], &[]);
    let overrides: Vec<&str> = var(&vars, "WINEDLLOVERRIDES").unwrap().split(';').collect();
    for want in ["d3d11=n", "dxgi=n", "d3d12=n", "nvapi64=n", "nvapi=n", "nvcuda=b"] {
        assert!(overrides.contains(&want), "missing {want}");
    }
    for (name, mode) in BASE_OVERRIDES {
        assert!(overrides.contains(&format!("{name}={mode}").as_str()));
    }
    // Wine's own GameInput stays on; Proton disables it for one game only.
    assert!(!overrides.iter().any(|o| o.starts_with("gameinput")));

    let no_nvapi = build_env_vars(t.path(), &pfx, &[], &[], &[], &[]);
    assert!(!var(&no_nvapi, "WINEDLLOVERRIDES").unwrap().contains("nvcuda"));
}

#[test]
fn deploy_cache_round_trips_and_rejects_foreign_data() {
    let t = TempDir::new("cache");
    assert!(DeployCache::load(t.path()).is_none());

    DeployCache { wine_hash: 1, dxvk_hash: 2, vkd3d_hash: 3, steam_hash: u64::MAX }.save(t.path());
    let c = DeployCache::load(t.path()).unwrap();
    assert_eq!((c.wine_hash, c.dxvk_hash, c.vkd3d_hash, c.steam_hash), (1, 2, 3, u64::MAX));

    let cache_file = fs::read_dir(t.path()).unwrap().next().unwrap().unwrap().path();
    for garbage in ["v5:1,2,3,4,5,6,7", "v6:1,2,3", "v6:1,2,3,4,5", "v6:a,b,c,d", ""] {
        fs::write(&cache_file, garbage).unwrap();
        assert!(DeployCache::load(t.path()).is_none(), "accepted {garbage:?}");
    }
}

#[test]
fn dir_hash_is_zero_for_missing_and_stable_for_unchanged() {
    let t = TempDir::new("dirhash");
    assert_eq!(dir_hash(&t.path().join("missing")), 0);
    let d = t.dir("present");
    assert_ne!(dir_hash(&d), 0);
    assert_eq!(dir_hash(&d), dir_hash(&d));
}

#[test]
fn env_config_parses_keys_and_skips_noise() {
    let t = TempDir::new("envconfig");
    t.file("env_config", "# comment\n\nA=1\n  B = two words  \nmalformed\n=novalue\nC=x=y\n");
    let parsed = parse_env_config(&t.path().join("quark"));
    let want: Vec<(String, String)> = [("A", "1"), ("B", "two words"), ("C", "x=y")]
        .iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    assert_eq!(parsed, want);
    assert!(parse_env_config(&t.path().join("nowhere/quark")).is_empty());
}

#[test]
fn copy_dir_fast_hardlinks_files_and_repairs_symlinks() {
    let t = TempDir::new("copyfast");
    let a = t.file("src/a.txt", "alpha");
    t.file("src/sub/b.txt", "beta");
    std::os::unix::fs::symlink("a.txt", t.path().join("src/rel_link")).unwrap();
    std::os::unix::fs::symlink("gone.txt", t.path().join("src/dead_link")).unwrap();
    let dst = t.path().join("dst");

    assert_eq!(copy_dir_fast(&t.path().join("src"), &dst), 4);
    assert_eq!(fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "beta");
    assert_eq!(fs::metadata(dst.join("a.txt")).unwrap().ino(), fs::metadata(&a).unwrap().ino());
    // A relative link is rewritten to the absolute file it resolves to in the
    // source tree; a dead one is carried over as-is.
    assert_eq!(fs::read_link(dst.join("rel_link")).unwrap(), fs::canonicalize(&a).unwrap());
    assert_eq!(fs::read_link(dst.join("dead_link")).unwrap(), PathBuf::from("gone.txt"));

    // Idempotent: nothing left to deploy on a second pass.
    assert_eq!(copy_dir_fast(&t.path().join("src"), &dst), 0);
}

#[test]
fn deploy_dlls_copies_present_names_and_skips_current_ones() {
    let t = TempDir::new("deploydlls");
    t.file("src/d3d11.dll", "eleven");
    t.file("src/dxgi.dll", "dxgi");
    let dst = t.path().join("sys32");

    let deployed = deploy_dlls(&t.path().join("src"), &dst, &["d3d11", "d3d9", "dxgi"], "TEST");
    assert_eq!(deployed, ["d3d11", "dxgi"]);
    assert_eq!(fs::read_to_string(dst.join("d3d11.dll")).unwrap(), "eleven");
    assert!(!dst.join("d3d9.dll").exists());
    assert!(file_matches(&t.path().join("src/dxgi.dll"), &dst.join("dxgi.dll")));

    assert_eq!(deploy_dlls(&t.path().join("src"), &dst, &["d3d11", "dxgi"], "TEST"), ["d3d11", "dxgi"]);
    assert!(deploy_dlls(&t.path().join("absent"), &dst, &["d3d11"], "TEST").is_empty());
}

#[test]
fn wine_binary_prefers_the_loader_beside_ntdll() {
    let t = TempDir::new("winebin");
    assert!(!has_wine_bin(t.path()));
    t.file("bin/wine", "");
    assert!(has_wine_bin(t.path()));
    assert_eq!(wine_binary(t.path()), t.path().join("bin/wine"));
    let unix_wine = t.file("lib/wine/x86_64-unix/wine", "");
    assert_eq!(wine_binary(t.path()), unix_wine);
}

#[test]
fn save_backup_restores_only_what_went_missing() {
    let t = TempDir::new("saves");
    let pfx = t.path().join("pfx");
    let user = "pfx/drive_c/users/steamuser";
    let save = t.file(&format!("{user}/AppData/Roaming/Game/save.dat"), "slot1");
    let kept = t.file(&format!("{user}/Documents/Other/notes.txt"), "keep");
    t.file(&format!("{user}/AppData/Roaming/Microsoft/skip.txt"), "system");

    let backup = snapshot_save_data(&pfx).unwrap();
    assert_eq!(backup, t.path().join("save_backup"));
    assert!(!backup.join("AppData/Roaming/Microsoft").exists());

    fs::remove_file(&save).unwrap();
    fs::write(&kept, "edited by the game").unwrap();
    restore_save_data(&pfx, &backup);
    assert_eq!(fs::read_to_string(&save).unwrap(), "slot1");
    assert_eq!(fs::read_to_string(&kept).unwrap(), "edited by the game");
    assert!(backup.exists(), "a restore keeps the backup as a second safety layer");

    // Nothing missing: the backup is cleaned up.
    let backup = snapshot_save_data(&pfx).unwrap();
    restore_save_data(&pfx, &backup);
    assert!(!backup.exists());
}

#[test]
fn the_wine_user_is_whoever_the_prefix_registry_names() {
    let t = TempDir::new("user");
    let pfx = t.path().join("pfx");
    t.file("pfx/system.reg",
           "[Software\\\\Microsoft\\\\Windows NT\\\\CurrentVersion\\\\ProfileList\\\\S-1-5-21-0-0-0-1000] 1\n\
            \"Flags\"=dword:00000000\n\
            \"ProfileImagePath\"=\"C:\\\\users\\\\gamer\"\n");
    assert_eq!(profile_user(&pfx), "gamer");
    let vars = build_env_vars(t.path(), &pfx, &[], &[], &[], &[]);
    let get = |k: &str| vars.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_str());
    assert_eq!(get("USER"), Some("gamer"), "Wine derives its user, and every profile path, from $USER");
    assert_eq!(get("USERPROFILE"), Some("C:\\users\\gamer"));
    assert_eq!(get("HOMEPATH"), Some("\\users\\gamer"));
    assert_eq!(get("WINEUSERNAME"), None, "ntdll appends it from USER; a second copy is a second source");

    let bare = t.path().join("bare");
    assert_eq!(profile_user(&bare), "steamuser", "no registry yet: the profile wineboot will create");
}

#[test]
fn valves_client_dlls_replace_the_bridge_deployed_under_their_names() {
    let t = TempDir::new("steamclient");
    let steam = t.path().join("steam");
    t.file("steam/legacycompat/steamclient64.dll", "valve 64");
    t.file("steam/legacycompat/steamclient.dll", "valve 32");
    let pfx = t.path().join("pfx");
    let steam_pfx = "pfx/drive_c/Program Files (x86)/Steam";
    t.file(&format!("{steam_pfx}/steamclient64.dll"), "the bridge, a Wine builtin");
    t.file("pfx/drive_c/windows/system32/steamclient64.dll", "the bridge again");

    deploy_steam_client(&steam, &pfx);
    assert_eq!(fs::read_to_string(pfx.join("drive_c/Program Files (x86)/Steam/steamclient64.dll")).unwrap(), "valve 64");
    assert_eq!(fs::read_to_string(pfx.join("drive_c/Program Files (x86)/Steam/steamclient.dll")).unwrap(), "valve 32");
    assert!(!pfx.join("drive_c/windows/system32/steamclient64.dll").exists(), "the copy the old deploy left is gone");

    // A second pass copies nothing.
    let before = fs::metadata(pfx.join("drive_c/Program Files (x86)/Steam/steamclient64.dll")).unwrap().modified().unwrap();
    deploy_steam_client(&steam, &pfx);
    assert_eq!(fs::metadata(pfx.join("drive_c/Program Files (x86)/Steam/steamclient64.dll")).unwrap().modified().unwrap(), before);
}

#[test]
fn drivers_land_in_system32_drivers_where_their_services_point() {
    let t = TempDir::new("layout");
    let wine = t.path().join("wine");
    t.file("wine/lib/wine/x86_64-windows/ntdll.dll", "ntdll");
    t.file("wine/lib/wine/x86_64-windows/kernel32.dll", "kernel32");
    t.file("wine/lib/wine/x86_64-windows/mountmgr.sys", "mountmgr");

    let pfx = t.path().join("pfx");
    prepare_prefix_layout(&wine, &pfx);
    let sys32 = pfx.join("drive_c/windows/system32");
    assert!(sys32.join("kernel32.dll").exists());
    assert!(sys32.join("drivers/mountmgr.sys").exists());
    assert!(!sys32.join("mountmgr.sys").exists());

    // A prefix synced before drivers had their own directory gets them,
    // and nothing else is replaced: DXVK's d3d11.dll stays.
    fs::remove_dir_all(sys32.join("drivers")).unwrap();
    fs::write(sys32.join("kernel32.dll"), "deployed over").unwrap();
    prepare_prefix_layout(&wine, &pfx);
    assert!(sys32.join("drivers/mountmgr.sys").exists());
    assert_eq!(fs::read_to_string(sys32.join("kernel32.dll")).unwrap(), "deployed over");
}

#[test]
fn syswow64_gets_the_32_bit_builtins_a_32_bit_process_loads() {
    let t = TempDir::new("wow64");
    let wine = t.path().join("wine");
    t.file("wine/lib/wine/x86_64-windows/ntdll.dll", "ntdll");
    t.file("wine/lib/wine/i386-windows/ntdll.dll", "ntdll32");
    t.file("wine/lib/wine/i386-windows/kernel32.dll", "kernel32_32");
    t.file("wine/lib/wine/i386-windows/d3d11.dll", "wined3d32");
    let pfx = t.path().join("pfx");
    t.file("pfx/drive_c/windows/syswow64/d3d11.dll", "dxvk32");
    let wow = pfx.join("drive_c/windows/syswow64");

    prepare_prefix_layout(&wine, &pfx);
    assert_eq!(fs::read_to_string(wow.join("kernel32.dll")).unwrap(), "kernel32_32");
    assert!(wow.join("ntdll.dll").exists());
    // What the prefix already holds stays: DXVK's 32-bit d3d11.dll.
    assert_eq!(fs::read_to_string(wow.join("d3d11.dll")).unwrap(), "dxvk32");

    // A prefix that has its 32-bit ntdll costs one stat and is left alone.
    fs::remove_file(wow.join("kernel32.dll")).unwrap();
    prepare_prefix_layout(&wine, &pfx);
    assert!(!wow.join("kernel32.dll").exists());
}

#[test]
fn a_new_prefix_takes_the_template_registry_and_an_old_one_keeps_its_own() {
    let t = TempDir::new("seed");
    let self_exe = t.file("compat/proton", "");
    t.file("compat/default_pfx/system.reg", "template system");
    t.file("compat/default_pfx/user.reg", "template user");
    let tpl_users = t.path().join("compat/default_pfx/drive_c/users");
    t.file("compat/default_pfx/drive_c/users/steamuser/AppData/Local/Temp/.keep", "");
    std::os::unix::fs::symlink("/", tpl_users.join("steamuser/Documents")).unwrap();

    let fresh = t.path().join("fresh/pfx");
    seed_prefix_registry(&self_exe, &fresh);
    assert_eq!(fs::read_to_string(fresh.join("system.reg")).unwrap(), "template system");
    assert_eq!(fs::read_to_string(fresh.join("user.reg")).unwrap(), "template user");
    assert!(!fresh.join("userdef.reg").exists(), "absent from the template, so not invented");
    let profile = fresh.join("drive_c/users/steamuser");
    assert!(profile.join("AppData/Local/Temp").is_dir(), "the profile folders user.reg names exist");
    assert!(!profile.join("AppData/Local/Temp/.keep").exists(), "directories only, no template files");
    assert!(fs::symlink_metadata(profile.join("Documents")).is_err(), "template symlinks are not followed or copied");

    let old = t.path().join("old/pfx");
    t.file("old/pfx/user.reg", "the game's own");
    t.file("old/pfx/system.reg", "the game's own");
    seed_prefix_registry(&self_exe, &old);
    assert_eq!(fs::read_to_string(old.join("system.reg")).unwrap(), "the game's own");
    assert!(!old.join("drive_c/users/steamuser").exists(), "an old prefix keeps its own profile tree");

    let untemplated = TempDir::new("seed-none");
    let bare_exe = untemplated.file("compat/proton", "");
    let pfx = untemplated.path().join("pfx");
    seed_prefix_registry(&bare_exe, &pfx);
    assert!(!pfx.join("system.reg").exists());
}
