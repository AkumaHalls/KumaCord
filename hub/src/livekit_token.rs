//! LiveKit access tokens: plain HS256 JWTs with LiveKit's `video` grant claim.
//! Hand-rolled (hmac + sha2 + base64) to avoid pulling in the livekit-api/ring dependency trees.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

/// How long a join token can be used to *enter* a room. Short on purpose: a token cannot be
/// revoked, so this bounds how long a removed member could come back with an old one. Every app
/// asks the hub for a new token right before each join (a channel, a DM call, a retry) and uses it
/// at once, so a minute is plenty. A session that is already in the room is not affected: LiveKit
/// sends the SDK a fresh token of its own as soon as the session starts and then every few
/// minutes (`RefreshToken`), and the SDK's own reconnects use that one, so a call can last for
/// hours. (6 h before v0.6.2, 10 min before v1.0.1: audit
/// `voxhub:hub:private-voice-rejoin-window-after-access-loss`.)
pub const TOKEN_TTL_SECS: i64 = 60;
/// LiveKit accepts a token up to this long after its `exp` (its JWT check allows for clock skew).
pub const LIVEKIT_LEEWAY_SECS: i64 = 60;
/// How long after it was issued an old join token could still get somebody into a room.
pub const TOKEN_USABLE_SECS: i64 = TOKEN_TTL_SECS + LIVEKIT_LEEWAY_SECS;
/// Server-API tokens are minted per call.
pub const ADMIN_TOKEN_TTL_SECS: i64 = 10 * 60;

/// What a server-muted member may still publish (item 12): everything but the microphone.
pub const SOURCES_WITHOUT_MIC: [&str; 3] = ["camera", "screen_share", "screen_share_audio"];
/// What any member may publish: the four kinds the app makes (no `UNKNOWN`-source tracks).
pub const MEMBER_SOURCES: [&str; 4] = ["camera", "microphone", "screen_share", "screen_share_audio"];

/// The sources a member may publish (`server_muted`: no microphone).
pub fn publish_sources(server_muted: bool) -> &'static [&'static str] {
    if server_muted { &SOURCES_WITHOUT_MIC } else { &MEMBER_SOURCES }
}

fn sign(api_secret: &str, claims: &Value) -> String {
    let header = json!({ "alg": "HS256", "typ": "JWT" });
    let signing = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(api_secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(signing.as_bytes());
    format!("{signing}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

pub fn join_token(api_key: &str, api_secret: &str, room: &str, identity: &str, name: &str, now_secs: i64) -> String {
    join_token_with(api_key, api_secret, room, identity, name, now_secs, false)
}

/// A join token: the four sources the app publishes ([`MEMBER_SOURCES`]); `server_muted` leaves
/// the microphone out.
pub fn join_token_with(api_key: &str, api_secret: &str, room: &str, identity: &str, name: &str, now_secs: i64, server_muted: bool) -> String {
    let video = json!({
        "room": room,
        "roomJoin": true,
        "canPublish": true,
        "canSubscribe": true,
        "canPublishData": true,
        "canPublishSources": publish_sources(server_muted),
    });
    let claims = json!({
        "iss": api_key,
        "sub": identity,
        "name": name,
        "nbf": now_secs - 10,
        "exp": now_secs + TOKEN_TTL_SECS,
        "video": video,
    });
    sign(api_secret, &claims)
}

/// A token for LiveKit's server API (`RoomService`), limited to one room; `None`: listing rooms.
pub fn admin_token(api_key: &str, api_secret: &str, room: Option<&str>, now_secs: i64) -> String {
    let video = match room {
        Some(room) => json!({ "room": room, "roomAdmin": true }),
        None => json!({ "roomList": true }),
    };
    let claims = json!({
        "iss": api_key,
        // (An internal subject, never shown; kept as it was before the rename to concord.)
        "sub": "voxhub-server",
        "nbf": now_secs - 10,
        "exp": now_secs + ADMIN_TOKEN_TTL_SECS,
        "video": video,
    });
    sign(api_secret, &claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(token: &str) -> Value {
        let body = token.split('.').nth(1).unwrap();
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).unwrap()).unwrap()
    }

    #[test]
    fn join_tokens_are_short_lived_and_limit_sources() {
        let t = claims(&join_token("k", "s", "voice-2", "7", "Ana", 1000));
        assert_eq!(t["exp"].as_i64().unwrap() - 1000, 60, "one minute to enter the room");
        assert_eq!(t["video"]["canPublishSources"], json!(["camera", "microphone", "screen_share", "screen_share_audio"]));
        let muted = claims(&join_token_with("k", "s", "dm-1-2", "7", "Ana", 1000, true));
        assert_eq!(muted["video"]["canPublishSources"], json!(["camera", "screen_share", "screen_share_audio"]));
        assert_eq!(muted["video"]["room"], "dm-1-2");
        let list = claims(&admin_token("k", "s", None, 1000));
        assert_eq!(list["video"], json!({ "roomList": true }));
    }

    /// v1.0.1: a join token (channel or DM call) is good for at most 90 s, LiveKit's leeway apart,
    /// so a member who lost access to a private room cannot keep coming back with an old one.
    #[test]
    fn join_tokens_expire_within_ninety_seconds() {
        const { assert!(TOKEN_TTL_SECS >= 1 && TOKEN_TTL_SECS <= 90 && TOKEN_USABLE_SECS <= 90 + LIVEKIT_LEEWAY_SECS) };
        for t in [join_token("k", "s", "voice-2", "7", "Ana", 5000), join_token_with("k", "s", "dm-1-2", "7", "Ana", 5000, false)] {
            let c = claims(&t);
            let (nbf, exp) = (c["nbf"].as_i64().unwrap(), c["exp"].as_i64().unwrap());
            assert!(exp > 5000 && exp - 5000 <= 90, "exp {exp}");
            assert!(exp - nbf <= 100, "valid for {} s", exp - nbf);
        }
    }
}
