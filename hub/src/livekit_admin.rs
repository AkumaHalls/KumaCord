//! A tiny client for LiveKit's server API (Twirp `livekit.RoomService`, JSON over HTTP on the
//! signal port), item 12. It makes moderation real in the SFU: a server-muted member's microphone
//! is refused by LiveKit itself (a modified or older client cannot get around it), a disconnected
//! member is removed from the room, and a stopped Go Live is muted at the SFU.
//!
//! The hub also lists rooms and participants (`ListRooms`, `ListParticipants`) to compare what
//! LiveKit really has with the hub's voice list (`voice_guard.rs`).
//!
//! Every call is authenticated with a short-lived `roomAdmin` (or `roomList`) JWT from
//! [`livekit_token`]. No SDK: a few JSON calls to a server on the same machine.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::Config;
use crate::livekit_token::{admin_token, publish_sources};
use crate::util::now_ms;

#[derive(Debug, thiserror::Error)]
pub enum LkError {
    /// The room or the participant is not there (already left, never joined).
    #[error("not found")]
    NotFound,
    /// The hub runs without LiveKit (`--no-livekit`, the API tests): nothing to enforce there.
    #[error("livekit disabled")]
    Disabled,
    #[error("{0}")]
    Other(String),
}

/// One published track, as `GetParticipant` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub sid: String,
    /// `"MICROPHONE"`, `"CAMERA"`, `"SCREEN_SHARE"`, `"SCREEN_SHARE_AUDIO"` or `"UNKNOWN"`.
    pub source: String,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantInfo {
    pub identity: String,
    pub tracks: Vec<TrackInfo>,
    /// The microphone is allowed by the participant's current permission.
    pub can_publish_mic: bool,
    /// When this participant session joined (unix seconds; 0 if LiveKit did not say).
    pub joined_at: i64,
}

#[derive(Clone)]
pub struct LiveKitAdmin {
    http: reqwest::Client,
    base: String,
    key: String,
    secret: String,
    enabled: bool,
}

/// LiveKit's protojson writes enums by name; accept numbers too.
fn source_name(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => match n.as_i64() {
            Some(1) => "CAMERA",
            Some(2) => "MICROPHONE",
            Some(3) => "SCREEN_SHARE",
            Some(4) => "SCREEN_SHARE_AUDIO",
            _ => "UNKNOWN",
        }
        .to_string(),
        _ => "UNKNOWN".into(),
    }
}

#[derive(Deserialize)]
struct TwirpError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    msg: String,
}

