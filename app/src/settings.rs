//! `%APPDATA%\concord\settings.json`. The server password is only stored when the user ticks
//! "Lembrar senha", and then only encrypted with Windows DPAPI (current user) or, on macOS, kept in
//! the login Keychain (settings.json holds only a reference, see `keychain`). The saved server
//! list (v0.9.1) is `servers.json` next to it (see `servers.rs`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use vox_client_core::{GateConfig, NoiseLevel, UserAudio, VoiceFxConfig};

use crate::alerts::AlertSettings;

/// Go Live quality presets. The bitrates come from `docs/quality-bench.md` (hard synthetic
/// motion through the real hardware encoders): past these the quality gain per Mbps flattens out.
/// The host's upload carries one copy per viewer (the SFU forwards, it doesn't transcode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Preset {
    P720p30,
    #[default]
    P1080p30,
    P1080p60,
    /// "Máxima": 1080p60 with more bits, for fast games / 3D on a good connection.
    Max1080p60,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::P720p30, Preset::P1080p30, Preset::P1080p60, Preset::Max1080p60];

    pub fn label(self) -> &'static str {
        match self {
            Preset::P720p30 => "720p 30 fps",
            Preset::P1080p30 => "1080p 30 fps",
            Preset::P1080p60 => "1080p 60 fps",
            Preset::Max1080p60 => "Máxima (1080p 60 fps)",
        }
    }

    /// (width, height, fps)
    pub fn dims(self) -> (u32, u32, u32) {
        match self {
            Preset::P720p30 => (1280, 720, 30),
            Preset::P1080p30 => (1920, 1080, 30),
            Preset::P1080p60 | Preset::Max1080p60 => (1920, 1080, 60),
        }
    }

    /// Video bitrate cap (bps).
    pub fn bitrate(self) -> u64 {
        match self {
            Preset::P720p30 => 4_000_000,
            Preset::P1080p30 => 8_000_000,
            Preset::P1080p60 => 12_000_000,
            Preset::Max1080p60 => 16_000_000,
        }
    }
}

/// Webcam quality ("Qualidade da câmera"), all at 30 fps. The bitrate follows the size (about
/// 1.2 Mbps at 720p, see `vox_client_core::golive::camera::camera_bitrate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CameraPreset {
    P360,
    P540,
    #[default]
    P720,
    P1080,
}

impl CameraPreset {
    pub const ALL: [CameraPreset; 4] = [CameraPreset::P360, CameraPreset::P540, CameraPreset::P720, CameraPreset::P1080];

    pub fn label(self) -> &'static str {
        match self {
            CameraPreset::P360 => "360p (economiza internet)",
            CameraPreset::P540 => "540p",
            CameraPreset::P720 => "720p (recomendado)",
            CameraPreset::P1080 => "1080p",
        }
    }

    pub fn to_core(self) -> vox_client_core::golive::camera::CameraQuality {
        use vox_client_core::golive::camera::CameraQuality as Q;
        match self {
            CameraPreset::P360 => Q::P360,
            CameraPreset::P540 => Q::P540,
            CameraPreset::P720 => Q::P720,
            CameraPreset::P1080 => Q::P1080,
        }
    }
}

/// One person's local listening settings (what *I* hear), in percent. Only non-default entries
/// are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserAudioPref {
    /// Their microphone volume, 0..=200.
    pub volume_pct: u32,
    pub muted: bool,
    /// Their screenshare audio volume, 0..=200.
    pub stream_volume_pct: u32,
    pub stream_muted: bool,
}

impl Default for UserAudioPref {
    fn default() -> Self {
        Self { volume_pct: 100, muted: false, stream_volume_pct: 100, stream_muted: false }
    }
}

