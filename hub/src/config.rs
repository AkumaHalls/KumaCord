//! `<data-dir>/config.toml`: created with defaults on first run.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::util::random_hex;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server_name: String,
    /// Shared password clients must send on login. Empty = open server.
    pub server_password: String,
    pub bind: String,
    /// Host/IP clients use to reach LiveKit (the host's public IP or DDNS name).
    pub public_host: String,
    /// The owner ("Dono", item 12): this username has every permission. Compared
    /// case-insensitively. Empty (the usual case) = "quem hospeda vira dono": the first login from
    /// the server PC itself (or whoever uses the claim code the console shows) becomes the owner,
    /// stored in the database. Set, it pins the owner and nothing can claim or transfer it.
    pub owner: String,
    /// Chat images: total storage cap, in GB (default 5). When it is reached new images are
    /// refused with a friendly message (or, with `attachment_retention_days`, the oldest images
    /// older than a day make room).
    pub max_attachment_storage_gb: f64,
    /// Chat images each person may upload per day (UTC), in MB (default 200).
    pub attachment_daily_mb: u64,
    /// Delete chat images older than this many days (the messages stay, with "[imagem]").
    /// 0 (the default) keeps them forever.
    pub attachment_retention_days: u32,
    pub livekit: LiveKitConfig,
}

impl Config {
    /// [`Config::max_attachment_storage_gb`] in bytes.
    pub fn attachment_storage_cap(&self) -> u64 {
        let gb = if self.max_attachment_storage_gb.is_finite() { self.max_attachment_storage_gb.max(0.0) } else { 5.0 };
        (gb * 1024.0 * 1024.0 * 1024.0) as u64
    }

    /// [`Config::attachment_daily_mb`] in bytes.
    pub fn attachment_daily_bytes(&self) -> u64 {
        self.attachment_daily_mb.saturating_mul(1024 * 1024)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LiveKitConfig {
    pub api_key: String,
    pub api_secret: String,
    /// Signal (HTTP/WebSocket) port.
    pub signal_port: u16,
    /// RTC over TCP (fallback when UDP is blocked).
    pub rtc_tcp_port: u16,
    /// RTC over UDP, single-port mux.
    pub rtc_udp_port: u16,
    /// Let LiveKit discover its public IP via STUN. Needed for remote peers behind port forwarding.
    pub use_external_ip: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_name: "concord".into(),
            server_password: String::new(),
            bind: "0.0.0.0:7890".into(),
            public_host: "127.0.0.1".into(),
            owner: String::new(),
            max_attachment_storage_gb: 5.0,
            attachment_daily_mb: 200,
            attachment_retention_days: 0,
            livekit: LiveKitConfig::default(),
        }
    }
}

impl Default for LiveKitConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            api_secret: String::new(),
            signal_port: 7880,
            rtc_tcp_port: 7881,
            rtc_udp_port: 7882,
            use_external_ip: false,
        }
    }
}

/// Load `config.toml` from `dir`, writing a fresh one (with random LiveKit keys) if missing.
pub fn load_or_create(dir: &Path) -> Result<Config> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("config.toml");
    if !path.exists() {
        let text = default_file(&random_hex::<8>(), &random_hex::<32>());
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(path = %path.display(), "created default config.toml");
    }
    let text = std::fs::read_to_string(&path)?;
    let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if cfg.livekit.api_key.is_empty() || cfg.livekit.api_secret.len() < 32 {
        anyhow::bail!("{}: livekit.api_key must be set and livekit.api_secret must be at least 32 chars", path.display());
    }
    cfg.public_host = cfg.public_host.trim().to_string();
    cfg.owner = cfg.owner.trim().to_string();
    Ok(cfg)
}

fn default_file(key: &str, secret: &str) -> String {
    format!(
        r#"# concord-server configuration. Restart the server after editing.

server_name = "concord"
# Shared password for logging in. Empty = anyone who can reach the server may join.
server_password = ""
bind = "0.0.0.0:7890"
# Host/IP that clients use to reach LiveKit: your public IP or DDNS name.
public_host = "127.0.0.1"
# The server owner ("Dono") can do everything, including making Admins. You normally do not need
# this: whoever hosts the server becomes the owner by logging in from this very PC (address
# 127.0.0.1), and the console window also shows a one-time code to claim it from anywhere.
# Writing `owner = "your-name"` here instead pins the owner by name (it can then no longer be
# claimed or transferred in the app). Either way, the owner's powers stay off until the app has
# asked for, and got, a personal password. Lost it? Run
#   concord-server --reset-password <name>
# and log in with the one-time code it prints. To forget the owner and start over:
#   concord-server --reset-owner
# owner = "seu_nome"

# Images in the chat. The app compresses them before sending (a photo is usually 200-500 KB).
# Total space for images, in GB; when it is full new images are refused.
max_attachment_storage_gb = 5
# What each person may send per day, in MB.
attachment_daily_mb = 200
# Delete images older than this many days (0 = keep forever). With it set, a full storage also
# makes room by deleting the oldest images.
attachment_retention_days = 0

[livekit]
# Generated on first run. Keep secret.
api_key = "API{key}"
api_secret = "{secret}"
signal_port = 7880
rtc_tcp_port = 7881
rtc_udp_port = 7882
# Set to true so LiveKit advertises your public IP (needed for friends outside your LAN).
use_external_ip = false
"#
    )
}
