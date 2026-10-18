//! Voice presence is checked against LiveKit's real rooms (`voice_guard.rs`), and `VoiceState`
//! floods are held back. A real hub on an ephemeral port; LiveKit is a small fake of its server
//! API (Twirp `RoomService` over HTTP) that keeps rooms and participants and records every call.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode as AxStatus};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start_tuned, Options, Server, Tuning};

const PASSWORD: &str = "hunter2";

// ---------------------------------------------------------------- the fake LiveKit

#[derive(Clone, Debug)]
struct Part {
    identity: String,
    joined_at: i64,
    /// `None`: every source (a token without `canPublishSources`).
    sources: Option<Vec<String>>,
}

#[derive(Default)]
struct Fake {
    rooms: BTreeMap<String, Vec<Part>>,
    /// (method, body) of every call.
    calls: Vec<(String, Value)>,
}

type FakeRef = Arc<Mutex<Fake>>;

fn part_json(p: &Part) -> Value {
    let mut perm = json!({ "canPublish": true, "canSubscribe": true });
    if let Some(s) = &p.sources {
        perm["canPublishSources"] = json!(s);
    }
    json!({ "identity": p.identity, "joinedAt": p.joined_at.to_string(), "permission": perm, "tracks": [] })
}

fn not_found() -> Response {
    (AxStatus::NOT_FOUND, Json(json!({ "code": "not_found", "msg": "not found" }))).into_response()
}

async fn twirp(State(f): State<FakeRef>, Path(method): Path<String>, headers: HeaderMap, body: String) -> Response {
    assert!(headers.get("authorization").is_some_and(|v| v.to_str().unwrap().starts_with("Bearer ")), "admin calls carry a token");
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let mut g = f.lock().unwrap();
    g.calls.push((method.clone(), v.clone()));
    let room = v["room"].as_str().unwrap_or_default().to_string();
    let identity = v["identity"].as_str().unwrap_or_default().to_string();
    match method.as_str() {
        "ListRooms" => Json(json!({ "rooms": g.rooms.keys().map(|n| json!({ "name": n })).collect::<Vec<_>>() })).into_response(),
        "ListParticipants" => match g.rooms.get(&room) {
            Some(list) => Json(json!({ "participants": list.iter().map(part_json).collect::<Vec<_>>() })).into_response(),
            None => not_found(),
        },
        "GetParticipant" => match g.rooms.get(&room).and_then(|l| l.iter().find(|p| p.identity == identity)) {
            Some(p) => Json(part_json(p)).into_response(),
            None => not_found(),
        },
        "RemoveParticipant" => {
            let Some(list) = g.rooms.get_mut(&room) else { return not_found() };
            let before = list.len();
            list.retain(|p| p.identity != identity);
            if list.len() == before {
                return not_found();
            }
            if list.is_empty() {
                g.rooms.remove(&room);
            }
            Json(json!({})).into_response()
        }
        "UpdateParticipant" => {
            let Some(p) = g.rooms.get_mut(&room).and_then(|l| l.iter_mut().find(|p| p.identity == identity)) else { return not_found() };
            p.sources = v["permission"]["canPublishSources"].as_array().map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect());
            Json(part_json(p)).into_response()
        }
        _ => Json(json!({})).into_response(),
    }
}

impl Fake {
    fn join(f: &FakeRef, room: &str, user: UserId, sources: Option<Vec<String>>) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        f.lock().unwrap().rooms.entry(room.to_string()).or_default().push(Part { identity: user.to_string(), joined_at: now, sources });
    }

    fn removals(f: &FakeRef, room: &str, user: UserId) -> usize {
        let id = user.to_string();
        f.lock().unwrap().calls.iter().filter(|(m, v)| m == "RemoveParticipant" && v["room"] == room && v["identity"] == id.as_str()).count()
    }

    fn present(f: &FakeRef, room: &str, user: UserId) -> bool {
        let id = user.to_string();
        f.lock().unwrap().rooms.get(room).is_some_and(|l| l.iter().any(|p| p.identity == id))
    }

    fn sources(f: &FakeRef, room: &str, user: UserId) -> Option<Option<Vec<String>>> {
        let id = user.to_string();
        f.lock().unwrap().rooms.get(room).and_then(|l| l.iter().find(|p| p.identity == id)).map(|p| p.sources.clone())
    }
}

