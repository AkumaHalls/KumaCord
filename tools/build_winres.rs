// The Windows resources of concord's programs (v1.0.1): the exe's icon (`app/assets/logo/concord.ico`,
// the owner's logo) and its version info (the name Explorer, the firewall prompt and the
// notifications show). Shared by `app/build.rs` and `hub/build.rs` (`#[path]`).
//
// No crate: the Windows SDK's rc.exe compiles a small .rc into a .res, which the MSVC linker takes
// as an input. rc.exe is looked for in `RC_EXE`, then in the Windows 10/11 SDK (newest first), then
// on PATH. Without it the build goes on with a warning and the exe simply keeps the default icon
// (the window, tray and notification icons are drawn by the app itself and do not depend on it).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Embed `ico` and the version info as `exe_name` (e.g. "concord.exe") described as `description`.
pub fn embed(ico: &Path, exe_name: &str, description: &str) {
    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed=../tools/build_winres.rs");
    println!("cargo:rerun-if-env-changed=RC_EXE");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe (Windows SDK) not found: {exe_name} is built without its icon. Set RC_EXE to embed it.");
        return;
    };
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let nums: Vec<u16> = version.split(['.', '-', '+']).take(3).map(|p| p.parse().unwrap_or(0)).collect();
    let (a, b, c) = (nums.first().copied().unwrap_or(0), nums.get(1).copied().unwrap_or(0), nums.get(2).copied().unwrap_or(0));
    let ico_abs = std::fs::canonicalize(ico).unwrap_or_else(|_| ico.to_path_buf());
    // (rc.exe reads `\\?\` paths badly, and backslashes are escapes in its strings.)
    let ico_text = ico_abs.display().to_string().trim_start_matches(r"\\?\").replace('\\', "\\\\");
    let rc_text = format!(
        "1 ICON \"{ico_text}\"\n\
         1 VERSIONINFO\n\
         FILEVERSION {a},{b},{c},0\n\
         PRODUCTVERSION {a},{b},{c},0\n\
         FILEFLAGSMASK 0x3f\n\
         FILEFLAGS 0x0\n\
         FILEOS 0x40004\n\
         FILETYPE 0x1\n\
         FILESUBTYPE 0x0\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"041604B0\"\n\
         \x20   BEGIN\n\
         \x20     VALUE \"FileDescription\", \"{description}\"\n\
         \x20     VALUE \"ProductName\", \"concord\"\n\
         \x20     VALUE \"FileVersion\", \"{version}\"\n\
         \x20     VALUE \"ProductVersion\", \"{version}\"\n\
         \x20     VALUE \"InternalName\", \"{stem}\"\n\
         \x20     VALUE \"OriginalFilename\", \"{exe_name}\"\n\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x0416, 1200\n\
         \x20 END\n\
         END\n",
        stem = exe_name.trim_end_matches(".exe"),
    );
    let rc_file = out.join("concord-resources.rc");
    let res_file = out.join("concord-resources.res");
    if let Err(e) = std::fs::write(&rc_file, rc_text) {
        println!("cargo:warning=could not write {}: {e}; {exe_name} is built without its icon", rc_file.display());
        return;
    }
    let status = Command::new(&rc).arg("/nologo").arg("/fo").arg(&res_file).arg(&rc_file).status();
    match status {
        Ok(s) if s.success() && res_file.exists() => {
            // Only the program links it (not the tests or the library).
            println!("cargo:rustc-link-arg-bins={}", res_file.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe failed ({s}): {exe_name} is built without its icon"),
        Err(e) => println!("cargo:warning=could not run {}: {e}; {exe_name} is built without its icon", rc.display()),
    }
}

fn find_rc() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RC_EXE").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let host_arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
    for base in ["ProgramFiles(x86)", "ProgramFiles"].iter().filter_map(std::env::var_os) {
        let bin = PathBuf::from(base).join("Windows Kits").join("10").join("bin");
        let Ok(dir) = std::fs::read_dir(&bin) else { continue };
        let mut versions: Vec<PathBuf> = dir.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join(host_arch).join("rc.exe").is_file()).collect();
        // "10.0.26100.0" > "10.0.17134.0": compare the numbers, newest first.
        versions.sort_by_key(|p| {
            p.file_name().and_then(|n| n.to_str()).map(|n| n.split('.').map(|x| x.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>()).unwrap_or_default()
        });
        if let Some(v) = versions.last() {
            return Some(v.join(host_arch).join("rc.exe"));
        }
    }
    // On PATH (a Developer Command Prompt).
    Command::new("rc.exe").arg("/?").output().ok().filter(|o| o.status.success() || !o.stdout.is_empty()).map(|_| PathBuf::from("rc.exe"))
}
