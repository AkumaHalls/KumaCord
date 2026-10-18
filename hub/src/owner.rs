//! "Quem hospeda vira dono": who the server's owner (Dono) is, and how somebody becomes one.
//!
//! - `owner = "name"` in `config.toml` pins the owner (the manual override: nothing below applies).
//! - Otherwise the owner is stored in SQLite (`server_meta.owner_user_id`) and starts out empty:
//!   - **The server PC claims it.** The first login (or authenticated connection) whose TCP peer
//!     is a loopback address, i.e. the app running on the host's own PC, becomes the owner. The
//!     decision is [`is_host_pc`]: the socket's peer address, never a header. Headers can only
//!     *veto* (a reverse proxy or tunnel on the same PC makes every remote user look like
//!     loopback; its `Forwarded` / `X-Forwarded-*` headers, or a `Host` that is not a loopback
//!     name, turn the automatic claim off for that request).
//!   - **The claim code.** While there is no owner the hub prints a one-time code in its console
//!     (and log). Anyone logged in can send it to `POST /api/owner/claim`. It is valid until
//!     somebody claims, is new on every start, compared in constant time, and wrong codes are
//!     slowed down (per address and for the whole server).
//! - `concord-server --reset-owner` clears the stored owner (a running hub notices within
//!   [`WATCH_EVERY`]); then the host can claim it again from the server PC.
//!
//! **The owner's first personal password.** The owner's powers stay off until the owner's name
//! has a personal password, and anybody with the server password can log in as a name that has
//! none. So the *first* password of the owner's name needs the same proof as a claim
//! ([`check_first_password`]): a request from the server PC, the very session that claimed the
//! server, or the setup code the console shows while the owner has no password.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use vox_proto::*;

use crate::admin::audit;
use crate::api::{too_many, ApiErr, Authed};
use crate::auth::ip_key;
use crate::db::{self, UserRow};
use crate::state::{self, AppState};
use crate::util::{ct_eq, random_bytes};

/// How often a running hub looks at the database for an owner changed behind its back
/// (`--reset-owner`).
pub const WATCH_EVERY: Duration = Duration::from_secs(2);

/// Throttle key shared by every address: a wrong claim code slows the whole server down, so
/// guessing cannot be spread over addresses.
const GLOBAL_KEY: &str = "claim:global";
/// Same for the setup code of the owner's first password.
const SETUP_GLOBAL_KEY: &str = "setup:global";

/// Crockford base32: no I, L, O, U.
const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 8;

/// The owner, as the hub knows it right now.
pub struct OwnerState {
    pinned: bool,
    /// The owner's username ("" = none). Never changes when `pinned`.
    name: RwLock<String>,
    /// The pending claim code (normalised), while there is no owner.
    code: Mutex<Option<String>>,
    /// The setup code (normalised) while the owner has no personal password yet.
    setup: Mutex<Option<String>>,
    /// Sessions (token hashes) that proved they are the host when claiming.
    proven: Mutex<HashSet<String>>,
}

impl OwnerState {
    /// `config_owner` is `config.toml`'s `owner` (empty = not pinned).
    pub fn new(config_owner: &str) -> Self {
        OwnerState {
            pinned: !config_owner.is_empty(),
            name: RwLock::new(config_owner.to_string()),
            code: Mutex::new(None),
            setup: Mutex::new(None),
            proven: Mutex::new(HashSet::new()),
        }
    }

    /// `owner = "..."` in `config.toml` decides; the app can neither claim nor transfer.
    pub fn pinned(&self) -> bool {
        self.pinned
    }