impl UserAudioPref {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn to_core(self) -> UserAudio {
        UserAudio {
            volume: self.volume_pct.min(200) as f32 / 100.0,
            muted: self.muted,
            stream_volume: self.stream_volume_pct.min(200) as f32 / 100.0,
            stream_muted: self.stream_muted,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: String,
    pub username: String,
    pub remember_password: bool,
    /// DPAPI-encrypted server password, hex encoded. Only present with `remember_password`.
    pub password_enc: Option<String>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// Output volume in percent (0..=200).
    pub volume_pct: u32,
    pub preset: Preset,
    pub share_audio: bool,
    /// "Priorizar fluidez": keep the frame rate under congestion (default: drop fps first).
    pub share_smooth: bool,
    pub ptt_enabled: bool,
    pub ptt_vk: u32,
    pub ptt_name: String,
    /// Input sensitivity ("Sensibilidade de entrada"): automatic or manual voice-activity gate.
    pub input_gate: GateConfig,
    /// "Supressão de ruído": Padrão (WebRTC), IA leve (RNNoise, the default) or IA máxima
    /// (DeepFilterNet3).
    pub noise_suppression: NoiseLevel,
    /// "Modificador de voz": effect and intensity (older files get "Desligado").
    pub voice_fx: VoiceFxConfig,
    /// Local volume / mute per person, keyed by username (identities are per-session ids).
    pub user_audio: BTreeMap<String, UserAudioPref>,
    /// Alert sounds (only I hear them).
    pub alerts: AlertSettings,
    /// "Abrir transmissões em janela separada": watched streams open in their own window
    /// instead of inside the call view.
    pub stream_popout: bool,
    /// "Ouvir só as transmissões fixadas": with several watched streams, only the pinned ones (or
    /// the fullscreen one) play sound. Off by default: every watched stream is heard.
    pub stream_focus_audio: bool,
    /// "Mostrar prévia da minha transmissão" (item 8a): while I am live, my stream tile shows a
    /// small picture of what I share. Off tears the preview tap down completely (no second
    /// video processor, no staging textures). Older files get the default (on).
    pub show_self_preview: bool,
    /// Camera to use (`vox_capture::CameraInfo::id`); `None` = the first camera.
    pub camera_device: Option<String>,
    /// Its name, to find it again if the id changed (plugged into another USB port).
    pub camera_name: Option<String>,
    pub camera_preset: CameraPreset,
    /// Per camera (by device link, else name): native format, image controls, mirror / rotation /
    /// zoom (v0.6). Only cameras with non-default choices are stored.
    pub camera_profiles: crate::camera_cfg::CameraProfiles,
    /// "Aceleração de hardware" (v0.7): off = the processor encodes the screen share and the
    /// camera. Older files get on.
    pub hw_accel: bool,
    /// "Codificador": a specific encoder (`vox_capture::EncoderCandidate::id`, stable across
    /// reboots); `None` = "Automático (recomendado)".
    pub video_encoder: Option<String>,
    /// "Mostrar prévia de links": cards with title and thumbnail under chat links (the hub
    /// fetches them). Added with item 10, so older settings.json files get the default (on).
    pub show_link_previews: bool,
    /// "Qualidade das imagens enviadas" (chat images): Normal (1920 px), Alta (2560) or Original
    /// (up to 4096). Older files get Normal.
    pub image_quality: vox_client_core::images::Quality,
    /// "Mostrar imagens automaticamente": chat images load by themselves (off: click to load).
    pub auto_show_images: bool,
    /// Item 12: per-device login tokens ("Lembrar este computador"), keyed by
    /// `server|username` (lowercase), each DPAPI-encrypted and hex encoded like the password.
    pub device_tokens: BTreeMap<String, String>,
    /// v1.0.1: the identity (`HealthResponse::server_id`) of the server that issued each login
    /// token above, same keys. A token is sent only to an address that answers with it (see
    /// `backend::Guard`); a token without one (saved before v1.0.1, or by an older hub) gets it
    /// at its next login. Not a secret.
    pub device_token_servers: BTreeMap<String, String>,
    /// Soundboard volume, mute and silenced sounds (added after v0.4.1: older files get the defaults).
    pub soundboard: crate::soundboard::SoundboardSettings,
    /// Profile settings of this computer (idle time before "Ausente"); older files get the defaults.
    pub profile: crate::profile::ProfileSettings,
    /// Windows notifications and the tray ("Notificações"); older files get the defaults.
    pub notify: crate::app::notify::NotifySettings,
    /// Reactions I used last, newest first (the picker's "Recentes"; v0.8).
    pub recent_reactions: Vec<String>,
    /// The v1.0 shell's remembered choices (center mode, docked chat width, member list).
    pub layout: crate::layout::LayoutSettings,
    /// "Seus servidores salvos" (v0.9.1). Kept in `servers.json` next to this file, not in it: an
    /// older app rewrites settings.json without it (see `servers.rs`). `server`, `username`,
    /// `remember_password` and `password_enc` above mirror its most recently used entry.
    #[serde(skip)]
    pub servers: crate::servers::ServerList,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server: String::new(),
            username: String::new(),
            remember_password: false,
            password_enc: None,
            input_device: None,
            output_device: None,
            volume_pct: 100,
            preset: Preset::default(),
            share_audio: true,
            share_smooth: false,
            ptt_enabled: false,
            ptt_vk: 0,
            ptt_name: String::new(),
            input_gate: GateConfig::default(),
            noise_suppression: NoiseLevel::default(),
            voice_fx: VoiceFxConfig::default(),
            user_audio: BTreeMap::new(),
            alerts: AlertSettings::default(),
            stream_popout: false,
            stream_focus_audio: false,
            show_self_preview: true,
            camera_device: None,
            camera_name: None,
            camera_preset: CameraPreset::default(),
            camera_profiles: Default::default(),
            hw_accel: true,
            video_encoder: None,
            show_link_previews: true,
            image_quality: Default::default(),
            auto_show_images: true,
            device_tokens: BTreeMap::new(),
            device_token_servers: BTreeMap::new(),
            soundboard: Default::default(),
            profile: Default::default(),
            notify: Default::default(),
            recent_reactions: Vec::new(),
            layout: Default::default(),
            servers: Default::default(),
        }
    }
}

