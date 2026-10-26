//! `hub/scripts/fetch-livekit.ps1` trusts the SHA256 pinned in the script, not the release's own
//! `checksums.txt`: a zip that matches a (replaced) `checksums.txt` but not the pin is refused.
//! Offline: the script reads a local folder (`-SourceDir`) with a dummy zip; nothing is downloaded.
#![cfg(windows)]

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

fn script() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts").join("fetch-livekit.ps1")
}

fn powershell(args: &[&str]) -> (bool, String) {
    let out = Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"]).args(args).output().unwrap();
    (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// A zip holding a fake `livekit-server.exe`, with a `checksums.txt` that agrees with it.
fn fixture(dir: &Path, version: &str) {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("livekit-server.exe"), b"not livekit").unwrap();
    let zip = dir.join(format!("livekit_{version}_windows_amd64.zip"));
    let cmd = format!("Compress-Archive -Path '{}' -DestinationPath '{}'", src.join("livekit-server.exe").display(), zip.display());
    let (ok, out) = powershell(&["-Command", &cmd]);
    assert!(ok, "{out}");
    let sha = Sha256::digest(std::fs::read(&zip).unwrap());
    let hex: String = sha.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(dir.join("checksums.txt"), format!("{hex}  livekit_{version}_windows_amd64.zip\n")).unwrap();
}

#[test]
fn a_zip_that_only_matches_checksums_txt_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "1.13.7");
    let dest = dir.path().join("bin");
    let (ok, out) = powershell(&[
        "-File",
        script().to_str().unwrap(),
        "-SourceDir",
        dir.path().to_str().unwrap(),
        "-Dest",
        dest.to_str().unwrap(),
    ]);
    assert!(!ok, "accepted: {out}");
    assert!(out.contains("does not match the pinned hash"), "{out}");
    assert!(!dest.join("livekit-server.exe").exists(), "nothing installed");
}

#[test]
fn another_version_needs_its_own_hash() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "9.9.9");
    let dest = dir.path().join("bin");
    let (ok, out) = powershell(&["-File", script().to_str().unwrap(), "-Version", "9.9.9", "-SourceDir", dir.path().to_str().unwrap(), "-Dest", dest.to_str().unwrap()]);
    assert!(!ok && out.contains("No SHA256 pinned"), "{out}");
    assert!(!dest.join("livekit-server.exe").exists());
}