    /// The owner's username ("" = none yet).
    pub fn name(&self) -> String {
        self.name.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn has_owner(&self) -> bool {
        !self.name.read().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    fn set(&self, name: &str) {
        if !self.pinned {
            *self.name.write().unwrap_or_else(|e| e.into_inner()) = name.to_string();
        }
    }

    /// A fresh code replaces any older one. Returns it as shown (`XXXX-XXXX`).
    fn new_code(&self) -> String {
        let bytes = random_bytes::<CODE_LEN>();
        let norm: String = bytes.iter().map(|b| CODE_ALPHABET[(*b & 31) as usize] as char).collect();
        *self.code.lock().unwrap_or_else(|e| e.into_inner()) = Some(norm.clone());
        display_code(&norm)
    }

    fn clear_code(&self) {
        *self.code.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The pending code as shown, if any (the hub's own console, and the tests).
    pub fn code(&self) -> Option<String> {
        self.code.lock().unwrap_or_else(|e| e.into_inner()).as_deref().map(display_code)
    }

    /// Is `offered` the pending code? Constant time.
    fn check_code(&self, offered: &str) -> bool {
        let g = self.code.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_deref() {
            Some(code) => ct_eq(&normalize_code(offered), code),
            None => false,
        }
    }

    /// The setup code as shown, while the owner has no personal password (console, tests).
    pub fn setup_code(&self) -> Option<String> {
        self.setup.lock().unwrap_or_else(|e| e.into_inner()).as_deref().map(display_code)
    }

    /// A setup code for the owner's first password; the existing one if there is one already.
    /// Returns it as shown and whether it is new.
    fn ensure_setup_code(&self) -> (String, bool) {
        let mut g = self.setup.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_deref() {
            return (display_code(c), false);
        }
        let bytes = random_bytes::<CODE_LEN>();
        let norm: String = bytes.iter().map(|b| CODE_ALPHABET[(*b & 31) as usize] as char).collect();
        *g = Some(norm.clone());
        (display_code(&norm), true)
    }

    fn check_setup_code(&self, offered: &str) -> bool {
        let g = self.setup.lock().unwrap_or_else(|e| e.into_inner());
        g.as_deref().is_some_and(|code| ct_eq(&normalize_code(offered), code))
    }

    /// The owner's name now has a personal password: no setup code, no special sessions.
    pub fn owner_protected(&self) {
        *self.setup.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.proven.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    fn prove(&self, session_hash: &str) {
        let mut g = self.proven.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() < 64 {
            g.insert(session_hash.to_string());
        }
    }

    fn is_proven(&self, session_hash: &str) -> bool {
        self.proven.lock().unwrap_or_else(|e| e.into_inner()).contains(session_hash)
    }
}

fn display_code(norm: &str) -> String {
    format!("{}-{}", &norm[..CODE_LEN / 2], &norm[CODE_LEN / 2..])
}

/// People type codes: ignore case, dashes and spaces; Crockford's look-alikes (O is 0, I and L are 1).
pub fn normalize_code(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

// ---------------------------------------------------------------- the server PC

/// Is this request coming from the app on the server PC itself?
///
/// True only when the TCP peer is a loopback address (127.0.0.0/8, `::1`, or the IPv4 loopback in
/// its `::ffff:` form). Logins over the LAN or the public IP of the same machine come from that
/// address, not from loopback, so they do not count: the host's app connects through `127.0.0.1`.
///
/// Headers never grant anything, they only veto: a request carrying `Forwarded`, `X-Forwarded-*`
/// or `X-Real-IP` went through a proxy (so the loopback peer is the proxy, not the user), and a
/// `Host` that is not `localhost` / a loopback address went through a tunnel or a name that is
/// not the server PC's own loopback.
pub fn is_host_pc(peer: IpAddr, headers: &HeaderMap) -> bool {
    if !peer.to_canonical().is_loopback() {
        return false;
    }
    const PROXY_HEADERS: [&str; 5] = ["forwarded", "x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "x-real-ip"];
    if PROXY_HEADERS.iter().any(|h| headers.contains_key(*h)) {
        return false;
    }
    match headers.get(axum::http::header::HOST) {
        None => true,
        Some(h) => h.to_str().is_ok_and(host_is_loopback),
    }
}

/// `127.0.0.1:7890`, `localhost`, `[::1]:7890`.
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        match host.rsplit_once(':') {
            Some((h, port)) if !h.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => h,
            _ => host,
        }
    };
    name.eq_ignore_ascii_case("localhost") || name.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

// ---------------------------------------------------------------- claiming

#[derive(Debug, Clone, Copy)]
enum Via {
    HostPc,
    Code,
}

impl Via {
    fn detail(self) -> &'static str {
        match self {
            Via::HostPc => "pc-do-servidor",
            Via::Code => "codigo",
        }
    }
}

/// Make `row` the owner if the server has none. Returns whether it won (of two simultaneous
/// claims exactly one does). `session`: the session that proved it (it may then choose the
/// owner's first personal password).
async fn claim(s: &AppState, row: &UserRow, via: Via, session: Option<&str>) -> anyhow::Result<bool> {
    if s.ownership.pinned() {
        return Ok(false);
    }
    let id = row.id;
    if !s.db.call(move |c| db::claim_owner(c, id)).await? {
        refresh(s).await?;
        return Ok(false);
    }
    s.ownership.set(&row.username);
    s.ownership.clear_code();
    if let Some(h) = session {
        s.ownership.prove(h);
    }
    tracing::info!(user = %row.username, id, via = via.detail(), "server claimed: this user is now the owner");
    audit(s, Some(id), AUDIT_CLAIM_OWNER, None, via.detail().to_string()).await;
    s.update_user(&row.user(&s.owner()));
    crate::access::reload_roles(s).await;
    if !row.has_password() {
        announce_setup(s);
    }
    Ok(true)
}

/// The automatic claim: `row` just logged in (or connected) and, if the server has no owner and
/// the request came from the server PC itself, they become the owner. Returns whether they did.
pub async fn claim_from_host_pc(s: &AppState, row: &UserRow, peer: IpAddr, headers: &HeaderMap, session: Option<&str>) -> anyhow::Result<bool> {
    if s.ownership.pinned() || s.ownership.has_owner() || !is_host_pc(peer, headers) {
        return Ok(false);
    }
    claim(s, row, Via::HostPc, session).await
}

/// The owner's name has no personal password yet and `me` (a session of it) wants to set the
/// first one: allowed from the server PC, from the session that claimed the server, or with the
/// setup code the console shows (wrong codes are slowed down like claim codes).
pub async fn check_first_password(s: &AppState, me: &Authed, peer: IpAddr, headers: &HeaderMap, code: Option<&str>) -> Result<(), ApiErr> {
    // A name under a reset code (`--reset-password`, printed on the host) only has sessions that
    // logged in with that code: the code was the proof.
    if is_host_pc(peer, headers) || s.ownership.is_proven(&me.session_hash) || me.row.reset_pending() {
        return Ok(());
    }
    let Some(code) = code.filter(|c| !c.trim().is_empty()) else {
        tracing::warn!(%peer, user = %me.user.username, "owner's first password refused: not from the server PC and no code");
        return Err(ApiErr::new(StatusCode::FORBIDDEN, ERR_OWNER_PROOF_REQUIRED));
    };
    let ip = format!("setup:{}", ip_key(peer));
    for key in [ip.as_str(), SETUP_GLOBAL_KEY] {
        if let Some(wait) = s.throttle.must_wait(key) {
            return Err(too_many(wait));
        }
    }
    if !s.ownership.check_setup_code(code) {
        tracing::warn!(%peer, user = %me.user.username, "owner's first password refused: wrong code");
        s.throttle.fail(&ip);
        s.throttle.fail(SETUP_GLOBAL_KEY);
        tokio::time::sleep(Duration::from_millis(300)).await;
        return Err(ApiErr::new(StatusCode::FORBIDDEN, "wrong claim code"));
    }
    s.throttle.succeed(&ip);
    s.throttle.succeed(SETUP_GLOBAL_KEY);
    Ok(())
}

/// `POST /api/owner/claim`
pub async fn claim_with_code(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    me: Authed,
    body: Result<Json<ClaimOwnerRequest>, JsonRejection>,
) -> Result<Json<ClaimOwnerResponse>, ApiErr> {
    let Json(req) = body?;
    if s.ownership.pinned() {
        return Err(ApiErr::new(StatusCode::CONFLICT, "the owner is set in config.toml"));
    }
    if s.ownership.has_owner() {
        return Err(ApiErr::new(StatusCode::CONFLICT, "this server already has an owner"));
    }
    let ip = format!("claim:{}", ip_key(peer.ip()));
    for key in [ip.as_str(), GLOBAL_KEY] {
        if let Some(wait) = s.throttle.must_wait(key) {
            tracing::warn!(%peer, user = %me.user.username, "claim refused: being slowed down");
            return Err(too_many(wait));
        }
    }
    if !s.ownership.check_code(&req.code) {
        tracing::warn!(%peer, user = %me.user.username, "claim rejected: wrong code");
        s.throttle.fail(&ip);
        s.throttle.fail(GLOBAL_KEY);
        tokio::time::sleep(Duration::from_millis(300)).await;
        return Err(ApiErr::new(StatusCode::FORBIDDEN, "wrong claim code"));
    }
    s.throttle.succeed(&ip);
    s.throttle.succeed(GLOBAL_KEY);
    if !claim(&s, &me.row, Via::Code, Some(&me.session_hash)).await? {
        return Err(ApiErr::new(StatusCode::CONFLICT, "this server already has an owner"));
    }
    let id = me.row.id;
    let row = s.db.call(move |c| db::get_user(c, id)).await?.ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))?;
    Ok(Json(ClaimOwnerResponse { user: row.user(&s.owner()), account: s.account(&row) }))
}

/// `POST /api/admin/transfer-owner`
pub async fn transfer(State(s): State<AppState>, me: Authed, body: Result<Json<TransferOwnerRequest>, JsonRejection>) -> Result<Json<OkResponse>, ApiErr> {
    let Json(req) = body?;
    if me.role != Role::Owner {
        return Err(ApiErr::new(StatusCode::FORBIDDEN, "your role does not allow this"));
    }
    if s.ownership.pinned() {
        return Err(ApiErr::new(StatusCode::CONFLICT, "the owner is set in config.toml: change it there"));
    }
    let to = req.user_id;
    if to == me.user.id {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "you are already the owner"));
    }
    let target = s.db.call(move |c| db::get_user(c, to)).await?.ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))?;
    if !target.has_password() {
        return Err(ApiErr::new(StatusCode::CONFLICT, "they must set a personal password first"));
    }
    let from = me.user.id;
    if !s.db.call(move |c| db::transfer_owner(c, from, to)).await? {
        return Err(ApiErr::new(StatusCode::CONFLICT, "the ownership changed meanwhile"));
    }
    s.ownership.set(&target.username);
    tracing::info!(from = %me.user.username, to = %target.username, "ownership transferred");
    audit(&s, Some(from), AUDIT_TRANSFER_OWNER, Some(to), String::new()).await;
    let owner = s.owner();
    for id in [from, to] {
        if let Some(row) = s.db.call(move |c| db::get_user(c, id)).await? {
            s.update_user(&row.user(&owner));
        }
    }
    crate::access::reload_roles(&s).await;
    Ok(Json(OkResponse { ok: true }))
}