async fn fake_livekit() -> (FakeRef, u16) {
    let fake: FakeRef = Arc::default();
    let app = Router::new().route("/twirp/livekit.RoomService/{method}", post(twirp)).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (fake, port)
}

// ---------------------------------------------------------------- the hub

struct Env {
    _server: Server,
    base: String,
    http: reqwest::Client,
    fake: FakeRef,
    _dir: tempfile::TempDir,
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

async fn setup(grace: Duration) -> Env {
    let (fake, port) = fake_livekit().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"testsrv\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = {port}\n"
        ),
    )
    .unwrap();
    let opts = Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) };
    let tuning = Tuning { livekit_api: true, voice_check_every: Duration::from_millis(150), voice_grace: grace, ..Tuning::default() };
    let server = start_tuned(opts, Default::default(), tuning).await.unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { _server: server, base, http: reqwest::Client::new(), fake, _dir: dir }
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn who(&self, name: &str) -> Who {
        let req = LoginRequest { username: name.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        let r = self.http.post(self.url("/api/login")).json(&req).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let l: LoginResponse = r.json().await.unwrap();
        Who { token: l.token, user: l.user }
    }

    /// The owner, with a personal password (set from this PC, 127.0.0.1).
    async fn owner(&self) -> Who {
        let o = self.who("Dono").await;
        assert_eq!(self.post(&o, "/api/account/password", json!({ "new": "dono-senha" })).await.status(), StatusCode::OK);
        o
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn join(&self, who: &Who, channel: ChannelId) -> VoiceJoinResponse {
        let r = self.post(who, &format!("/api/voice/{channel}/join"), json!({})).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn voice_ids(&self, who: &Who) -> (ChannelId, ChannelId) {
        let chs: Vec<Channel> = self.http.get(self.url("/api/channels")).bearer_auth(&who.token).send().await.unwrap().json().await.unwrap();
        let v: Vec<ChannelId> = chs.iter().filter(|c| c.kind == ChannelKind::Voice).map(|c| c.id).collect();
        (v[0], v[1])
    }

    async fn ws(&self, who: &Who) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        let mut ws = Ws(ws);
        assert!(matches!(ws.next_within(Duration::from_secs(5)).await, Some(ServerEvent::Ready { .. })));
        ws
    }
}

struct Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>);

