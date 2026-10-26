//! User profiles (v0.6): display name, "Sobre mim", accent colour, custom status and presence.
//! The wire contract and the validation rules are in `vox_proto` (`profile.rs`).
//!
//! Storage: `users.display_name` (it rides on every [`User`], so it is a column of `users` and
//! comes with every query that builds one) and the `profiles` table for everything else, read only
//! when somebody opens a profile or connects. Presence is *per person*: the chosen presence is
//! persisted, an automatic "away" (the app's idle detection) is not, and the live state of
//! everybody who is connected is kept in memory by `state.rs`, which also enforces invisibility
//! (an invisible person is left out of the online list and of every presence event).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use rusqlite::{params, Connection, OptionalExtension};
use vox_proto::*;

use crate::api::{ApiErr, AuthUser, Authed};
use crate::db;
use crate::state::{self, AppState};
use crate::util::now_ms;

type ApiResult<T> = Result<Json<T>, ApiErr>;

/// How often expired custom statuses are cleared (and announced).
const SWEEP_EVERY: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------- live state of one person

/// What the hub keeps in memory about a connected person.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PresenceEntry {
    /// The presence the person chose (persisted).
    pub stored: Presence,
    /// The app's idle detection set "away" over `Online` (not persisted: a new connection starts clean).
    pub auto_away: bool,
    pub status: Option<CustomStatus>,
}

impl PresenceEntry {
    /// What the person is right now.
    pub fn effective(&self) -> Presence {
        if self.stored == Presence::Online && self.auto_away {
            Presence::Away
        } else {
            self.stored
        }
    }

    /// Plain "online, no status": nothing worth telling a client about.
    pub fn is_default(&self) -> bool {
        self.effective() == Presence::Online && self.status.is_none()
    }

    pub fn event(&self, user_id: UserId) -> ServerEvent {
        ServerEvent::PresenceChanged { user_id, presence: self.effective(), status: self.status.clone() }
    }
}

// ---------------------------------------------------------------- rate limits

/// A sliding window per person.
pub struct RateWindow {
    hits: Mutex<HashMap<UserId, VecDeque<Instant>>>,
    limit: usize,
    window: Duration,
}

impl RateWindow {
    pub fn new(limit: usize, window: Duration) -> Self {
        RateWindow { hits: Mutex::new(HashMap::new()), limit, window }
    }

    /// Count one attempt of `user`; `Err(wait)` when they are over the limit (not counted then).
    pub fn allow(&self, user: UserId, now: Instant) -> Result<(), Duration> {
        let mut g = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let q = g.entry(user).or_default();
        while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= self.window) {
            q.pop_front();
        }
        if q.len() >= self.limit {
            return Err(self.window.saturating_sub(now.saturating_duration_since(q[0])));
        }
        q.push_back(now);
        if g.len() > 256 {
            let window = self.window;
            g.retain(|_, q| q.back().is_some_and(|t| now.saturating_duration_since(*t) < window));
        }
        Ok(())
    }
}

/// Per-hub profile state: the rate limits.
pub struct ProfileState {
    /// Profile and status edits.
    pub edits: RateWindow,
    /// Presence changes (the app's idle detection makes a few).
    pub presence: RateWindow,
}

impl Default for ProfileState {
    fn default() -> Self {
        ProfileState {
            edits: RateWindow::new(PROFILE_RATE_LIMIT, Duration::from_secs(PROFILE_RATE_WINDOW_SECS)),
            presence: RateWindow::new(PRESENCE_RATE_LIMIT, Duration::from_secs(PRESENCE_RATE_WINDOW_SECS)),
        }
    }
}

fn too_fast(wait: Duration, what: &str) -> ApiErr {
    let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
    ApiErr::new(StatusCode::TOO_MANY_REQUESTS, format!("too many {what} changes, wait {secs} s"))
}

// ---------------------------------------------------------------- database