// ---------------------------------------------------------------- startup and watching

/// Read the stored owner at start. With none, make a code and print it where the host looks.
/// With an owner whose name has no personal password yet, print the setup code for it.
pub async fn init(s: &AppState) -> anyhow::Result<()> {
    if s.ownership.pinned() {
        tracing::info!(owner = %s.owner(), "server owner (config.toml)");
        let name = s.owner();
        let row = s.db.call(move |c| db::find_user(c, &name)).await?;
        if !row.is_some_and(|r| r.has_password()) {
            announce_setup(s);
        }
        return Ok(());
    }
    let stored = s.db.call(|c| db::owner_row(c)).await?;
    match stored {
        Some(row) => {
            s.ownership.set(&row.username);
            tracing::info!(owner = %row.username, "server owner");
            if !row.has_password() {
                announce_setup(s);
            }
        }
        None => announce(s),
    }
    Ok(())
}

/// The owner has no personal password yet: a setup code for choosing it from another PC, in the
/// log and in the console window.
fn announce_setup(s: &AppState) {
    let (code, new) = s.ownership.ensure_setup_code();
    if !new {
        return;
    }
    tracing::warn!(code, owner = %s.owner(), "the owner has no personal password yet: set it in the app on this PC (127.0.0.1), or from another PC with this code");
    eprintln!();
    eprintln!("==============================================================================");
    eprintln!("  O dono ({}) ainda não tem senha pessoal. Defina-a no app deste PC", s.owner());
    eprintln!("  (endereço 127.0.0.1) ou, de outro PC, com o código: {code}");
    eprintln!("==============================================================================");
    eprintln!();
}