impl LiveKitAdmin {
    /// `enabled = false` (the hub was started with `--no-livekit`) turns every call into
    /// [`LkError::Disabled`].
    pub fn new(cfg: &Config, enabled: bool) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()?;
        Ok(LiveKitAdmin {
            http,
            base: format!("http://127.0.0.1:{}", cfg.livekit.signal_port),
            key: cfg.livekit.api_key.clone(),
            secret: cfg.livekit.api_secret.clone(),
            enabled,
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// `room`: the room the call is about (`None`: `ListRooms`).
    async fn call(&self, method: &str, room: Option<&str>, body: Value) -> Result<Value, LkError> {
        if !self.enabled {
            return Err(LkError::Disabled);
        }
        let url = format!("{}/twirp/livekit.RoomService/{method}", self.base);
        let token = admin_token(&self.key, &self.secret, room, now_ms() / 1000);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| LkError::Other(format!("{method}: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| LkError::Other(format!("{method}: {e}")))?;
        if status.is_success() {
            return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
        }
        let err: TwirpError = serde_json::from_str(&text).unwrap_or(TwirpError { code: String::new(), msg: text.clone() });
        if status.as_u16() == 404 || err.code == "not_found" {
            return Err(LkError::NotFound);
        }
        Err(LkError::Other(format!("{method}: HTTP {status}: {} {}", err.code, err.msg)))
    }

    /// The permission a member has in a room (the same sources as the join token);
    /// `server_muted` drops the microphone.
    fn permission(server_muted: bool) -> Value {
        let sources: Vec<String> = publish_sources(server_muted).iter().map(|s| s.to_ascii_uppercase()).collect();
        json!({ "canSubscribe": true, "canPublish": true, "canPublishData": true, "canPublishSources": sources })
    }

    /// Server-mute / unmute. Muting removes the microphone from what the participant may publish:
    /// LiveKit unpublishes a microphone that is live and refuses a new one. Unmuting gives the
    /// permission back (a current client publishes its microphone again by itself).
    pub async fn set_mic_allowed(&self, room: &str, identity: &str, allowed: bool) -> Result<(), LkError> {
        let body = json!({ "room": room, "identity": identity, "permission": Self::permission(!allowed) });
        self.call("UpdateParticipant", Some(room), body).await.map(|_| ())
    }

    /// Disconnect a participant from the room.
    pub async fn remove(&self, room: &str, identity: &str) -> Result<(), LkError> {
        self.call("RemoveParticipant", Some(room), json!({ "room": room, "identity": identity })).await.map(|_| ())
    }

    pub async fn mute_track(&self, room: &str, identity: &str, track_sid: &str, muted: bool) -> Result<(), LkError> {
        let body = json!({ "room": room, "identity": identity, "trackSid": track_sid, "muted": muted });
        self.call("MutePublishedTrack", Some(room), body).await.map(|_| ())
    }

    pub async fn participant(&self, room: &str, identity: &str) -> Result<ParticipantInfo, LkError> {
        let v = self.call("GetParticipant", Some(room), json!({ "room": room, "identity": identity })).await?;
        Ok(parse_participant(&v))
    }

    /// The names of the rooms LiveKit has right now.
    pub async fn list_rooms(&self) -> Result<Vec<String>, LkError> {
        let v = self.call("ListRooms", None, json!({})).await?;
        Ok(v["rooms"].as_array().map(|a| a.iter().filter_map(|r| r["name"].as_str().map(str::to_string)).collect()).unwrap_or_default())
    }

    /// Everybody in `room` right now.
    pub async fn list_participants(&self, room: &str) -> Result<Vec<ParticipantInfo>, LkError> {
        let v = self.call("ListParticipants", Some(room), json!({ "room": room })).await?;
        Ok(v["participants"].as_array().map(|a| a.iter().map(parse_participant).collect()).unwrap_or_default())
    }

    /// End someone's Go Live at the SFU: mute every screen-share track they publish. Returns how
    /// many tracks were muted.
    pub async fn mute_screen_share(&self, room: &str, identity: &str) -> Result<usize, LkError> {
        let p = self.participant(room, identity).await?;
        let mut n = 0;
        for t in p.tracks.iter().filter(|t| t.source.starts_with("SCREEN_SHARE") && !t.muted) {
            self.mute_track(room, identity, &t.sid, true).await?;
            n += 1;
        }
        Ok(n)
    }
}

fn parse_participant(v: &Value) -> ParticipantInfo {
    let tracks = v["tracks"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|t| TrackInfo {
                    sid: t["sid"].as_str().unwrap_or_default().to_string(),
                    source: source_name(&t["source"]),
                    muted: t["muted"].as_bool().unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default();
    let perm = &v["permission"];
    // protojson may use the JSON (camelCase) or the proto (snake_case) field names.
    let field = |camel: &str, snake: &str| if perm[camel].is_null() { perm[snake].clone() } else { perm[camel].clone() };
    let sources: Vec<String> =
        field("canPublishSources", "can_publish_sources").as_array().map(|a| a.iter().map(source_name).collect()).unwrap_or_default();
    let can_publish = field("canPublish", "can_publish").as_bool().unwrap_or(false);
    // protojson writes int64 as a string.
    let int = |x: &Value| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok()));
    let joined_at = int(&v["joinedAt"]).or_else(|| int(&v["joined_at"])).unwrap_or(0);
    ParticipantInfo {
        identity: v["identity"].as_str().unwrap_or_default().to_string(),
        tracks,
        can_publish_mic: can_publish && (sources.is_empty() || sources.iter().any(|s| s == "MICROPHONE")),
        joined_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn participant_json_is_read_with_names_or_numbers() {
        let v = json!({
            "identity": "7",
            "permission": { "canPublish": true, "canPublishSources": ["CAMERA", "SCREEN_SHARE", "SCREEN_SHARE_AUDIO"] },
            "tracks": [ { "sid": "TR_a", "source": "MICROPHONE" }, { "sid": "TR_b", "source": 3, "muted": true } ],
            "joinedAt": "1700000000",
        });
        let p = parse_participant(&v);
        assert_eq!(p.identity, "7");
        assert_eq!(p.joined_at, 1_700_000_000);
        assert!(!p.can_publish_mic);
        assert_eq!(p.tracks[0], TrackInfo { sid: "TR_a".into(), source: "MICROPHONE".into(), muted: false });
        assert_eq!(p.tracks[1].source, "SCREEN_SHARE");
        assert!(p.tracks[1].muted);
        let open = parse_participant(&json!({ "identity": "8", "permission": { "canPublish": true } }));
        assert!(open.can_publish_mic && open.tracks.is_empty());
    }

    #[test]
    fn mute_permission_keeps_everything_but_the_mic() {
        let p = LiveKitAdmin::permission(true);
        assert_eq!(p["canPublishSources"], json!(["CAMERA", "SCREEN_SHARE", "SCREEN_SHARE_AUDIO"]));
        assert_eq!(p["canSubscribe"], json!(true));
        assert_eq!(LiveKitAdmin::permission(false)["canPublishSources"], json!(["CAMERA", "MICROPHONE", "SCREEN_SHARE", "SCREEN_SHARE_AUDIO"]));
    }
}
