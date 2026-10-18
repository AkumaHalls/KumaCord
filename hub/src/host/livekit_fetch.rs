//! The voice server (LiveKit) is not shipped with the app: the first "Criar servidor" downloads
//! the pinned release, the same one `hub/scripts/fetch-livekit.ps1` installs, and checks it against
//! the SHA-256 pinned here (the zip, then the `livekit-server.exe` inside it). A mismatch installs
//! nothing. The zip is read with a few lines over `miniz_oxide` (already in the tree through `png`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::util::hex;

pub const VERSION: &str = "1.13.7";
pub const EXE_NAME: &str = "livekit-server.exe";

/// What a download / install is checked against.
#[derive(Debug, Clone, Copy)]
pub struct Pins<'a> {
    pub zip_sha256: &'a str,
    pub exe_sha256: &'a str,
}

/// livekit_1.13.7_windows_amd64.zip and the exe inside it (same as `fetch-livekit.ps1`).
pub const PINNED: Pins<'static> = Pins {
    zip_sha256: "e539e7d2f75807b9c9202cd2a0bf2cb3d52fc4c52978a6953e0f47bc339fe77f",
    exe_sha256: "951f9466cd4450b3c3c7f1a5830a05a9bb97cc11d93cf1be9ebcd3b47cc316d1",
};

/// Largest download accepted (the zip is ~20 MB) and largest exe unpacked (~45 MB).
const MAX_ZIP: usize = 100 * 1024 * 1024;
const MAX_EXE: usize = 200 * 1024 * 1024;

pub fn zip_name() -> String {
    format!("livekit_{VERSION}_windows_amd64.zip")
}

