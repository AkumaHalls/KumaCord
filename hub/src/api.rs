//! HTTP API (`/api/*`) and router assembly.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::DefaultBodyLimit;
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, Request, State};
use axum::http::{header, request::Parts, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use vox_proto::*;

use crate::auth::{self, ip_key, user_key, Denied, Proof};
use crate::db::UserRow;
use crate::linkpreview::{LinkPreviews, RateLimited};
use crate::livekit_token;
use crate::state::AppState;
use crate::util::{now_ms, random_hex, sha256_hex};
use crate::{admin, avatar, db, owner, profile, sounds, ws};

pub fn router(state: AppState) -> Router {
    let limits = std::sync::Arc::new(state.tuning.limits.clone());
    Router::new()
        .route("/api/health", get(health))
        .route("/api/login", post(login))
        .route("/api/channels", get(channels))
        .route("/api/channels/{id}/messages", get(messages).post(post_message).layer(DefaultBodyLimit::max(MESSAGE_MAX_BODY)))
        .route("/api/voice/{id}/join", post(voice_join))
        .route("/api/link-preview", get(link_preview))
        .route("/api/link-preview/image", get(link_preview_image))
        // item 12
        .route("/api/account/password", post(admin::set_password))
        .route("/api/messages/{id}/delete", post(admin::delete_message))
        .route("/api/messages/{id}/edit", post(admin::edit_message))
        .route("/api/messages/{id}/pin", post(admin::pin_message))
        .route("/api/messages/{id}/unpin", post(admin::unpin_message))
        .route("/api/messages/{id}/reactions", post(crate::chat::react))
        .route("/api/mod/users/{id}/mute", post(admin::mute))
        .route("/api/mod/users/{id}/disconnect", post(admin::disconnect))
        .route("/api/mod/users/{id}/move", post(admin::move_user))
        .route("/api/mod/users/{id}/stop-stream", post(admin::stop_stream))
        .route("/api/admin/users", get(admin::users))
        .route("/api/admin/users/{id}/role", post(admin::set_role))
        .route("/api/admin/users/{id}/badges", put(admin::set_badges))
        .route("/api/admin/users/{id}/ban", post(admin::ban_user))
        .route("/api/admin/users/{id}/unban", post(admin::unban_user))
        .route("/api/admin/users/{id}/kick", post(admin::kick_user))
        .route("/api/admin/users/{id}/timeout", post(admin::timeout_user))
        .route("/api/admin/custom-roles", get(admin::custom_roles).post(admin::create_custom_role))
        .route("/api/admin/custom-roles/{id}", post(admin::update_custom_role).delete(admin::delete_custom_role))
        .route("/api/admin/users/{id}/custom-roles", post(admin::set_user_custom_roles))
        .route("/api/admin/users/{id}/reset-password", post(admin::reset_password))
        .route("/api/admin/audit", get(admin::audit_log))
        // "quem hospeda vira dono"
        .route("/api/owner/claim", post(owner::claim_with_code))
        .route("/api/admin/transfer-owner", post(owner::transfer))
        .route("/api/admin/channels", post(admin::create_channel))
        .route("/api/admin/channels/reorder", post(admin::reorder_channels))
        .route("/api/admin/channels/{id}/rename", post(admin::rename_channel))
        .route("/api/admin/channels/{id}/delete", post(admin::delete_channel))
        // private channels (v0.9, `access.rs`)
        .route("/api/admin/channels/{id}/access", post(admin::set_channel_access))
        // channel categories (v1.1)
        .route("/api/admin/categories", get(admin::list_categories).post(admin::create_category))
        .route("/api/admin/categories/{id}", post(admin::update_category).delete(admin::delete_category))
        // profile pictures
        .route("/api/users/{id}/avatar", get(avatar::get_avatar))
        .route("/api/me/avatar", put(avatar::put_avatar).layer(DefaultBodyLimit::max(AVATAR_MAX_UPLOAD_BYTES)))
        .route("/api/mod/users/{id}/remove-avatar", post(avatar::remove_avatar))
        // profiles: display name, bio, colour, custom status, presence
        .route("/api/users/{id}/profile", get(profile::get_profile))
        .route("/api/me/profile", put(profile::put_profile))
        .route("/api/me/presence", put(profile::put_presence))
        .route("/api/me/status", put(profile::put_status).delete(profile::delete_status))
        .route("/api/mod/users/{id}/clear-profile", post(profile::clear_profile))
        // soundboard
        .route("/api/sounds", get(sounds::list).post(sounds::upload).layer(DefaultBodyLimit::max(SOUND_MAX_UPLOAD_BYTES + 4096)))
        .route("/api/sounds/{id}/audio", get(sounds::audio))
        .route("/api/sounds/{id}/rename", post(sounds::rename))
        .route("/api/sounds/{id}/delete", post(sounds::delete))
        // chat images (`attachments.rs`)
        .route("/api/attachments/limits", get(crate::attachments::limits))
        .route("/api/attachments", post(crate::attachments::upload).layer(DefaultBodyLimit::max(ATTACHMENT_MAX_UPLOAD_BODY)))
        .route("/api/attachments/{id}", get(crate::attachments::get_file))
        .route("/api/attachments/{id}/thumb", get(crate::attachments::get_thumb))
        // invites (v0.8, `invites.rs`)
        .route("/api/invites", get(crate::invites::list_invites).post(crate::invites::create))
        .route("/api/invites/redeem", post(crate::invites::redeem))
        .route("/api/invites/{id}/revoke", post(crate::invites::revoke))
        // direct messages and private calls (`dms.rs`, `calls.rs`)
        .merge(crate::dms::routes())
        .route("/ws", get(ws::handler))
        .layer(middleware::from_fn(log_request))
        .layer(middleware::from_fn(move |req, next| crate::serve::timeout_layer(limits.clone(), req, next)))
        .with_state(state)
}

// ---------- errors ----------

pub struct ApiErr(pub StatusCode, pub String);

impl ApiErr {
    pub fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        ApiErr(status, msg.into())
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(ApiError { error: self.1 })).into_response()
    }
}

impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("internal error: {e:#}");
        ApiErr::new(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
    }
}

impl From<JsonRejection> for ApiErr {
    fn from(e: JsonRejection) -> Self {
        ApiErr::new(StatusCode::BAD_REQUEST, format!("invalid request body: {}", e.body_text()))
    }
}

type ApiResult<T> = Result<Json<T>, ApiErr>;

async fn log_request(req: Request, next: Next) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let t = Instant::now();
    let resp = next.run(req).await;
    tracing::debug!(%method, path, status = resp.status().as_u16(), ms = t.elapsed().as_millis() as u64, "http");
    resp
}

fn forbidden() -> ApiErr {
    ApiErr::new(StatusCode::FORBIDDEN, "not allowed")
}

// ---------- auth ----------

fn bearer_hash(parts: &Parts) -> Option<String> {
    let v = parts.headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let t = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?;
    Some(sha256_hex(t.trim()))
}

/// Resolves a session token hash to its user.
pub async fn user_for_token_hash(state: &AppState, hash: String) -> Result<UserRow, ApiErr> {
    state
        .db
        .call(move |c| db::user_for_session(c, &hash))
        .await?
        .ok_or_else(|| ApiErr::new(StatusCode::UNAUTHORIZED, "invalid or expired session"))
}

pub struct AuthUser(pub User);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiErr;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiErr> {
        let hash = bearer_hash(parts).ok_or_else(|| ApiErr::new(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
        Ok(AuthUser(user_for_token_hash(state, hash).await?.user(&state.owner())))
    }
}

/// An authenticated caller with what permission checks need (item 12).
pub struct Authed {
    pub row: UserRow,
    pub user: User,
    /// The role permissions are checked against: `Member` while a needed personal password is
    /// not set yet.
    pub role: Role,
    pub session_hash: String,
}

impl FromRequestParts<AppState> for Authed {
    type Rejection = ApiErr;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiErr> {
        let hash = bearer_hash(parts).ok_or_else(|| ApiErr::new(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
        let row = user_for_token_hash(state, hash.clone()).await?;
        let owner = state.owner();
        Ok(Authed { user: row.user(&owner), role: row.effective_role(&owner), row, session_hash: hash })
    }
}

/// `429` with the wait in seconds.
pub fn too_many(wait: Duration) -> ApiErr {
    let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
    ApiErr::new(StatusCode::TOO_MANY_REQUESTS, format!("{ERR_TOO_MANY_ATTEMPTS}, wait {secs} s"))
}

/// How long a password check waits for an argon2 slot before the hub answers "busy" (`429`).
const ARGON2_QUEUE_WAIT: Duration = Duration::from_secs(3);

/// Run `f` (argon2) on the blocking pool, at most [`crate::auth::ARGON2_AT_ONCE`] at a time. The
/// slot is held by the blocking work itself, so a cancelled request does not free it early.
pub async fn argon2_blocking<T: Send + 'static>(s: &AppState, f: impl FnOnce() -> T + Send + 'static) -> Result<T, ApiErr> {
    let permit = match tokio::time::timeout(ARGON2_QUEUE_WAIT, s.argon2.clone().acquire_owned()).await {
        Ok(Ok(p)) => p,
        _ => {
            tracing::warn!("password checks are saturated: request refused");
            return Err(too_many(Duration::from_secs(2)));
        }
    };
    crate::util::spawn_blocking_with(permit, f).await.map_err(|e| anyhow::anyhow!("{e}").into())
}

// ---------- handlers ----------

async fn health(State(s): State<AppState>) -> Json<HealthResponse> {
    let addr = ("127.0.0.1", s.cfg.livekit.signal_port);
    let livekit_ok = matches!(
        tokio::time::timeout(Duration::from_millis(500), tokio::net::TcpStream::connect(addr)).await,
        Ok(Ok(_))
    );
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION,
        livekit_ok,
        server_id: Some(s.server_id.clone()),
    })
}