/// Autotests (`--settings-dir`): settings.json and servers.json live in this folder.
static DIR: OnceLock<PathBuf> = OnceLock::new();

/// Autotests only: keep the settings in `dir` (read at start, written on every change) instead of
/// `%APPDATA%\concord`.
pub fn set_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// The settings live in an autotest's own folder.
pub fn dir_overridden() -> bool {
    DIR.get().is_some()
}

fn path() -> Option<PathBuf> {
    if let Some(d) = DIR.get() {
        return Some(d.join("settings.json"));
    }
    if let Some(d) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(d).join("concord").join("settings.json"));
    }
    // macOS: ~/Library/Application Support/concord (there is no %APPDATA%).
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home).join("Library").join("Application Support").join("concord").join("settings.json"));
    }
    None
}

/// Where settings.json is (the autotest reads the files back).
pub fn file_path() -> Option<PathBuf> {
    path()
}

/// The saved server list (v0.9.1), next to settings.json.
pub fn servers_path(settings: &Path) -> PathBuf {
    settings.with_file_name("servers.json")
}

/// Write through a temporary file, so a crash never leaves half a file.
fn write_atomic(p: &Path, text: &str) -> Result<(), String> {
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, p).map_err(|e| e.to_string())
}

/// The video encoder self-test results (v0.7), next to settings.json.
pub fn encoder_cache_path() -> Option<PathBuf> {
    path().map(|p| p.with_file_name("encoders.json"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(p) = path() else { return Self::default() };
        Self::load_at(&p)
    }

    /// settings.json at `p`, and the saved server list next to it. The single saved server of
    /// v0.9.0 (or of an older app run since) joins the list (`servers.rs`).
    pub fn load_at(p: &Path) -> Self {
        let mut s: Settings = match std::fs::read_to_string(p) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                warn!(error = %e, "settings.json is invalid; using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        let list = std::fs::read_to_string(servers_path(p)).ok();
        s.servers = list.as_deref().and_then(crate::servers::ServerList::from_json).unwrap_or_default();
        if list.is_some() && s.servers.servers.is_empty() && s.servers.next_id == 0 {
            warn!("servers.json is invalid; starting a new list");
        }
        if s.merge_legacy_server(crate::app::now_ms()) {
            info!(entries = s.servers.servers.len(), "server list: the saved server of an older version joined the list");
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        self.save_at(&p);
    }

    /// settings.json at `p`, and servers.json next to it.
    pub fn save_at(&self, p: &Path) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let res = serde_json::to_string_pretty(self).map_err(|e| e.to_string()).and_then(|text| write_atomic(p, &text));
        if let Err(e) = res {
            warn!(error = %e, "cannot save settings");
        }
        if let Err(e) = write_atomic(&servers_path(p), &self.servers.to_json()) {
            warn!(error = %e, "cannot save the server list");
        }
    }

    /// The remembered server password, if any (decrypted).
    pub fn saved_password(&self) -> Option<String> {
        if !self.remember_password {
            return None;
        }
        unprotect(self.password_enc.as_deref()?)
    }

    fn device_key(server: &str, username: &str) -> String {
        format!("{}|{}", server.trim().to_ascii_lowercase(), username.trim().to_ascii_lowercase())
    }

    /// This computer's login token for `username` on `server` (item 12), decrypted.
    pub fn device_token(&self, server: &str, username: &str) -> Option<String> {
        unprotect(self.device_tokens.get(&Self::device_key(server, username))?)
    }

    /// Remember (DPAPI-encrypted) or forget this computer's login token for a name. A new token
    /// starts without a server identity ([`Settings::set_device_token_server`]).
    pub fn set_device_token(&mut self, server: &str, username: &str, token: Option<&str>) {
        let key = Self::device_key(server, username);
        self.device_token_servers.remove(&key);
        match token.and_then(protect) {
            Some(enc) => {
                self.device_tokens.insert(key, enc);
            }
            None => {
                self.device_tokens.remove(&key);
            }
        }
    }

    /// v1.0.1: the identity of the server that issued this computer's login token for `username`
    /// on `server`, when known.
    pub fn device_token_server(&self, server: &str, username: &str) -> Option<String> {
        self.device_token_servers.get(&Self::device_key(server, username)).filter(|id| vox_proto::is_server_id(id)).cloned()
    }

    /// v1.0.1: tie the login token saved for `username` on `server` (if there is one) to a server
    /// identity (`None`: unknown).
    pub fn set_device_token_server(&mut self, server: &str, username: &str, id: Option<&str>) {
        let key = Self::device_key(server, username);
        match id.filter(|id| vox_proto::is_server_id(id) && self.device_tokens.contains_key(&key)) {
            Some(id) => {
                self.device_token_servers.insert(key, id.to_string());
            }
            None => {
                self.device_token_servers.remove(&key);
            }
        }
    }
    /// The video encoder choice for vox-client-core (Settings → Vídeo).
    pub fn encoder_pref(&self) -> vox_client_core::golive::encoders::EncoderPref {
        vox_client_core::golive::encoders::EncoderPref { hardware: self.hw_accel, encoder: self.video_encoder.clone() }
    }

    /// Saved local audio settings of `username` (defaults when none).
    pub fn user_audio(&self, username: &str) -> UserAudioPref {
        self.user_audio.get(username).copied().unwrap_or_default()
    }

    /// Remember `username`'s local audio settings; default values are not stored.
    pub fn set_user_audio(&mut self, username: &str, mut pref: UserAudioPref) {
        pref.volume_pct = pref.volume_pct.min(200);
        pref.stream_volume_pct = pref.stream_volume_pct.min(200);
        if pref.is_default() {
            self.user_audio.remove(username);
        } else {
            self.user_audio.insert(username.to_string(), pref);
        }
    }

}

// ---------------------------------------------------------------- DPAPI

/// Kept from before the rename to concord (v1.0.1): the passwords and tokens already saved were
/// encrypted with it, and the migrated settings.json must still decrypt.
#[cfg_attr(not(windows), allow(dead_code))]
const ENTROPY: &[u8] = b"voxhub-settings-v1";

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg_attr(not(windows), allow(dead_code))]
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(windows)]
pub(crate) fn protect(plain: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: the blobs point at live buffers for the duration of the call; the output buffer is
    // allocated by DPAPI and released with LocalFree after copying it out.
    unsafe {
        if CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), 0, &mut out) == 0 {
            return None;
        }
        let hex = to_hex(std::slice::from_raw_parts(out.pbData, out.cbData as usize));
        LocalFree(out.pbData as *mut core::ffi::c_void);
        Some(hex)
    }
}