pub fn zip_url() -> String {
    format!("https://github.com/livekit/livekit/releases/download/v{VERSION}/{}", zip_name())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// The installed voice server in `bin_dir`, if it is there and is the pinned one.
pub fn installed(bin_dir: &Path) -> Option<PathBuf> {
    let exe = bin_dir.join(EXE_NAME);
    match sha256_file(&exe) {
        Ok(h) if h == PINNED.exe_sha256 => Some(exe),
        Ok(h) => {
            tracing::warn!(path = %exe.display(), sha256 = h, "livekit: installed file is not the pinned release");
            None
        }
        Err(_) => None,
    }
}

/// Download the pinned zip (`progress(bytes so far, total)`), check it and install the exe.
pub async fn download(bin_dir: &Path, progress: &(dyn Fn(u64, Option<u64>) + Send + Sync)) -> Result<PathBuf> {
    crate::linkpreview::fetch::install_crypto_provider();
    let client = reqwest::Client::builder()
        .user_agent(concat!("concord/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()?;
    let url = zip_url();
    tracing::info!(%url, "livekit: downloading");
    let mut resp = client
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .context("Não foi possível baixar o servidor de voz. Confira a internet e tente de novo.")?;
    let total = resp.content_length();
    if total.is_some_and(|t| t as usize > MAX_ZIP) {
        bail!("O download do servidor de voz é maior do que o esperado; nada foi instalado.");
    }
    let mut buf = Vec::with_capacity(total.unwrap_or(32 << 20).min(MAX_ZIP as u64) as usize);
    progress(0, total);
    while let Some(chunk) = resp.chunk().await.context("O download do servidor de voz parou no meio. Tente de novo.")? {
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_ZIP {
            bail!("O download do servidor de voz é maior do que o esperado; nada foi instalado.");
        }
        progress(buf.len() as u64, total);
    }
    tracing::info!(bytes = buf.len(), "livekit: downloaded");
    let bin_dir = bin_dir.to_path_buf();
    tokio::task::spawn_blocking(move || install_zip(&bin_dir, &buf, &PINNED)).await?
}

/// Check `zip` against `pins.zip_sha256`, take the exe out, check it against `pins.exe_sha256`
/// and install it in `bin_dir`.
pub fn install_zip(bin_dir: &Path, zip: &[u8], pins: &Pins) -> Result<PathBuf> {
    let got = hex(&Sha256::digest(zip));
    if got != pins.zip_sha256 {
        tracing::error!(expected = pins.zip_sha256, got, "livekit: zip hash mismatch");
        bail!("O arquivo do servidor de voz não confere com a assinatura esperada (SHA-256). Por segurança, nada foi instalado.");
    }
    let exe = extract(zip, EXE_NAME, MAX_EXE).context("O arquivo do servidor de voz veio danificado.")?;
    install_exe_bytes(bin_dir, &exe, pins)
}

/// A local copy instead of the download (offline installs, tests): a folder with the release zip,
/// or the exe itself. Checked exactly like a download.
pub fn install_from(bin_dir: &Path, source: &Path, pins: &Pins) -> Result<PathBuf> {
    if source.is_dir() {
        let zip = std::fs::read(source.join(zip_name())).with_context(|| format!("reading {} from {}", zip_name(), source.display()))?;
        return install_zip(bin_dir, &zip, pins);
    }
    let exe = std::fs::read(source).with_context(|| format!("reading {}", source.display()))?;
    install_exe_bytes(bin_dir, &exe, pins)
}

fn install_exe_bytes(bin_dir: &Path, exe: &[u8], pins: &Pins) -> Result<PathBuf> {
    let got = hex(&Sha256::digest(exe));
    if got != pins.exe_sha256 {
        tracing::error!(expected = pins.exe_sha256, got, "livekit: exe hash mismatch");
        bail!("O servidor de voz não confere com a assinatura esperada (SHA-256). Por segurança, nada foi instalado.");
    }
    std::fs::create_dir_all(bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    let part = bin_dir.join(format!("{EXE_NAME}.part"));
    let out = bin_dir.join(EXE_NAME);
    std::fs::write(&part, exe).with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, &out).with_context(|| format!("installing {}", out.display()))?;
    tracing::info!(path = %out.display(), sha256 = got, "livekit: installed");
    Ok(out)
}

// ---------------------------------------------------------------- zip

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The file whose name (last path part, any case) is `name`, unpacked (stored or deflate; no
/// encryption, no zip64), at most `max` bytes.
pub fn extract(zip: &[u8], name: &str, max: usize) -> Result<Vec<u8>> {
    const EOCD: u32 = 0x0605_4b50;
    const CENTRAL: u32 = 0x0201_4b50;
    const LOCAL: u32 = 0x0403_4b50;
    let bad = || anyhow::anyhow!("not a usable zip");
    let floor = zip.len().saturating_sub(22 + 0xFFFF);
    let eocd = (floor..=zip.len().saturating_sub(22)).rev().find(|&i| u32_at(zip, i) == Some(EOCD)).ok_or_else(bad)?;
    let entries = u16_at(zip, eocd + 10).ok_or_else(bad)? as usize;
    let mut at = u32_at(zip, eocd + 16).ok_or_else(bad)? as usize;
    for _ in 0..entries {
        if u32_at(zip, at) != Some(CENTRAL) {
            return Err(bad());
        }
        let flags = u16_at(zip, at + 8).ok_or_else(bad)?;
        let method = u16_at(zip, at + 10).ok_or_else(bad)?;
        let csize = u32_at(zip, at + 20).ok_or_else(bad)? as usize;
        let usize_ = u32_at(zip, at + 24).ok_or_else(bad)? as usize;
        let (n, e, c) = (u16_at(zip, at + 28).ok_or_else(bad)? as usize, u16_at(zip, at + 30).ok_or_else(bad)? as usize, u16_at(zip, at + 32).ok_or_else(bad)? as usize);
        let local = u32_at(zip, at + 42).ok_or_else(bad)? as usize;
        let entry_name = String::from_utf8_lossy(zip.get(at + 46..at + 46 + n).ok_or_else(bad)?).into_owned();
        at += 46 + n + e + c;
        let last = entry_name.rsplit(['/', '\\']).next().unwrap_or("");
        if !last.eq_ignore_ascii_case(name) {
            continue;
        }
        if flags & 1 != 0 || csize == u32::MAX as usize || usize_ > max {
            return Err(bad());
        }
        if u32_at(zip, local) != Some(LOCAL) {
            return Err(bad());
        }
        let data_at = local + 30 + u16_at(zip, local + 26).ok_or_else(bad)? as usize + u16_at(zip, local + 28).ok_or_else(bad)? as usize;
        let data = zip.get(data_at..data_at + csize).ok_or_else(bad)?;
        let out = match method {
            0 => data.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, max).map_err(|_| bad())?,
            _ => return Err(bad()),
        };
        if out.len() != usize_ {
            return Err(bad());
        }
        return Ok(out);
    }
    bail!("{name} not found in the zip")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A zip with the given files (`deflate`: method 8, else stored).
    pub(crate) fn make_zip(files: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in files {
            let (method, body) = if deflate { (8u16, miniz_oxide::deflate::compress_to_vec(data, 6)) } else { (0u16, data.to_vec()) };
            let offset = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 8]); // time, date, crc (not checked here)
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&body);
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]); // extra, comment, disk, attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_at = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn sha(b: &[u8]) -> String {
        hex(&Sha256::digest(b))
    }

    #[test]
    fn the_exe_comes_out_of_the_zip() {
        let exe = b"MZ fake livekit ".repeat(1000);
        for deflate in [false, true] {
            let zip = make_zip(&[("LICENSE", b"text"), ("livekit_1.13.7/livekit-server.exe", &exe)], deflate);
            assert_eq!(extract(&zip, EXE_NAME, MAX_EXE).unwrap(), exe);
            assert!(extract(&zip, "other.exe", MAX_EXE).is_err());
            assert!(extract(&zip, EXE_NAME, 100).is_err(), "over the size cap");
        }
        assert!(extract(b"not a zip at all", EXE_NAME, MAX_EXE).is_err());
        let zip = make_zip(&[("livekit-server.exe", &exe)], true);
        assert!(extract(&zip[..zip.len() / 2], EXE_NAME, MAX_EXE).is_err(), "cut short");
    }

    #[test]
    fn only_the_pinned_files_are_installed() {
        let dir = tempfile::tempdir().unwrap();
        let exe = b"MZ fake livekit".to_vec();
        let zip = make_zip(&[("livekit-server.exe", &exe)], true);
        // The real pins: a zip that is not the release is refused, nothing is written.
        let e = install_zip(dir.path(), &zip, &PINNED).unwrap_err();
        assert!(format!("{e:#}").contains("SHA-256"), "{e:#}");
        assert!(!dir.path().join(EXE_NAME).exists());
        // A zip with the right hash but another exe inside is refused too.
        let zip_sha = sha(&zip);
        let e = install_zip(dir.path(), &zip, &Pins { zip_sha256: &zip_sha, exe_sha256: PINNED.exe_sha256 }).unwrap_err();
        assert!(format!("{e:#}").contains("SHA-256"), "{e:#}");
        assert!(!dir.path().join(EXE_NAME).exists());
        // Both right: installed.
        let exe_sha = sha(&exe);
        let pins = Pins { zip_sha256: &zip_sha, exe_sha256: &exe_sha };
        let out = install_zip(dir.path(), &zip, &pins).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), exe);
        // From a local folder holding the zip, and from an exe file.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join(zip_name()), &zip).unwrap();
        let dest = dir.path().join("b");
        install_from(&dest, src.path(), &pins).unwrap();
        assert_eq!(std::fs::read(dest.join(EXE_NAME)).unwrap(), exe);
        std::fs::write(src.path().join("x.exe"), b"something else").unwrap();
        assert!(install_from(&dest, &src.path().join("x.exe"), &pins).is_err());
        // `installed` only accepts the pinned release.
        assert_eq!(installed(&dest), None);
    }

    /// The real `bin\livekit-server.exe` of this checkout (when present) is the pinned one, and
    /// installing it from that file works (no download).
    #[test]
    fn the_pinned_exe_installs_from_a_local_copy() {
        let real = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("bin").join(EXE_NAME);
        if !real.exists() {
            eprintln!("skipped: no bin\\{EXE_NAME}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = install_from(dir.path(), &real, &PINNED).unwrap();
        assert_eq!(installed(dir.path()), Some(out));
    }

    /// One real download from GitHub (run by hand: `cargo test -p voxhub-server real_download -- --ignored`).
    #[tokio::test]
    #[ignore]
    async fn real_download_matches_the_pins() {
        let dir = tempfile::tempdir().unwrap();
        let seen = std::sync::atomic::AtomicU64::new(0);
        let out = download(dir.path(), &|n, _| seen.store(n, std::sync::atomic::Ordering::Relaxed)).await.unwrap();
        assert_eq!(installed(dir.path()), Some(out));
        eprintln!("downloaded {} bytes", seen.load(std::sync::atomic::Ordering::Relaxed));
    }
}
