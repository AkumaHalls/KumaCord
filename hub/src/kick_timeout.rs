//! Kick and timeout moderation actions (v1.1).

use axum::extract::{Path, State};
use axum::Json;
use axum::http::StatusCode;
use vox_proto::*;

use crate::api::{ApiErr, ApiResult};
use crate::db::{self, UserRow};
use crate::state::AppState;
use crate::util::now_ms;

/// Maximum length for kick reason.
pub const KICK_REASON_MAX: usize = 256;
/// Maximum length for timeout reason.
pub const TIMEOUT_REASON_MAX: usize = 256;

/// `POST /api/admin/users/{id}/kick` (v1.1): kick a user from the server.
pub async fn kick_user(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>, body: Result<Json<KickRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    if !may(&s, &me, perm::KICK_MEMBERS).await {
        return Err(forbidden());
    }
    if id == me.user.id {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "not on yourself"));
    }
    let t = target_row(&s, id).await?;
    let theirs = t.role(&s.owner());
    if theirs == Role::Owner || !me.role.outranks(theirs) {
        return Err(forbidden());
    }
    let reason: String = req.reason.trim().chars().take(KICK_REASON_MAX).collect();
    s.send_to(id, &ServerEvent::Moderated { action: ModAction::Kicked { reason: reason.clone() }, by: me.user.clone() });
    s.kick(id, None);
    audit(&s, Some(me.user.id), AUDIT_KICK, Some(id), reason).await;
    Ok(ok())
}

/// `POST /api/admin/users/{id}/timeout` (v1.1): timeout a user for a duration.
pub async fn timeout_user(State(s): State<AppState>, me: Authed, Path(id): Path<UserId>, body: Result<Json<TimeoutRequest>, JsonRejection>) -> ApiResult<OkResponse> {
    let Json(req) = body?;
    if !may(&s, &me, perm::MODERATE_MEMBERS).await {
        return Err(forbidden());
    }
    if id == me.user.id {
        return Err(ApiErr::new(StatusCode::BAD_REQUEST, "not on yourself"));
    }
    let t = target_row(&s, id).await?;
    let theirs = t.role(&s.owner());
    if theirs == Role::Owner || !me.role.outranks(theirs) {
        return Err(forbidden());
    }
    let duration_ms = req.duration_ms.clamp(60_000, 28 * 24 * 3600 * 1000);
    let reason: String = req.reason.trim().chars().take(TIMEOUT_REASON_MAX).collect();
    let until_ms = now_ms() + duration_ms;
    s.db.call(move |c| db::timeout_user(c, id, until_ms, &reason)).await?;
    s.send_to(id, &ServerEvent::Moderated { action: ModAction::TimedOut { until_ms, reason: reason.clone() }, by: me.user.clone() });
    s.kick(id, None);
    audit(&s, Some(me.user.id), AUDIT_TIMEOUT, Some(id), format!("{}ms: {}", duration_ms, reason)).await;
    Ok(ok())
}
