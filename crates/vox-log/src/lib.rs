//! Shared logging setup for every concord binary.
//!
//! Policy (keeps logs useful for bug hunting without bloating disk or CPU):
//! - Default level is `info`; override with the `VOX_LOG` env var (same syntax as `RUST_LOG`,
//!   e.g. `VOX_LOG=debug` or `VOX_LOG=info,vox_capture=trace`).
//! - Files rotate daily and only the newest `MAX_FILES` are kept.
//! - Writes go through a non-blocking background thread, so logging never stalls audio/video.
//! - Hot paths (per frame / per audio packet) must NOT log. Emit aggregated stats instead,
//!   at most every `STATS_INTERVAL` (see [`RateLimiter`]).
//! - Panics are logged before the process dies.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub const MAX_FILES: usize = 5;
pub const STATS_INTERVAL: Duration = Duration::from_secs(30);
pub const ENV_VAR: &str = "VOX_LOG";
/// SDK `log` records are bridged into tracing (the `tracing-log` feature), so the LiveKit SDK's
/// own warnings and errors reach the file too.
///
/// The one thing the SDK only logs at debug is the PeerConnection / ICE state (`connection
/// change, Connecting Publisher`), and that is exactly what explains a join that never completes
/// (Connecting, then Failed). The rest of that module's debug output dumps SDP and every
/// candidate, far too much for a normal log, so [`IceStateLayer`] picks just that line out and
/// re-emits it at info.
const ICE_STATE_SOURCE: &str = "livekit::rtc_engine::rtc_session";

struct IceStateLayer;

#[derive(Default)]
struct MessageText(String);

impl tracing::field::Visit for MessageText {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.0, "{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for IceStateLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut text = MessageText::default();
        event.record(&mut text);
        if let Some(state) = text.0.strip_prefix("connection change, ") {
            tracing::info!(target: "livekit::ice", "PeerConnection state: {state}");
        }
    }
}

/// Keep this alive for the whole program; dropping it flushes and stops the log writer.
pub struct LogGuard {
    _file: WorkerGuard,
    pub log_dir: PathBuf,
}

/// Default log directory: `%LOCALAPPDATA%\concord\logs\<app>` on Windows, `./logs/<app>` elsewhere.
pub fn default_log_dir(app: &str) -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("concord").join("logs").join(app))
        .unwrap_or_else(|| PathBuf::from("logs").join(app))
}

/// Initialise logging to a rotating file (always) and stderr (when `console` is true).
pub fn init(app: &str, log_dir: Option<&Path>, console: bool) -> LogGuard {
    let log_dir = log_dir.map(Path::to_path_buf).unwrap_or_else(|| default_log_dir(app));
    let _ = std::fs::create_dir_all(&log_dir);

    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(app)
        .filename_suffix("log")
        .max_log_files(MAX_FILES)
        .build(&log_dir)
        .expect("failed to create log file appender");
    let (file_writer, guard) = tracing_appender::non_blocking(appender);

    let filter = || EnvFilter::try_from_env(ENV_VAR).unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_names(true)
        .compact()
        .with_filter(filter());

    let console_layer = console.then(|| {
        fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .compact()
            .with_filter(filter())
    });

    let ice_layer = IceStateLayer.with_filter(EnvFilter::new(format!("{ICE_STATE_SOURCE}=debug")));

    tracing_subscriber::registry().with(file_layer).with(console_layer).with(ice_layer).init();

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "panic", "{info}");
        prev(info);
    }));

    tracing::info!(app, version = env!("CARGO_PKG_VERSION"), dir = %log_dir.display(), "logging started");
    LogGuard { _file: guard, log_dir }
}

/// Lock-free "at most once per interval" gate for stats logging from hot paths.
///
/// ```ignore
/// static STATS: RateLimiter = RateLimiter::new();
/// if STATS.ready(vox_log::STATS_INTERVAL) { tracing::info!(fps, "capture stats"); }
/// ```
pub struct RateLimiter {
    last_ms: AtomicU64,
}

impl RateLimiter {
    pub const fn new() -> Self {
        Self { last_ms: AtomicU64::new(0) }
    }

    pub fn ready(&self, every: Duration) -> bool {
        let now = now_ms();
        let last = self.last_ms.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < every.as_millis() as u64 {
            return false;
        }
        self.last_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    // +1 so the first call never reads as "0 = never fired".
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}