/// The profiles table (called by the migration; safe to run any number of times). Purely
/// additive: a hub that predates profiles never looks at it.
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS profiles (
            user_id INTEGER PRIMARY KEY REFERENCES users(id),
            bio TEXT NOT NULL DEFAULT '',
            accent TEXT,
            status_emoji TEXT NOT NULL DEFAULT '',
            status_text TEXT NOT NULL DEFAULT '',
            status_expires_ms INTEGER,
            presence TEXT NOT NULL DEFAULT 'online',
            updated_ms INTEGER NOT NULL DEFAULT 0
        );",
    )
}

/// What the `profiles` table holds for one person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub bio: String,
    pub accent: Option<String>,
    pub status: Option<CustomStatus>,
    pub presence: Presence,
}

impl Default for ProfileRow {
    fn default() -> Self {
        ProfileRow { bio: String::new(), accent: None, status: None, presence: Presence::Online }
    }
}

pub fn load(conn: &Connection, user_id: UserId) -> rusqlite::Result<ProfileRow> {
    let row = conn
        .query_row(
            "SELECT bio, accent, status_emoji, status_text, status_expires_ms, presence FROM profiles WHERE user_id = ?",
            [user_id],
            |r| {
                let (emoji, text, expires): (String, String, Option<i64>) = (r.get(2)?, r.get(3)?, r.get(4)?);
                let status = (!(emoji.is_empty() && text.is_empty())).then_some(CustomStatus { emoji, text, expires_at_ms: expires });
                Ok(ProfileRow { bio: r.get(0)?, accent: r.get(1)?, status, presence: Presence::from_wire(&r.get::<_, String>(5)?) })
            },
        )
        .optional()?;
    Ok(row.unwrap_or_default())
}

/// What a connecting person starts with: their stored presence and a status that has not expired.
pub fn load_entry(conn: &Connection, user_id: UserId, now: i64) -> rusqlite::Result<PresenceEntry> {
    let row = load(conn, user_id)?;
    Ok(PresenceEntry { stored: row.presence, auto_away: false, status: row.status.filter(|s| s.live(now).is_some()) })
}

fn ensure_row(conn: &Connection, user_id: UserId) -> rusqlite::Result<()> {
    conn.execute("INSERT OR IGNORE INTO profiles (user_id, updated_ms) VALUES (?, ?)", params![user_id, now_ms()])?;
    Ok(())
}

/// Apply a profile update; `None` leaves a field alone. Returns whether the display name changed.
pub fn update(conn: &mut Connection, user_id: UserId, display_name: Option<&Option<String>>, bio: Option<&str>, accent: Option<&Option<String>>) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    ensure_row(&tx, user_id)?;
    let mut name_changed = false;
    if let Some(name) = display_name {
        let before: Option<String> = tx.query_row("SELECT display_name FROM users WHERE id = ?", [user_id], |r| r.get(0))?;
        name_changed = before != *name;
        tx.execute("UPDATE users SET display_name = ? WHERE id = ?", params![name, user_id])?;
    }
    if let Some(bio) = bio {
        tx.execute("UPDATE profiles SET bio = ?, updated_ms = ? WHERE user_id = ?", params![bio, now_ms(), user_id])?;
    }
    if let Some(accent) = accent {
        tx.execute("UPDATE profiles SET accent = ?, updated_ms = ? WHERE user_id = ?", params![accent, now_ms(), user_id])?;
    }
    tx.commit()?;
    Ok(name_changed)
}

pub fn set_status(conn: &Connection, user_id: UserId, status: Option<&CustomStatus>) -> rusqlite::Result<()> {
    ensure_row(conn, user_id)?;
    let (emoji, text, expires) = match status {
        Some(s) => (s.emoji.as_str(), s.text.as_str(), s.expires_at_ms),
        None => ("", "", None),
    };
    conn.execute(
        "UPDATE profiles SET status_emoji = ?, status_text = ?, status_expires_ms = ?, updated_ms = ? WHERE user_id = ?",
        params![emoji, text, expires, now_ms(), user_id],
    )?;
    Ok(())
}

pub fn set_presence(conn: &Connection, user_id: UserId, presence: Presence) -> rusqlite::Result<()> {
    ensure_row(conn, user_id)?;
    conn.execute("UPDATE profiles SET presence = ?, updated_ms = ? WHERE user_id = ?", params![presence.wire_name(), now_ms(), user_id])?;
    Ok(())
}

