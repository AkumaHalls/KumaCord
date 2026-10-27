//! kumacord-server: chat, presence, auth, LiveKit token issuing and LiveKit process supervision.

pub mod access;
pub mod admin;
pub mod api;
pub mod attachments;
pub mod auth;
pub mod avatar;
pub mod calls;
pub mod chat;
pub mod config;
pub mod db;
pub mod diag;
pub mod dms;
pub mod host;
pub mod ids;
pub mod invites;
pub mod ipv6;
pub mod job;
pub mod listen;
pub mod livekit;
pub mod livekit_admin;
pub mod linkpreview;
pub mod livekit_token;
pub mod owner;
pub mod profile;
pub mod serve;
pub mod sounds;
pub mod state;
pub mod util;
pub mod voice_guard;
pub mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub struct Options {
    pub data_dir: PathBuf,
    pub no_livekit: bool,
    pub livekit_bin: PathBuf,
    /// Overrides `bind` from config.toml (tests use `127.0.0.1:0`).
    pub bind_override: Option<String>,
}

/// Timings and limits a real server never changes (tests shorten or lower them).
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Use LiveKit's server API even with `no_livekit` (tests point `livekit.signal_port` at a
    /// fake LiveKit).
    pub livekit_api: bool,
    /// How often the hub compares LiveKit's rooms with its voice list (`voice_guard.rs`).
    pub voice_check_every: Duration,
    /// How long somebody may be in a room without being in the hub's list before being removed.
    pub voice_grace: Duration,
    /// v1.0.1: how often that comparison runs right after somebody lost a room (access to a
    /// private channel, a moderator's disconnect / move, a password reset), for as long as a join
    /// token from before could still get them back in (`voice_guard::watch_closely`).
    pub voice_check_fast: Duration,
    /// The HTTP listener's limits.
    pub limits: serve::Limits,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            livekit_api: false,
            voice_check_every: Duration::from_secs(5),
            voice_grace: Duration::from_secs(30),
            voice_check_fast: Duration::from_secs(1),
            limits: serve::Limits::default(),
        }
    }
}

pub struct Server {
    /// Address actually bound (the first one, see [`Server::addrs`]).
    pub addr: SocketAddr,
    /// v1.0.2: every address the hub listens on (`[::]:7890` dual-stack, or `0.0.0.0:7890` and
    /// `[::]:7890`, or the one `bind` names).
    pub addrs: Vec<SocketAddr>,
    /// One socket takes IPv4 and IPv6 (see `listen.rs`).
    pub dual_stack: bool,
    pub config: config::Config,
    state: state::AppState,
    shutdown_tx: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    slots: serve::Slots,
}

impl Server {
    /// HTTP connections open right now (tests).
    pub fn open_connections(&self) -> usize {
        self.slots.open()
    }

    /// The code the console shows while the owner has no personal password yet (tests).
    pub fn owner_setup_code(&self) -> Option<String> {
        self.state.ownership.setup_code()
    }

    /// The one-time code that claims the server while it has no owner (what the console shows).
    pub fn claim_code(&self) -> Option<String> {
        self.state.ownership.code()
    }

    /// Stop accepting, close websockets, stop LiveKit; waits (bounded) for everything to end.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        for t in self.tasks {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
    }
}

/// The runtime a hub runs on: ~15 users need very little (two workers, a small blocking pool).
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).max_blocking_threads(8).thread_name("hub").enable_all().build()
}

/// Run a hub until `until` (handed the running server) finishes, then shut it down cleanly. The
/// one entry point of `kumacord-server` (until Ctrl+C) and of the app's host mode (until "Parar
/// servidor" or the app closing, see [`host`]).
pub async fn serve_until<F: std::future::Future<Output = ()>>(opts: Options, until: impl FnOnce(&Server) -> F) -> Result<()> {
    let server = start(opts).await?;
    until(&server).await;
    server.shutdown().await;
    Ok(())
}

pub async fn start(opts: Options) -> Result<Server> {
    start_with(opts, linkpreview::PreviewConfig::default()).await
}

/// [`start`] with explicit link-preview settings (the defaults are right for a real server; tests
/// use this to point the fetcher at a local test web server).
pub async fn start_with(opts: Options, previews: linkpreview::PreviewConfig) -> Result<Server> {
    start_tuned(opts, previews, Tuning::default()).await
}