async fn login(
    State(s): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<LoginRequest>, JsonRejection>,
) -> ApiResult<LoginResponse> {
    let Json(req) = body?;
    if req.protocol_version != PROTOCOL_VERSION {
        return Err(ApiErr::new(
            StatusCode::BAD_REQUEST,
            format!("protocol version mismatch: server {PROTOCOL_VERSION}, client {}", req.protocol_version),
        ));
    }
    let username = req.username.trim().to_string();
    let len = username.chars().count();
    if !(2..=32).contains(&len) || username.chars().any(char::is_control) {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "username must be 2-32 characters"));
    }
    let ip = ip_key(peer.ip());
    if let Some(wait) = s.throttle.must_wait(&ip) {
        tracing::warn!(%peer, username, "login refused: address is being slowed down");
        return Err(too_many(wait));
    }
    // v0.8: the server password, an invite, or a remembered device of an invite member.
    let admission = match crate::invites::admit(&s, &req, &username).await? {
        Ok(a) => a,
        Err(refused) => {
            tracing::warn!(%peer, username, invite = req.invite.is_some(), reason = %refused.1, "login rejected: server password / invite");
            s.throttle.fail(&ip);
            tokio::time::sleep(Duration::from_millis(300)).await; // blunt brute-force brake
            return Err(refused);
        }
    };

    // Item 12: a name protected by a personal password needs it (or a remembered device).
    let name = username.clone();
    let existing = s.db.call(move |c| db::find_user(c, &name)).await?;
    let mut proof = Proof::Open;
    if let Some(row) = existing {
        let ukey = user_key(&row.username);
        let device_ok = match req.device_token.as_deref() {
            Some(t) if row.has_password() => {
                let (h, id) = (sha256_hex(t), row.id);
                s.db.call(move |c| db::use_device_token(c, id, &h)).await?
            }
            _ => false,
        };
        // A check of a password / code reserves its attempt first (check-and-reserve in one step),
        // so simultaneous guesses get no more checks than guesses one after the other.
        let checks_secret = !device_ok && req.password.is_some() && (row.has_password() || row.reset_hash.is_some());
        let attempt = if checks_secret {
            match s.throttle.begin(&ukey) {
                Ok(a) => Some(a),
                Err(wait) => {
                    tracing::warn!(%peer, username, "login refused: name is being slowed down");
                    return Err(too_many(wait));
                }
            }
        } else {
            None
        };
        let password = req.password.clone();
        let checked = if checks_secret && row.has_password() {
            argon2_blocking(&s, move || auth::check(&row, password.as_deref(), device_ok, now_ms())).await?
        } else {
            auth::check(&row, password.as_deref(), device_ok, now_ms())
        };
        match checked {
            Ok(p) => {
                proof = p;
                if let Some(a) = attempt {
                    if matches!(p, Proof::Password | Proof::ResetCode) {
                        a.succeed();
                    }
                }
            }
            Err(Denied::Required) => {
                tracing::info!(%peer, username, device_token = req.device_token.is_some(), "login needs the personal password");
                return Err(ApiErr::new(StatusCode::UNAUTHORIZED, ERR_PERSONAL_PASSWORD_REQUIRED));
            }
            Err(Denied::Wrong) => {
                tracing::warn!(%peer, username, "login rejected: wrong personal password");
                if let Some(a) = attempt {
                    a.fail();
                }
                s.throttle.fail(&ip);
                tokio::time::sleep(Duration::from_millis(300)).await;
                return Err(ApiErr::new(StatusCode::UNAUTHORIZED, ERR_WRONG_PERSONAL_PASSWORD));
            }
            Err(Denied::Expired) => {
                tracing::warn!(%peer, username, "login rejected: reset code expired");
                return Err(ApiErr::new(StatusCode::UNAUTHORIZED, ERR_RESET_CODE_EXPIRED));
            }
        }
    }
    // v1.0.1: an invite only creates new accounts. An existing name came in with its own proof
    // (personal password, remembered device, reset code) or not at all: an unprotected name
    // needs the server password, as before invites existed.
    let invite_name_taken = || {
        tracing::warn!(%peer, username, "login rejected: an invite does not admit an existing name");
        ApiErr::new(StatusCode::UNAUTHORIZED, ERR_INVITE_NAME_TAKEN)
    };
    if admission == crate::invites::Admission::ExistingName && !matches!(proof, Proof::Password | Proof::DeviceToken | Proof::ResetCode) {
        return Err(invite_name_taken());
    }
    s.throttle.succeed(&ip);
    crate::invites::take_use(&s, admission).await?;

    let token = random_hex::<32>();
    let hash = sha256_hex(&token);
    let name = username.clone();
    let mut row = if matches!(admission, crate::invites::Admission::Invite(_)) {
        // The name was free at the check; somebody may have taken it since (the use is spent).
        match s.db.call(move |c| db::login_new(c, &name, &hash)).await? {
            Some(row) => row,
            None => return Err(invite_name_taken()),
        }
    } else {
        s.db.call(move |c| db::login(c, &name, &hash)).await?
    };
    // v1.1: a banned account never comes in — not with a password, not with an invite,
    // not with a remembered device (the ban deleted them all). The reason, if any,
    // travels with the refusal so the login screen can tell why.
    if let Some(b) = s.db.call(move |c| db::ban_of(c, row.id)).await? {
        tracing::warn!(user = %row.username, by = ?b.banned_by, "login refused: banned");
        let why = if b.reason.trim().is_empty() { String::new() } else { format!(" Motivo: {}.", b.reason.trim()) };
        return Err(ApiErr::new(StatusCode::FORBIDDEN, format!("Você foi banido deste servidor.{why}")));
    }

    let owner = s.owner().clone();
    let user = row.user(&owner);
    let session_hash = hash.clone();
    let session_hash_for_login = hash.clone();
    s.db.call(move |c| {
        db::create_session(c, row.id, &session_hash_for_login, now_ms() + SESSION_TTL_MS)?;
        if let Some(device_token) = &req.device_token {
            db::add_device_token(c, row.id, &sha256_hex(device_token))?;
        }
        Ok(())
    }).await?;
    let expires_at_ms = now_ms() + SESSION_TTL_MS;
    let refresh_at = expires_at_ms - SESSION_TTL_MS / 4; // refresh at 75%
    let members = s.members();
    let member_versions = members.iter().map(|m| (m.user.id, m.version)).collect();
    let (channels, dm_channels, categories) = s.db.call(move |c| {
        let chans = crate::channels::load_channels(c);
        let dm = crate::dms::list_dms_for(c, row.id)?;
        let cats = db::list_categories(c)?;
        Ok((chans, dm, cats))
    }).await?;

    let resp = LoginResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION,
        api_version: API_VERSION,
        session_token: token.clone(),
        session_expires_at_ms: expires_at_ms,
        refresh_at_ms: refresh_at,
        user,
        channels,
        dm_channels: dm_channels,
        categories,
        server_id: Some(s.server_id.clone()),
        member_versions,
        settings: crate::db::UserSettings {
            locale: row.locale.clone().unwrap_or_else(|| Locale::PtBr),
            theme: row.theme.clone().unwrap_or_else(|| Theme::Dark),
            server_muted: row.server_muted,
            server_muted_until_ms: row.server_muted_until_ms,
            notification_level: row.notification_level,
            push_enabled: row.push_enabled,
        },
    };
    Ok(Json(resp))
}