/// Clear every status whose time is up; returns who had one.
pub fn expire_statuses(conn: &mut Connection, now: i64) -> rusqlite::Result<Vec<UserId>> {
    let tx = conn.transaction()?;
    let ids: Vec<UserId> = {
        let mut st = tx.prepare("SELECT user_id FROM profiles WHERE status_expires_ms IS NOT NULL AND status_expires_ms <= ?")?;
        let rows = st.query_map([now], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if !ids.is_empty() {
        tx.execute(
            "UPDATE profiles SET status_emoji = '', status_text = '', status_expires_ms = NULL WHERE status_expires_ms IS NOT NULL AND status_expires_ms <= ?",
            [now],
        )?;
    }
    tx.commit()?;
    Ok(ids)
}

/// Is `name` (case-insensitively) the username of somebody else?
fn is_other_username(conn: &Connection, name: &str, me: UserId) -> rusqlite::Result<bool> {
    conn.query_row("SELECT 1 FROM users WHERE username = ?1 COLLATE NOCASE AND id != ?2", params![name, me], |_| Ok(())).optional().map(|r| r.is_some())
}

// ---------------------------------------------------------------- background task

/// Background task: clear custom statuses whose time is up, and tell everybody.
pub async fn watch(s: AppState, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = now_ms();
                s.expire_statuses(now);
                if let Err(e) = s.db.call(move |c| expire_statuses(c, now)).await {
                    tracing::warn!("clearing expired statuses failed: {e:#}");
                }
            }
            _ = state::wait_shutdown(&mut shutdown) => return,
        }
    }
}

// ---------------------------------------------------------------- HTTP

fn refusal(e: ProfileError) -> ApiErr {
    ApiErr::new(StatusCode::BAD_REQUEST, e.to_string())
}

/// `target`'s profile as `viewer` may see it.
async fn build(s: &AppState, viewer: UserId, target: UserId) -> Result<UserProfile, ApiErr> {
    let row = s.db.call(move |c| db::get_user(c, target)).await?.ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))?;
    let stored = s.db.call(move |c| load(c, target)).await?;
    let now = now_ms();
    let user = row.user(&s.owner());
    let live = s.presence_entry(target);
    let me = viewer == target;
    let (online, presence, status) = match live {
        // Connected: the live state wins. An invisible person is offline to everybody else, and
        // nothing about their status is shown either.
        Some(e) => {
            let p = e.effective();
            if p.visible_to_others() || me {
                (true, p, e.status)
            } else {
                (false, Presence::Online, None)
            }
        }
        None => (false, Presence::Online, if stored.presence.visible_to_others() || me { stored.status } else { None }),
    };
    Ok(UserProfile {
        user,
        bio: stored.bio,
        accent: stored.accent,
        status: status.filter(|st| st.live(now).is_some()),
        presence,
        online,
        member_since_ms: row.created_at_ms,
    })
}

/// `GET /api/users/{id}/profile`
pub async fn get_profile(State(s): State<AppState>, AuthUser(me): AuthUser, Path(id): Path<UserId>) -> ApiResult<UserProfile> {
    Ok(Json(build(&s, me.id, id).await?))
}

/// `PUT /api/me/profile`
pub async fn put_profile(State(s): State<AppState>, me: Authed, body: Result<Json<ProfileUpdate>, JsonRejection>) -> ApiResult<UserProfile> {
    let Json(req) = body?;
    let display_name = req.display_name.as_deref().map(clean_display_name).transpose().map_err(refusal)?;
    let bio = req.bio.as_deref().map(clean_bio).transpose().map_err(refusal)?;
    let accent = req.accent.as_deref().map(clean_accent).transpose().map_err(refusal)?;
    if let Err(wait) = s.profiles.edits.allow(me.user.id, Instant::now()) {
        tracing::warn!(user = %me.user.username, "profile edit refused: rate limit");
        return Err(too_fast(wait, "profile"));
    }
    let id = me.user.id;
    if let Some(Some(name)) = display_name.clone() {
        if s.db.call(move |c| is_other_username(c, &name, id)).await? {
            return Err(ApiErr::new(StatusCode::CONFLICT, "that name is somebody else's username"));
        }
    }
    let name_changed = s.db.call(move |c| update(c, id, display_name.as_ref(), bio.as_deref(), accent.as_ref())).await?;
    tracing::info!(user = %me.user.username, name_changed, "profile saved");
    if name_changed {
        announce_user(&s, id).await?;
    }
    Ok(Json(build(&s, id, id).await?))
}