/// [`start_with`] with explicit [`Tuning`] (tests).
pub async fn start_tuned(opts: Options, previews: linkpreview::PreviewConfig, tuning: Tuning) -> Result<Server> {
    let cfg = config::load_or_create(&opts.data_dir)?;
    // The hub's reqwest is `rustls-no-provider` on purpose (the ring provider, no aws-lc in
    // the Docker build): install it before anything can build a client — `LiveKitAdmin::new`
    // does this too; this is the belt to its suspenders if startup is ever reordered.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let db = db::Db::open(&opts.data_dir.join("voxhub.db")).context("opening database")?;
    // Badges (v1.1): the tenure ladder and any first-members awards only move forward, and
    // a hub that was down on the day a tier crossed would still show the old one — so
    // everybody's materialized flags are recomputed at startup (cheap: one pass).
    {
        let owner = cfg.owner.clone();
        let changed = db.call(move |c| db::recompute_badges(c, &owner)).await?;
        if changed > 0 {
            tracing::info!(changed, "badges recomputados na inicialização");
        }
    }
    // A database a newer hub marked as needing it (`access.rs`): say so loudly.
    if let Some(wanted) = db.call(|c| access::min_hub_version(c)).await? {
        if access::too_old(access::PRIVATE_MIN_HUB_VERSION, &wanted) {
            tracing::error!(wanted, "this database was marked for a server {wanted} or newer: update this server");
            eprintln!("ATENÇÃO: este banco de dados pede o servidor {wanted} ou mais novo. Atualize o servidor.");
        }
    }
    let channels = db.call(|c| db::list_channels(c)).await?;
    let server_id = db.call(|c| db::server_id(c)).await?;
    let server_muted = db.call(|c| db::server_muted_ids(c)).await?;

    let bind = opts.bind_override.as_deref().unwrap_or(&cfg.bind);
    let bound = listen::bind(&listen::plan(bind, cfg.ipv6)).await.with_context(|| format!("binding {bind}"))?;
    let (addr, addrs, dual_stack) = (bound.addrs[0], bound.addrs.clone(), bound.dual_stack);

    tracing::info!(
        server = %cfg.server_name,
        %addr,
        listening = %bound.describe(),
        public_host = %cfg.public_host,
        password = !cfg.server_password.is_empty(),
        livekit = !opts.no_livekit,
        signal_port = cfg.livekit.signal_port,
        rtc_tcp = cfg.livekit.rtc_tcp_port,
        rtc_udp = cfg.livekit.rtc_udp_port,
        external_ip = cfg.livekit.use_external_ip,
        data_dir = %opts.data_dir.display(),
        "kumacord-server starting"
    );
    if cfg.public_host == "127.0.0.1" {
        tracing::warn!("public_host is 127.0.0.1: only this PC can join voice; set it in config.toml");
    }
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = Vec::new();

    if !opts.no_livekit {
        let yaml = livekit::write_config(&cfg, &opts.data_dir)?;
        tasks.push(tokio::spawn(livekit::supervise(opts.livekit_bin, yaml, shutdown_rx.clone())));
    }

    let previews = linkpreview::LinkPreviews::new(previews, db.clone(), &cfg.public_host).context("starting link previews")?;
    let lk_admin = livekit_admin::LiveKitAdmin::new(&cfg, !opts.no_livekit || tuning.livekit_api).context("LiveKit server API client")?;
    let attachments = attachments::AttachmentState::new(&opts.data_dir, &cfg);
    let slots = serve::Slots::new(tuning.limits.clone());
    let state = state::Shared::new(cfg.clone(), db, server_id, channels, server_muted, previews, lk_admin, attachments, tuning, shutdown_rx.clone());
    owner::init(&state).await?;
    // Private channels: everybody's effective role, before anybody connects.
    access::init_roles(&state).await?;
    tasks.push(tokio::spawn(owner::watch(state.clone(), shutdown_rx.clone())));
    tasks.push(tokio::spawn(profile::watch(state.clone(), shutdown_rx.clone())));
    tasks.push(tokio::spawn(attachments::watch(state.clone(), shutdown_rx.clone())));
    tasks.push(tokio::spawn(voice_guard::watch(state.clone(), shutdown_rx.clone())));
    let app = api::router(state.clone());
    // One accept loop per socket (two without a dual-stack one); the connection caps are shared.
    for listener in bound.listeners {
        tasks.push(tokio::spawn(serve::serve(listener, app.clone(), slots.clone(), shutdown_rx.clone())));
    }

    Ok(Server { addr, addrs, dual_stack, config: cfg, state, shutdown_tx, tasks, slots })
}