#[cfg(windows)]
pub(crate) fn unprotect(hex: &str) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};
    let mut bytes = from_hex(hex)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_mut_ptr(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: as in `protect`.
    unsafe {
        if CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        ) == 0
        {
            return None;
        }
        let plain = String::from_utf8(std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec()).ok();
        LocalFree(out.pbData as *mut core::ffi::c_void);
        plain
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn protect(_plain: &str) -> Option<String> {
    None
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(crate) fn unprotect(_hex: &str) -> Option<String> {
    None
}

// ---------------------------------------------------------------- macOS Keychain

/// macOS: the secret goes to the login Keychain; settings.json keeps only an opaque reference.
#[cfg(target_os = "macos")]
pub(crate) fn protect(plain: &str) -> Option<String> {
    keychain::protect(plain)
}

#[cfg(target_os = "macos")]
pub(crate) fn unprotect(reference: &str) -> Option<String> {
    keychain::unprotect(reference)
}

/// Saved passwords and login tokens on macOS (phase M1 of docs/PLANO-macOS.md).
///
/// - Every secret lives in ONE generic-password item of the login Keychain (service `concord`,
///   account `secrets`): a small JSON map from random ids to secrets. settings.json and
///   servers.json only hold `kc1:<id>` (32 random hex digits): no plaintext, no ciphertext.
/// - One item, not one per secret: the app is signed ad hoc, so after an update macOS asks once
///   ("Permitir Sempre") instead of once per saved password.
/// - The same secret saved again reuses its id, and [`keychain_gc`] (at startup) drops the ids no
///   file mentions any more, so the item does not grow with every login.
/// - Autotests with their own settings folder use their own account (`secrets-test-<hash>`), so
///   they never touch the real item. Unit tests keep the map in memory only (the real item
///   functions have their own test).
/// - Keychain unavailable (locked, access denied): nothing is saved (`None`), as before M1.
#[cfg(target_os = "macos")]
mod keychain {
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard};

    pub(super) const SERVICE: &str = "concord";
    pub(super) const PREFIX: &str = "kc1:";
    /// errSecItemNotFound
    const NOT_FOUND: i32 = -25300;

    /// The map as last read or written (`None`: not read yet).
    static CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

    fn account() -> String {
        match super::DIR.get() {
            Some(d) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                d.hash(&mut h);
                format!("secrets-test-{:016x}", h.finish())
            }
            None => "secrets".to_string(),
        }
    }

    /// A reference id: 16 random bytes, hex.
    fn new_id() -> Option<String> {
        use std::io::Read;
        let mut b = [0u8; 16];
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).ok()?;
        Some(super::to_hex(&b))
    }

    pub(super) fn is_reference(s: &str) -> bool {
        s.strip_prefix(PREFIX).is_some_and(|id| id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit()))
    }

    /// The item's bytes (`Ok(None)`: there is no item yet).
    pub(super) fn item_get(account: &str) -> Result<Option<Vec<u8>>, String> {
        match security_framework::passwords::get_generic_password(SERVICE, account) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(format!("{e} ({})", e.code())),
        }
    }

    pub(super) fn item_set(account: &str, bytes: &[u8]) -> Result<(), String> {
        security_framework::passwords::set_generic_password(SERVICE, account, bytes).map_err(|e| format!("{e} ({})", e.code()))
    }

    pub(super) fn item_delete(account: &str) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(SERVICE, account) {
            Err(e) if e.code() != NOT_FOUND => Err(format!("{e} ({})", e.code())),
            _ => Ok(()),
        }
    }

    #[cfg(not(test))]
    fn load() -> Result<HashMap<String, String>, String> {
        Ok(match item_get(&account())? {
            Some(b) => serde_json::from_slice(&b).map_err(|e| format!("damaged item: {e}"))?,
            None => HashMap::new(),
        })
    }

    #[cfg(not(test))]
    fn store(map: &HashMap<String, String>) -> Result<(), String> {
        let a = account();
        if map.is_empty() {
            return item_delete(&a);
        }
        item_set(&a, serde_json::to_string(map).map_err(|e| e.to_string())?.as_bytes())
    }

    // Unit tests never touch the user's Keychain.
    #[cfg(test)]
    fn load() -> Result<HashMap<String, String>, String> {
        Ok(HashMap::new())
    }

    #[cfg(test)]
    fn store(_map: &HashMap<String, String>) -> Result<(), String> {
        Ok(())
    }

    /// The cache, read from the Keychain on first use (`None`: it cannot be read).
    fn cached() -> Option<MutexGuard<'static, Option<HashMap<String, String>>>> {
        let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if cache.is_none() {
            match load() {
                Ok(m) => *cache = Some(m),
                Err(e) => {
                    tracing::warn!(error = %e, "keychain: cannot read the saved secrets");
                    return None;
                }
            }
        }
        Some(cache)
    }

    pub(super) fn protect(plain: &str) -> Option<String> {
        let mut cache = cached()?;
        let map = cache.as_mut()?;
        if let Some(id) = map.iter().find(|(_, v)| v.as_str() == plain).map(|(k, _)| k.clone()) {
            return Some(format!("{PREFIX}{id}"));
        }
        let id = new_id()?;
        map.insert(id.clone(), plain.to_string());
        match store(map) {
            Ok(()) => Some(format!("{PREFIX}{id}")),
            Err(e) => {
                map.remove(&id);
                tracing::warn!(error = %e, "keychain: cannot save a secret");
                None
            }
        }
    }

    pub(super) fn unprotect(reference: &str) -> Option<String> {
        if !is_reference(reference) {
            return None;
        }
        let id = reference[PREFIX.len()..].to_ascii_lowercase();
        cached()?.as_ref()?.get(&id).cloned()
    }

    /// Drop the entries whose reference is not in `text`; returns how many went.
    pub(super) fn keep_referenced(map: &mut HashMap<String, String>, text: &str) -> usize {
        let before = map.len();
        map.retain(|id, _| text.contains(&format!("{PREFIX}{id}")));
        before - map.len()
    }

    /// Forget every secret whose reference does not appear in `text` (the settings files).
    pub(super) fn gc(text: &str) {
        let Some(mut cache) = cached() else { return };
        let Some(map) = cache.as_mut() else { return };
        let dropped = keep_referenced(map, text);
        if dropped == 0 {
            return;
        }
        match store(map) {
            Ok(()) => tracing::info!(dropped, kept = map.len(), "keychain: unused saved secrets removed"),
            Err(e) => tracing::warn!(error = %e, "keychain: cannot clean up"),
        }
    }
}