/// Tell everybody the user's name (or anything on their `User`) changed.
async fn announce_user(s: &AppState, id: UserId) -> Result<(), ApiErr> {
    let row = s.db.call(move |c| db::get_user(c, id)).await?.ok_or_else(|| ApiErr::new(StatusCode::NOT_FOUND, "user not found"))?;
    s.update_user(&row.user(&s.owner()));
    Ok(())
}

/// `PUT /api/me/presence`
pub async fn put_presence(State(s): State<AppState>, me: Authed, body: Result<Json<PresenceUpdate>, JsonRejection>) -> ApiResult<PresenceResponse> {
    let Json(req) = body?;
    if let Err(wait) = s.profiles.presence.allow(me.user.id, Instant::now()) {
        return Err(too_fast(wait, "presence"));
    }
    let id = me.user.id;
    if !req.auto {
        let p = req.presence;
        s.db.call(move |c| set_presence(c, id, p)).await?;
    }
    let presence = s.set_presence(id, req.presence, req.auto).unwrap_or(if req.auto { Presence::Online } else { req.presence });
    tracing::info!(user = %me.user.username, ?presence, auto = req.auto, "presence");
    Ok(Json(PresenceResponse { presence }))
}

async fn store_status(s: &AppState, me: &Authed, status: Option<CustomStatus>) -> ApiResult<StatusResponse> {
    if let Err(wait) = s.profiles.edits.allow(me.user.id, Instant::now()) {
        return Err(too_fast(wait, "status"));
    }
    let (id, st) = (me.user.id, status.clone());
    s.db.call(move |c| set_status(c, id, st.as_ref())).await?;
    s.set_status(id, status.clone());
    tracing::info!(user = %me.user.username, set = status.is_some(), "status");
    Ok(Json(StatusResponse { status }))
}

/// `PUT /api/me/status`
pub async fn put_status(State(s): State<AppState>, me: Authed, body: Result<Json<StatusUpdate>, JsonRejection>) -> ApiResult<StatusResponse> {
    let Json(req) = body?;
    let status = clean_status(&req.emoji, &req.text, req.expires_in_secs, now_ms()).map_err(refusal)?;
    store_status(&s, &me, status).await
}

/// `DELETE /api/me/status`
pub async fn delete_status(State(s): State<AppState>, me: Authed) -> ApiResult<StatusResponse> {
    store_status(&s, &me, None).await
}

fn excerpt(text: &str) -> String {
    let t: String = text.chars().take(80).map(|c| if c.is_control() { ' ' } else { c }).collect();
    if text.chars().count() > 80 { format!("{t}...") } else { t }
}