async fn channels(State(s): State<AppState>) -> Json<Vec<Channel>> {
    Json(s.channels())
}

async fn messages(
    State(s): State<AppState>,
    me: Authed,
    Path(id): Path<ChannelId>,
    Query(q): Query<crate::messages::MessageQuery>,
) -> ApiResult<Vec<Message>> {
    use crate::messages;
    let msgs = s.db.call(move |c| messages::load(c, id, q.before, q.limit)).await?;
    Ok(Json(msgs))
}

async fn post_message(State(s): State<AppState>, me: Authed, Path(id): Path<ChannelId>, body: Bytes) -> ApiResult<Message> {
    let resp = crate::messages::post(&s, &me, id, body).await?;
    Ok(Json(resp))
}

async fn voice_join(State(s): State<AppState>, me: Authed, Path(id): Path<ChannelId>) -> ApiResult<VoiceJoinResponse> {
    let resp = crate::voice::join(&s, &me, id).await?;
    Ok(Json(resp))
}

async fn link_preview(State(s): State<AppState>, Query(q): Query<LinkPreviewQuery>) -> Response {
    crate::linkpreview::render(&s, &q.url).into_response()
}

async fn link_preview_image(State(s): State<AppState>, Query(q): Query<LinkPreviewQuery>) -> Response {
    crate::linkpreview::render_image(&s, &q.url).into_response()
}

#[derive(Deserialize)]
struct LinkPreviewQuery {
    url: String,
}