/// macOS, at startup: drop the Keychain secrets that neither settings.json nor servers.json
/// mention any more (a password saved again gets a new reference). An unreadable file: nothing is
/// dropped.
#[cfg(target_os = "macos")]
pub fn keychain_gc() {
    let Some(p) = path() else { return };
    let read = |p: &Path| match std::fs::read_to_string(p) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Err(_) => None,
    };
    let (Some(a), Some(b)) = (read(&p), read(&servers_path(&p))) else { return };
    keychain::gc(&format!("{a}\n{b}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preview_of_my_own_live_is_on_by_default_and_old_files_get_that() {
        assert!(Settings::default().show_self_preview);
        // A file written before the switch existed.
        let old: Settings = serde_json::from_str(r#"{"server":"h","username":"ana"}"#).unwrap();
        assert!(old.show_self_preview);
        // Off survives the file.
        let off = Settings { show_self_preview: false, ..Default::default() };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.show_self_preview);
    }

    /// macOS: the real Keychain item functions (its own account, removed at the end), and a saved
    /// secret's reference is opaque and reused for the same secret.
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_item_roundtrip_and_opaque_references() {
        let account = format!("unit-test-{}", std::process::id());
        match keychain::item_set(&account, b"{\"x\":\"y\"}") {
            Ok(()) => {
                assert_eq!(keychain::item_get(&account).unwrap().as_deref(), Some(&b"{\"x\":\"y\"}"[..]));
                keychain::item_set(&account, b"{}").unwrap();
                assert_eq!(keychain::item_get(&account).unwrap().as_deref(), Some(&b"{}"[..]), "replaced in place");
                keychain::item_delete(&account).unwrap();
                assert_eq!(keychain::item_get(&account).unwrap(), None);
                keychain::item_delete(&account).unwrap();
            }
            // A session without a usable login Keychain (e.g. some SSH sessions): nothing to test.
            Err(e) => eprintln!("keychain unavailable here, item test skipped: {e}"),
        }
        let r = protect("senha do Mac").unwrap();
        assert!(keychain::is_reference(&r) && !r.contains("senha"), "{r}");
        assert_eq!(protect("senha do Mac"), Some(r.clone()), "the same secret keeps its reference");
        assert_ne!(protect("outra senha"), Some(r.clone()));
        assert_eq!(unprotect(&r).as_deref(), Some("senha do Mac"));
        assert_eq!(unprotect("kc1:00000000000000000000000000000000"), None);
        assert_eq!(unprotect("01000000d08c9ddf"), None, "a Windows DPAPI value means nothing here");
        // The startup clean-up keeps exactly what the files mention.
        let mut m: std::collections::HashMap<String, String> = [("a".repeat(32), "1".into()), ("b".repeat(32), "2".into())].into_iter().collect();
        let text = format!("{{\"password_enc\":\"kc1:{}\"}}", "a".repeat(32));
        assert_eq!(keychain::keep_referenced(&mut m, &text), 1);
        assert_eq!(m.keys().cloned().collect::<Vec<_>>(), vec!["a".repeat(32)]);
    }

    #[test]
    fn password_roundtrips_and_is_not_plain() {
        let enc = protect("s3nha secreta").expect("dpapi");
        assert!(!enc.contains("s3nha"));
        assert_eq!(unprotect(&enc).as_deref(), Some("s3nha secreta"));
        assert_eq!(unprotect("zz"), None);
    }

    #[test]
    fn settings_roundtrip_through_the_file_without_plaintext_password() {
        let dir = std::env::temp_dir().join(format!("concord-settings-test-{}", std::process::id()));
        // Only this test touches APPDATA.
        std::env::set_var("APPDATA", &dir);
        let s = Settings {
            server: "http://h:7890".into(),
            username: "ana".into(),
            remember_password: true,
            password_enc: protect("segredo-123"),
            ..Default::default()
        };
        s.save();
        let raw = std::fs::read_to_string(dir.join("concord").join("settings.json")).unwrap();
        assert!(!raw.contains("segredo-123"));
        let back = Settings::load();
        assert_eq!(back.username, "ana");
        assert_eq!(back.saved_password().as_deref(), Some("segredo-123"));
        // The single saved server became the list's entry, and its file has no plain password.
        assert_eq!(back.servers.servers.len(), 1);
        let list = std::fs::read_to_string(dir.join("concord").join("servers.json")).unwrap();
        assert!(!list.contains("segredo-123"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn defaults_when_fields_are_missing() {
        let s: Settings = serde_json::from_str(r#"{"server":"http://x:7890"}"#).unwrap();
        assert_eq!(s.server, "http://x:7890");
        assert_eq!(s.volume_pct, 100);
        assert!(s.share_audio);
        assert!(!s.stream_popout, "streams open inside the app by default");
        let s: Settings = serde_json::from_str(r#"{"stream_popout":true}"#).unwrap();
        assert!(s.stream_popout);
    }
    #[test]
    fn user_audio_is_kept_per_username_and_defaults_are_not_stored() {
        let mut s = Settings::default();
        assert!(s.user_audio("ana").is_default());
        let pref = UserAudioPref { volume_pct: 150, muted: true, stream_volume_pct: 40, stream_muted: false };
        s.set_user_audio("ana", pref);
        s.set_user_audio("bob", UserAudioPref { volume_pct: 30, ..Default::default() });
        assert_eq!(s.user_audio("ana"), pref);
        assert_eq!(s.user_audio("bob").volume_pct, 30);
        assert!(s.user_audio("cat").is_default());
        // Survives a save/load cycle through JSON, keyed by name.
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"ana\""));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_audio("ana"), pref);
        assert_eq!(back.user_audio("bob").volume_pct, 30);
        // Back to defaults removes the entry; huge values are clamped to 200 %.
        s.set_user_audio("ana", UserAudioPref::default());
        assert!(!s.user_audio.contains_key("ana"));
        s.set_user_audio("bob", UserAudioPref { volume_pct: 9000, stream_volume_pct: 201, ..Default::default() });
        assert_eq!((s.user_audio("bob").volume_pct, s.user_audio("bob").stream_volume_pct), (200, 200));
    }

    #[test]
    fn alert_settings_default_on_except_presence_and_roundtrip() {
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert_eq!(s.alerts, AlertSettings::default());
        assert!(s.alerts.enabled && s.alerts.room && s.alerts.streams && s.alerts.own_state && !s.alerts.presence);
        let mut s = s;
        s.alerts.volume_pct = 35;
        s.alerts.presence = true;
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!((back.alerts.volume_pct, back.alerts.presence), (35, true));
        let s: Settings = serde_json::from_str(r#"{"alerts":{"enabled":false}}"#).unwrap();
        assert!(!s.alerts.enabled && s.alerts.volume_pct == 60);
    }

    #[test]
    fn old_settings_files_and_partial_entries_load() {
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert!(s.user_audio.is_empty());
        let s: Settings = serde_json::from_str(r#"{"user_audio":{"ana":{"muted":true}}}"#).unwrap();
        let a = s.user_audio("ana");
        assert!(a.muted);
        assert_eq!((a.volume_pct, a.stream_volume_pct), (100, 100));
    }

    #[test]
    fn input_gate_defaults_to_automatic_and_roundtrips() {
        use vox_client_core::GateMode;
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert_eq!(s.input_gate, GateConfig::default());
        assert_eq!(s.input_gate.mode, GateMode::Auto);
        let mut s = s;
        s.input_gate = GateConfig { mode: GateMode::Manual, threshold_db: -38.0, hold_ms: 500 };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"manual\""));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.input_gate, s.input_gate);
        // A hand-edited file with silly values is clamped when applied.
        let s: Settings = serde_json::from_str(r#"{"input_gate":{"mode":"manual","threshold_db":-400.0,"hold_ms":0}}"#).unwrap();
        assert_eq!(s.input_gate.sanitized().threshold_db, vox_client_core::audio::gate::THRESHOLD_MIN_DB);
    }

    #[test]
    fn voice_fx_defaults_to_off_and_roundtrips() {
        use vox_client_core::VoiceEffect;
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.voice_fx, VoiceFxConfig { effect: VoiceEffect::Off, intensity_pct: 50 });
        let s = Settings { voice_fx: VoiceFxConfig { effect: VoiceEffect::Cave, intensity_pct: 80 }, ..Settings::default() };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""voice_fx":{"effect":"cave","intensity_pct":80}"#), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.voice_fx, s.voice_fx);
        // An effect this version does not know: off.
        let s: Settings = serde_json::from_str(r#"{"voice_fx":{"effect":"vocoder"}}"#).unwrap();
        assert_eq!(s.voice_fx.effect, VoiceEffect::Off);
    }

    #[test]
    fn noise_suppression_defaults_to_ai_light_and_roundtrips() {
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert_eq!(s.noise_suppression, NoiseLevel::AiLight);
        let mut s = s;
        s.noise_suppression = NoiseLevel::Standard;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"noise_suppression\":\"standard\""), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.noise_suppression, NoiseLevel::Standard);
        // IA máxima persists as "ai_max".
        let s: Settings = serde_json::from_str(r#"{"noise_suppression":"ai_max"}"#).unwrap();
        assert_eq!(s.noise_suppression, NoiseLevel::AiMax);
        assert!(serde_json::to_string(&s).unwrap().contains("\"noise_suppression\":\"ai_max\""));
        // A level this version does not know (a newer app's file) falls back to the default.
        let s: Settings = serde_json::from_str(r#"{"noise_suppression":"ai_ultra"}"#).unwrap();
        assert_eq!(s.noise_suppression, NoiseLevel::AiLight);
    }

    #[test]
    fn camera_settings_default_and_roundtrip() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!((s.camera_device.clone(), s.camera_preset), (None, CameraPreset::P720));
        let s = Settings {
            camera_device: Some(r"\\?\usb#vid_046d&pid_082d#{e5323777}\global".into()),
            camera_name: Some("HD Pro Webcam C920".into()),
            camera_preset: CameraPreset::P360,
            ..Default::default()
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.camera_device, s.camera_device);
        assert_eq!(back.camera_name, s.camera_name);
        assert_eq!(back.camera_preset, CameraPreset::P360);
        assert_eq!(CameraPreset::P1080.to_core().dims(), (1920, 1080, 30));
    }

    #[test]
    fn video_encoder_choice_defaults_to_automatic_hardware_and_roundtrips() {
        let s: Settings = serde_json::from_str(r#"{"server":"x"}"#).unwrap();
        assert!(s.hw_accel && s.video_encoder.is_none());
        assert_eq!(s.encoder_pref(), Default::default());
        let s = Settings { hw_accel: false, video_encoder: Some("nvenc:10de:1f99:00001234:0".into()), ..Default::default() };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(!back.hw_accel);
        assert_eq!(back.encoder_pref().encoder.as_deref(), Some("nvenc:10de:1f99:00001234:0"));
        assert_eq!(encoder_cache_path().map(|p| p.file_name().unwrap().to_owned()), Some("encoders.json".into()));
    }

    #[test]
    fn pref_converts_to_core_gains() {
        let c = UserAudioPref { volume_pct: 200, muted: true, stream_volume_pct: 50, stream_muted: false }.to_core();
        assert_eq!((c.volume, c.muted, c.stream_volume, c.stream_muted), (2.0, true, 0.5, false));
        assert!(UserAudioPref::default().to_core().is_default());
    }

    #[test]
    fn device_tokens_are_per_server_and_name_and_encrypted() {
        let mut s = Settings::default();
        s.set_device_token("http://1.2.3.4:7890", "Ana", Some("tok-ana"));
        s.set_device_token("http://1.2.3.4:7890", "Bia", Some("tok-bia"));
        assert_eq!(s.device_token("HTTP://1.2.3.4:7890 ", "ana").as_deref(), Some("tok-ana"));
        assert_eq!(s.device_token("http://other:7890", "Ana"), None);
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("tok-ana"), "stored encrypted");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.device_token("http://1.2.3.4:7890", "Bia").as_deref(), Some("tok-bia"));
        s.set_device_token("http://1.2.3.4:7890", "Ana", None);
        assert_eq!(s.device_token("http://1.2.3.4:7890", "Ana"), None);
        // Settings files from before item 12 load with none.
        let old: Settings = serde_json::from_str(r#"{"server":"x","username":"y"}"#).unwrap();
        assert!(old.device_tokens.is_empty());
    }
}