/// `POST /api/mod/users/{id}/clear-profile` (Moderador and up, for people below their role)
pub async fn clear_profile(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>, body: Result<Json<ModClearProfileRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    let target = crate::admin::moderation_target(&s, &me, id).await?;
    if req.is_empty() {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "nothing to clear"));
    }
    let (clear_name, clear_bio, clear_status) = (req.display_name, req.bio, req.status);
    // What is removed goes to the log, so a moderator's call can be reviewed.
    let removed = s
        .db
        .call(move |c| {
            let before = load(c, id)?;
            let mut parts = Vec::new();
            if clear_bio && !before.bio.is_empty() {
                parts.push(format!("bio: {}", excerpt(&before.bio)));
                ensure_row(c, id)?;
                c.execute("UPDATE profiles SET bio = '', updated_ms = ? WHERE user_id = ?", params![now_ms(), id])?;
            }
            if clear_status {
                if let Some(st) = before.status.filter(|st| !st.is_empty()) {
                    parts.push(format!("status: {} {}", st.emoji, excerpt(&st.text)).trim_end().to_string());
                }
                set_status(c, id, None)?;
            }
            if clear_name {
                let name: Option<String> = c.query_row("SELECT display_name FROM users WHERE id = ?", [id], |r| r.get(0))?;
                if let Some(name) = name {
                    parts.push(format!("display_name: {}", excerpt(&name)));
                    c.execute("UPDATE users SET display_name = NULL WHERE id = ?", [id])?;
                }
            }
            Ok(parts)
        })
        .await?;
    if clear_status {
        s.set_status(id, None);
    }
    if clear_name {
        announce_user(&s, id).await?;
    }
    let detail = if removed.is_empty() { "(nada para limpar)".to_string() } else { removed.join(" | ") };
    crate::admin::audit(&s, Some(me.user.id), AUDIT_CLEAR_PROFILE, Some(target.id), detail).await;
    Ok(Json(OkResponse { ok: true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_presence_and_defaults() {
        let mut e = PresenceEntry::default();
        assert!(e.is_default());
        e.auto_away = true;
        assert_eq!(e.effective(), Presence::Away);
        assert!(!e.is_default());
        // A chosen presence beats the automatic one.
        e.stored = Presence::Dnd;
        assert_eq!(e.effective(), Presence::Dnd);
        e.stored = Presence::Invisible;
        assert_eq!(e.effective(), Presence::Invisible);
        e = PresenceEntry { status: Some(CustomStatus { emoji: "🎮".into(), ..Default::default() }), ..Default::default() };
        assert!(!e.is_default());
    }

    #[test]
    fn rate_window_slides() {
        let w = RateWindow::new(2, Duration::from_secs(10));
        let t0 = Instant::now();
        assert!(w.allow(1, t0).is_ok() && w.allow(1, t0).is_ok());
        assert!(w.allow(1, t0 + Duration::from_secs(1)).is_err());
        assert!(w.allow(2, t0 + Duration::from_secs(1)).is_ok());
        assert!(w.allow(1, t0 + Duration::from_secs(10)).is_ok());
    }

    #[test]
    fn rows_round_trip_and_statuses_expire() {
        let dir = tempfile::tempdir().unwrap();
        let db = db::Db::open(&dir.path().join("t.db")).unwrap();
        db.call_now(|c| {
            c.execute("INSERT INTO users (username, created_at_ms) VALUES ('ana', 1), ('bia', 1)", [])?;
            assert_eq!(load(c, 1)?, ProfileRow::default(), "no row yet");
            update(c, 1, Some(&Some("Ana".into())), Some("oi"), Some(&Some("#112233".into())))?;
            set_presence(c, 1, Presence::Dnd)?;
            set_status(c, 1, Some(&CustomStatus { emoji: "🎮".into(), text: "x".into(), expires_at_ms: Some(1000) }))?;
            set_status(c, 2, Some(&CustomStatus { text: "para sempre".into(), ..Default::default() }))?;
            let row = load(c, 1)?;
            assert_eq!((row.bio.as_str(), row.accent.as_deref(), row.presence), ("oi", Some("#112233"), Presence::Dnd));
            assert_eq!(db::get_user(c, 1)?.unwrap().display_name.as_deref(), Some("Ana"));
            // A status that already ran out is not loaded for a connecting person.
            assert_eq!(load_entry(c, 1, 999)?.status.map(|s| s.text), Some("x".into()));
            assert_eq!(load_entry(c, 1, 1000)?.status, None);
            assert_eq!(load_entry(c, 1, 5)?.stored, Presence::Dnd);
            assert_eq!(expire_statuses(c, 999)?, Vec::<UserId>::new());
            assert_eq!(expire_statuses(c, 1000)?, vec![1]);
            assert_eq!(load(c, 1)?.status, None);
            assert!(load(c, 2)?.status.is_some(), "a status without a deadline stays");
            // Clearing the display name leaves the rest of the profile.
            update(c, 1, Some(&None), None, None)?;
            assert_eq!(db::get_user(c, 1)?.unwrap().display_name, None);
            assert_eq!(load(c, 1)?.bio, "oi");
            assert!(is_other_username(c, "BIA", 1)? && !is_other_username(c, "ANA", 1)?);
            Ok(())
        })
        .unwrap();
    }
}