impl Ws {
    async fn next_within(&mut self, wait: Duration) -> Option<ServerEvent> {
        loop {
            match tokio::time::timeout(wait, self.0.next()).await.ok()?? {
                Ok(WsMsg::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
                Ok(WsMsg::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn voice(&mut self, channel: Option<ChannelId>, muted: bool) {
        let ev = ClientEvent::VoiceState { channel_id: channel, muted, deafened: false, streaming: false, camera: false, audio_only: false };
        self.0.send(WsMsg::Text(serde_json::to_string(&ev).unwrap().into())).await.unwrap();
    }

    /// Every `VoiceStateChanged` about `user` within `wait`.
    async fn voice_events_of(&mut self, user: UserId, wait: Duration) -> Vec<VoiceMember> {
        let until = Instant::now() + wait;
        let mut out = Vec::new();
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match self.next_within(left).await {
                Some(ServerEvent::VoiceStateChanged { member }) if member.user.id == user => out.push(member),
                Some(_) => {}
                None => break,
            }
        }
        out
    }
}

fn claims(token: &str) -> Value {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap()).unwrap()
}

async fn eventually(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("not within {secs} s: {what}");
}

fn has_mic(sources: &Option<Vec<String>>) -> bool {
    sources.as_ref().is_none_or(|s| s.iter().any(|x| x.eq_ignore_ascii_case("microphone")))
}

// ---------------------------------------------------------------- tests

/// A member who joins a room without declaring it is not out of reach: moderation finds the real
/// participant, and the hub removes them from the room after the grace period.
#[tokio::test]
async fn unlisted_participants_are_moderated_and_removed() {
    let env = setup(Duration::from_millis(1500)).await;
    let owner = env.owner().await;
    let (eve, ana) = (env.who("eve").await, env.who("ana").await);
    let (v1, v2) = env.voice_ids(&ana).await;
    let (r1, r2) = (format!("voice-{v1}"), format!("voice-{v2}"));

    // Join tokens: a minute to enter (v1.0.1), and only the app's four kinds of tracks.
    let join = env.join(&eve, v1).await;
    let c = claims(&join.livekit_token);
    assert_eq!(c["exp"].as_i64().unwrap() - c["nbf"].as_i64().unwrap(), 60 + 10);
    assert_eq!(c["video"]["canPublishSources"], json!(["camera", "microphone", "screen_share", "screen_share_audio"]));

    // Eve sits in the room without telling the hub; Ana does it properly.
    Fake::join(&env.fake, &r1, eve.user.id, None);
    let mut ws_ana = env.ws(&ana).await;
    ws_ana.voice(Some(v1), false).await;
    Fake::join(&env.fake, &r1, ana.user.id, None);

    // A server mute reaches the unlisted participant in LiveKit.
    assert_eq!(env.post(&owner, &format!("/api/mod/users/{}/mute", eve.user.id), json!({ "muted": true })).await.status(), StatusCode::OK);
    eventually("eve's microphone taken away", 3, || Fake::sources(&env.fake, &r1, eve.user.id).is_some_and(|s| !has_mic(&s))).await;

    // ...and she is removed for being unlisted; Ana is not.
    eventually("eve removed from the room", 5, || !Fake::present(&env.fake, &r1, eve.user.id)).await;
    assert_eq!(Fake::removals(&env.fake, &r1, ana.user.id), 0, "listed people stay");
    assert!(Fake::present(&env.fake, &r1, ana.user.id));

    // Disconnect works on an unlisted participant too (it used to answer 409 "not in voice").
    Fake::join(&env.fake, &r2, eve.user.id, None);
    assert_eq!(env.post(&owner, &format!("/api/mod/users/{}/disconnect", eve.user.id), json!({})).await.status(), StatusCode::OK);
    eventually("eve disconnected from the second room", 4, || Fake::removals(&env.fake, &r2, eve.user.id) >= 1).await;
    // Nobody anywhere: now it is a 409.
    eventually("rooms empty of eve", 3, || !Fake::present(&env.fake, &r2, eve.user.id)).await;
    assert_eq!(env.post(&owner, &format!("/api/mod/users/{}/disconnect", eve.user.id), json!({})).await.status(), StatusCode::CONFLICT);
}

/// After a moderator takes somebody out, a token from before does not bring them back (even
/// declared); a fresh token does.
#[tokio::test]
async fn old_tokens_do_not_bring_a_removed_member_back() {
    let env = setup(Duration::from_secs(60)).await; // the grace never runs out here
    let owner = env.owner().await;
    let eve = env.who("eve").await;
    let (v1, _) = env.voice_ids(&eve).await;
    let r1 = format!("voice-{v1}");
    env.join(&eve, v1).await;
    let mut ws_eve = env.ws(&eve).await;
    ws_eve.voice(Some(v1), false).await;
    Fake::join(&env.fake, &r1, eve.user.id, None);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(Fake::removals(&env.fake, &r1, eve.user.id), 0, "listed: left alone");

    assert_eq!(env.post(&owner, &format!("/api/mod/users/{}/disconnect", eve.user.id), json!({})).await.status(), StatusCode::OK);
    eventually("eve removed", 4, || !Fake::present(&env.fake, &r1, eve.user.id)).await;
    // Joined in the revocation's second, the voice check may take her out before the
    // disconnect's own (delayed) removal: let that one land too before counting.
    tokio::time::sleep(voxhub_server::admin::DISCONNECT_GRACE + Duration::from_millis(300)).await;
    let removed = Fake::removals(&env.fake, &r1, eve.user.id);

    // Back with the old token, and declaring the room: removed again at once (a later second
    // than the removal, after the wait above).
    Fake::join(&env.fake, &r1, eve.user.id, None);
    ws_eve.voice(Some(v1), false).await;
    eventually("the old token's session removed", 3, || Fake::removals(&env.fake, &r1, eve.user.id) > removed).await;

    // A fresh token from the hub: she may stay.
    env.join(&eve, v1).await;
    Fake::join(&env.fake, &r1, eve.user.id, None);
    ws_eve.voice(Some(v1), true).await;
    let now = Fake::removals(&env.fake, &r1, eve.user.id);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(Fake::removals(&env.fake, &r1, eve.user.id), now);
    assert!(Fake::present(&env.fake, &r1, eve.user.id));
}

/// A server mute holds against a token from before it (the hub corrects LiveKit's permission),
/// and private call tokens leave the microphone out too.
#[tokio::test]
async fn server_mute_holds_on_old_tokens_and_in_dm_calls() {
    let env = setup(Duration::from_secs(60)).await;
    let owner = env.owner().await;
    let (eve, ana) = (env.who("eve").await, env.who("ana").await);
    let (v1, _) = env.voice_ids(&eve).await;
    let r1 = format!("voice-{v1}");
    let mut ws_eve = env.ws(&eve).await;
    assert_eq!(env.post(&owner, &format!("/api/mod/users/{}/mute", eve.user.id), json!({ "muted": true })).await.status(), StatusCode::OK);
    // A token from before the mute (every source) in a room she declares.
    Fake::join(&env.fake, &r1, eve.user.id, None);
    ws_eve.voice(Some(v1), false).await;
    eventually("the microphone taken away", 3, || Fake::sources(&env.fake, &r1, eve.user.id).is_some_and(|s| !has_mic(&s))).await;

    // DM call tokens: no microphone for her, the usual four for Ana.
    let dm: DmConversation = env.post(&eve, &format!("/api/dms/{}", ana.user.id), json!({})).await.json().await.unwrap();
    let call: DmCallResponse = env.post(&eve, &format!("/api/dms/{}/call", dm.id), json!({})).await.json().await.unwrap();
    assert_eq!(claims(&call.join.livekit_token)["video"]["canPublishSources"], json!(["camera", "screen_share", "screen_share_audio"]));
    let accepted: DmCallResponse = env.post(&ana, &format!("/api/dms/{}/call/accept", dm.id), json!({})).await.json().await.unwrap();
    assert_eq!(claims(&accepted.join.livekit_token)["video"]["canPublishSources"], json!(["camera", "microphone", "screen_share", "screen_share_audio"]));
}

/// The same `VoiceState` again and again reaches nobody; a flood of changes is held back to the
/// rate limit, and the newest state still arrives.
#[tokio::test]
async fn voice_state_floods_are_dropped_and_coalesced() {
    let env = setup(Duration::from_secs(60)).await;
    let (bob, cid) = (env.who("bob").await, env.who("cid").await);
    let (v1, _) = env.voice_ids(&bob).await;
    let mut ws_cid = env.ws(&cid).await;
    let mut ws_bob = env.ws(&bob).await;
    // Bob keeps reading, so his own socket never lags.
    for _ in 0..300 {
        ws_bob.voice(Some(v1), false).await;
    }
    let seen = ws_cid.voice_events_of(bob.user.id, Duration::from_millis(1500)).await;
    assert_eq!(seen.len(), 1, "one change, told once: {seen:?}");

    for i in 0..200 {
        ws_bob.voice(Some(v1), i % 2 == 0).await;
    }
    ws_bob.voice(Some(v1), true).await;
    let seen = ws_cid.voice_events_of(bob.user.id, Duration::from_secs(3)).await;
    assert!(seen.len() <= 15, "held back to the rate limit: {}", seen.len());
    assert!(seen.last().is_some_and(|m| m.muted && m.channel_id == Some(v1)), "the newest state arrives: {:?}", seen.last());
    drop(ws_bob);
}