/// No owner: a new code, in the log and in the console window.
fn announce(s: &AppState) {
    let code = s.ownership.new_code();
    tracing::warn!(code, "no owner yet: whoever logs in from the server PC (127.0.0.1) becomes the owner, or anyone can claim it with this code");
    eprintln!();
    eprintln!("==============================================================================");
    eprintln!("  Nenhum dono ainda. Para virar dono, entre no app e use o código: {code}");
    eprintln!("  (ou entre pelo próprio PC do servidor: use o endereço 127.0.0.1)");
    eprintln!("==============================================================================");
    eprintln!();
}

/// Compare the stored owner with what this hub believes and fix the difference (the owner was
/// cleared by `--reset-owner`, or somebody else won a claim race): tell everybody whose role changed.
async fn refresh(s: &AppState) -> anyhow::Result<()> {
    if s.ownership.pinned() {
        return Ok(());
    }
    let stored = s.db.call(|c| db::owner_row(c)).await?;
    let before = s.ownership.name();
    let after = stored.as_ref().map(|r| r.username.clone()).unwrap_or_default();
    if before == after {
        return Ok(());
    }
    s.ownership.set(&after);
    tracing::info!(from = before, to = after, "the stored owner changed");
    let owner = s.owner();
    if !before.is_empty() {
        let name = before.clone();
        if let Some(row) = s.db.call(move |c| db::find_user(c, &name)).await? {
            s.update_user(&row.user(&owner));
        }
    }
    s.ownership.owner_protected();
    match stored {
        Some(row) => {
            s.ownership.clear_code();
            s.update_user(&row.user(&owner));
            if !row.has_password() {
                announce_setup(s);
            }
        }
        None => announce(s),
    }
    Ok(())
}

/// Background task: notice `--reset-owner` while the hub runs.
pub async fn watch(s: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(WATCH_EVERY);
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Err(e) = refresh(&s).await {
                    tracing::warn!("owner check failed: {e:#}");
                }
                // Private channels follow roles changed outside this hub too (`--reset-password`,
                // `--reset-owner`); nothing is sent unless something changed.
                crate::access::reload_roles(&s).await;
            }
            _ = state::wait_shutdown(&mut shutdown) => return,
        }
    }
}

// ---------------------------------------------------------------- command line

/// `concord-server --reset-owner`: forget the owner. Safe while the server runs.
pub fn reset_owner_cli(data_dir: &std::path::Path) -> anyhow::Result<String> {
    let path = data_dir.join("voxhub.db");
    if !path.exists() {
        anyhow::bail!("no database at {} (wrong --data-dir?)", path.display());
    }
    let db = db::Db::open(&path)?;
    let (had, name) = db.call_now(|c| {
        let name = db::owner_row(c)?.map(|r| r.username);
        let had = db::clear_owner(c)?;
        if had {
            db::audit(c, None, AUDIT_RESET_OWNER, None, name.as_deref().unwrap_or(""))?;
        }
        Ok((had, name))
    })?;
    Ok(if had {
        format!(
            "O dono do servidor ({}) foi removido. O servidor fica sem dono: quem entrar no app pelo próprio PC do servidor (127.0.0.1) vira o novo dono, \
             ou use o código que o servidor mostra na janela dele.\n\
             Se o dono estiver no config.toml (owner = \"...\"), tire essa linha: ela continua valendo.",
            name.unwrap_or_default()
        )
    } else {
        "O servidor não tinha dono guardado. Se o dono está no config.toml (owner = \"...\"), tire essa linha ou troque o nome por lá.".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn only_a_loopback_peer_is_the_server_pc() {
        let none = HeaderMap::new();
        for ip in ["127.0.0.1", "127.5.6.7", "::1", "::ffff:127.0.0.1"] {
            assert!(is_host_pc(ip.parse().unwrap(), &none), "{ip}");
        }
        for ip in ["192.168.0.10", "10.0.0.2", "8.8.8.8", "169.254.1.1", "::ffff:10.0.0.1", "fe80::1", "2001:db8::1", "0.0.0.0"] {
            assert!(!is_host_pc(ip.parse().unwrap(), &none), "{ip}");
        }
    }

    #[test]
    fn headers_can_veto_but_never_grant() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let lan: IpAddr = "192.168.0.10".parse().unwrap();
        // A loopback Host is normal for the host's own app.
        assert!(is_host_pc(lo, &headers(&[("host", "127.0.0.1:7890")])));
        assert!(is_host_pc(lo, &headers(&[("host", "localhost:7890")])));
        assert!(is_host_pc(lo, &headers(&[("host", "[::1]:7890")])));
        // A proxy / tunnel on the same PC: the loopback peer is the proxy.
        assert!(!is_host_pc(lo, &headers(&[("host", "meu-servidor.example.com")])));
        assert!(!is_host_pc(lo, &headers(&[("host", "127.0.0.1:7890"), ("x-forwarded-for", "203.0.113.9")])));
        assert!(!is_host_pc(lo, &headers(&[("forwarded", "for=203.0.113.9")])));
        assert!(!is_host_pc(lo, &headers(&[("x-real-ip", "203.0.113.9")])));
        // Saying you are local does not make you local.
        assert!(!is_host_pc(lan, &headers(&[("host", "127.0.0.1:7890")])));
        assert!(!is_host_pc(lan, &headers(&[("x-forwarded-for", "127.0.0.1"), ("host", "localhost")])));
    }

    #[test]
    fn claim_codes_are_typed_by_people() {
        let st = OwnerState::new("");
        let shown = st.new_code();
        assert_eq!(shown.len(), 9);
        assert_eq!(shown.as_bytes()[4], b'-');
        assert!(shown.chars().filter(|c| *c != '-').all(|c| CODE_ALPHABET.contains(&(c as u8))));
        assert_eq!(st.code().as_deref(), Some(shown.as_str()));
        assert!(st.check_code(&shown));
        assert!(st.check_code(&shown.to_lowercase().replace('-', " ")));
        assert!(!st.check_code("AAAA-AAAA"));
        assert!(!st.check_code(""));
        assert!(!st.check_code(&format!("{shown}0")));
        // Two codes differ (40 random bits), and a new one replaces the old.
        let second = st.new_code();
        assert_ne!(shown, second);
        assert!(!st.check_code(&shown) && st.check_code(&second));
        st.clear_code();
        assert!(!st.check_code(&second) && st.code().is_none());
        assert_eq!(normalize_code("o0Il-1 x"), "00111X");
    }

    #[test]
    fn a_pinned_owner_never_changes() {
        let st = OwnerState::new("Dono");
        assert!(st.pinned() && st.has_owner());
        st.set("Outro");
        assert_eq!(st.name(), "Dono");
        let st = OwnerState::new("");
        assert!(!st.pinned() && !st.has_owner());
        st.set("Ana");
        assert_eq!(st.name(), "Ana");
    }
}
