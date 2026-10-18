//! End-to-end tests: real server on an ephemeral port, no LiveKit, temp data dir.

use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::StatusCode;
use serde_json::Value;
use sha2::Sha256;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "hunter2";
const API_KEY: &str = "APItest0000";
const API_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

struct Env {
    server: Server,
    base: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    // signal_port 1: nothing listens there, so health reports livekit_ok = false.
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"testsrv\"\nserver_password = \"{PASSWORD}\"\npublic_host = \"voice.example.org\"\n\
             [livekit]\napi_key = \"{API_KEY}\"\napi_secret = \"{API_SECRET}\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let server = start(Options {
        data_dir: dir.path().to_path_buf(),
        no_livekit: true,
        livekit_bin: "unused".into(),
        bind_override: Some("127.0.0.1:0".into()),
    })
    .await
    .unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { server, base, http: reqwest::Client::new(), _dir: dir }
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn login_raw(&self, user: &str, password: &str, version: u32) -> reqwest::Response {
        let req = LoginRequest { username: user.into(), server_password: password.into(), protocol_version: version, ..Default::default() };
        self.http.post(self.url("/api/login")).json(&req).send().await.unwrap()
    }

    async fn login(&self, user: &str) -> LoginResponse {
        let r = self.login_raw(user, PASSWORD, PROTOCOL_VERSION).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn get(&self, path: &str, token: &str) -> reqwest::Response {
        self.http.get(self.url(path)).bearer_auth(token).send().await.unwrap()
    }

    async fn ws(&self, token: &str) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={token}", self.base)).await.unwrap();
        Ws(ws)
    }
}

struct Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>);

impl Ws {
    /// Next server event (control frames skipped); None on close or after 5 s.
    async fn next(&mut self) -> Option<ServerEvent> {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.0.next()).await.ok()?? {
                Ok(WsMsg::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
                Ok(WsMsg::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn expect(&mut self) -> ServerEvent {
        self.next().await.expect("expected an event")
    }

    async fn send(&mut self, ev: &ClientEvent) {
        self.0.send(WsMsg::Text(serde_json::to_string(ev).unwrap().into())).await.unwrap();
    }

    async fn send_raw(&mut self, json: String) {
        self.0.send(WsMsg::Text(json.into())).await.unwrap();
    }

    /// Assert nothing arrives for a short while.
    async fn expect_quiet(&mut self) {
        let r = tokio::time::timeout(Duration::from_millis(300), self.0.next()).await;
        assert!(r.is_err(), "unexpected frame: {r:?}");
    }
}

fn channel_id(chs: &[Channel], name: &str) -> i64 {
    chs.iter().find(|c| c.name == name).unwrap().id
}

#[tokio::test]
async fn health_needs_no_auth() {
    let env = setup().await;
    let h: HealthResponse = env.http.get(env.url("/api/health")).send().await.unwrap().json().await.unwrap();
    assert!(h.ok);
    assert_eq!(h.protocol_version, PROTOCOL_VERSION);
    assert!(!h.livekit_ok);
    env.server.shutdown().await;
}

#[tokio::test]
async fn login_rules() {
    let env = setup().await;

    let ok = env.login("  Ana ").await;
    assert_eq!(ok.user.username, "Ana");
    assert_eq!(ok.server_name, "testsrv");
    assert_eq!(ok.token.len(), 64);

    // Same user (case-insensitive) keeps its id and gets a fresh token.
    let again = env.login("ana").await;
    assert_eq!(again.user, ok.user);
    assert_ne!(again.token, ok.token);

    let bad_pw = env.login_raw("Ana", "nope", PROTOCOL_VERSION).await;
    assert_eq!(bad_pw.status(), StatusCode::UNAUTHORIZED);
    let err: ApiError = bad_pw.json().await.unwrap();
    assert!(!err.error.is_empty());

    let bad_ver = env.login_raw("Ana", PASSWORD, PROTOCOL_VERSION + 1).await;
    assert_eq!(bad_ver.status(), StatusCode::BAD_REQUEST);
    assert!(bad_ver.json::<ApiError>().await.unwrap().error.contains("version"));

    for name in ["a", &"x".repeat(33), "  "] {
        let r = env.login_raw(name, PASSWORD, PROTOCOL_VERSION).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "username {name:?}");
    }

    let junk = env.http.post(env.url("/api/login")).body("not json").header("content-type", "application/json").send().await.unwrap();
    assert_eq!(junk.status(), StatusCode::BAD_REQUEST);
    assert!(junk.json::<ApiError>().await.is_ok());
    env.server.shutdown().await;
}

#[tokio::test]
async fn channels_and_auth() {
    let env = setup().await;
    assert_eq!(env.http.get(env.url("/api/channels")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(env.get("/api/channels", "deadbeef").await.status(), StatusCode::UNAUTHORIZED);

    let me = env.login("Bruno").await;
    let chs: Vec<Channel> = env.get("/api/channels", &me.token).await.json().await.unwrap();
    let got: Vec<_> = chs.iter().map(|c| (c.name.as_str(), c.kind)).collect();
    assert_eq!(got, [("geral", ChannelKind::Text), ("Sala 1", ChannelKind::Voice), ("Sala 2", ChannelKind::Voice)]);

    assert_eq!(env.get("/api/channels/999/messages", &me.token).await.status(), StatusCode::NOT_FOUND);
    env.server.shutdown().await;
}

/// v1.0.1: the hub's identity, the same in `/api/health` and `Ready`, kept across restarts (a
/// fresh data folder is another server).
#[tokio::test]
async fn the_server_id_is_stable_and_in_health_and_ready() {
    let env = setup().await;
    let health = || async { env.http.get(env.url("/api/health")).send().await.unwrap().json::<HealthResponse>().await.unwrap() };
    let id = health().await.server_id.expect("a server id");
    assert!(is_server_id(&id), "{id}");
    assert_eq!(health().await.server_id.as_deref(), Some(id.as_str()));
    let me = env.login("Carla").await;
    let mut ws = env.ws(&me.token).await;
    match ws.expect().await {
        ServerEvent::Ready { server_id, .. } => assert_eq!(server_id.as_deref(), Some(id.as_str())),
        other => panic!("expected Ready, got {other:?}"),
    }
    drop(ws);
    let dir = env._dir;
    env.server.shutdown().await;
    let again = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) }).await.unwrap();
    let h: HealthResponse = reqwest::get(format!("http://127.0.0.1:{}/api/health", again.addr.port())).await.unwrap().json().await.unwrap();
    assert_eq!(h.server_id.as_deref(), Some(id.as_str()), "the same server after a restart");
    again.shutdown().await;
    let other = setup().await;
    let h: HealthResponse = other.http.get(other.url("/api/health")).send().await.unwrap().json().await.unwrap();
    assert_ne!(h.server_id.as_deref(), Some(id.as_str()), "another data folder, another server");
    other.server.shutdown().await;
}

#[tokio::test]
async fn voice_join_token() {
    let env = setup().await;
    let me = env.login("Carla").await;
    let chs: Vec<Channel> = env.get("/api/channels", &me.token).await.json().await.unwrap();
    let (text, voice) = (channel_id(&chs, "geral"), channel_id(&chs, "Sala 2"));

    let join = |id: i64, token: String| {
        let (http, url) = (env.http.clone(), env.url(&format!("/api/voice/{id}/join")));
        async move { http.post(url).bearer_auth(token).send().await.unwrap() }
    };
    assert_eq!(join(voice, "bad".into()).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(join(text, me.token.clone()).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(join(999, me.token.clone()).await.status(), StatusCode::NOT_FOUND);

    let r: VoiceJoinResponse = join(voice, me.token.clone()).await.json().await.unwrap();
    assert_eq!(r.room, format!("voice-{voice}"));
    // LiveKit host follows the address the client used to reach the hub, not public_host.
    assert_eq!(r.livekit_url, "ws://127.0.0.1:1");

    // Decode and verify the JWT the way LiveKit would.
    let parts: Vec<&str> = r.livekit_token.split('.').collect();
    assert_eq!(parts.len(), 3);
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(API_SECRET.as_bytes()).unwrap();
    mac.update(format!("{}.{}", parts[0], parts[1]).as_bytes());
    mac.verify_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).expect("bad signature");
    let header: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(header["alg"], "HS256");
    let c: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(c["iss"], API_KEY);
    assert_eq!(c["sub"], me.user.id.to_string());
    assert_eq!(c["name"], "Carla");
    // A minute to enter the room (v1.0.1; a session in it lives on with LiveKit's refreshed tokens).
    assert_eq!(c["exp"].as_i64().unwrap() - c["nbf"].as_i64().unwrap(), 60 + 10);
    let v = &c["video"];
    assert_eq!(v["room"], format!("voice-{voice}"));
    for grant in ["roomJoin", "canPublish", "canSubscribe", "canPublishData"] {
        assert_eq!(v[grant], true, "{grant}");
    }
    env.server.shutdown().await;
}

#[tokio::test]
async fn websocket_chat_presence_and_voice() {
    let env = setup().await;
    let (a, b) = (env.login("Ana").await, env.login("Beto").await);

    assert!(
        tokio_tungstenite::connect_async(format!("ws://{}/ws?token=nope", env.base)).await.is_err(),
        "bad token must be refused"
    );

    let mut wa = env.ws(&a.token).await;
    let ServerEvent::Ready { user, channels, online, voice, .. } = wa.expect().await else { panic!("expected Ready") };
    assert_eq!(user, a.user);
    assert_eq!(channels.len(), 3);
    assert_eq!(online, vec![a.user.clone()]);
    assert!(voice.is_empty());
    let (text, room) = (channel_id(&channels, "geral"), channel_id(&channels, "Sala 1"));

    // Beto connects: Ana sees him come online, Beto's snapshot lists both.
    let mut wb = env.ws(&b.token).await;
    let ServerEvent::Ready { online, .. } = wb.expect().await else { panic!("expected Ready") };
    assert_eq!(online.len(), 2);
    assert!(matches!(wa.expect().await, ServerEvent::UserOnline { user } if user == b.user));

    // A second connection for Beto does not announce anything.
    let mut wb2 = env.ws(&b.token).await;
    assert!(matches!(wb2.expect().await, ServerEvent::Ready { .. }));
    wa.expect_quiet().await;

    // Chat: Ana sends, Beto receives (and Ana gets her own copy).
    wa.send(&ClientEvent::SendMessage { channel_id: text, content: "  olá, mundo  ".into(), mentions: vec![], reply_to: None, reply_mention: false }).await;
    for w in [&mut wb, &mut wa] {
        let ServerEvent::MessageCreated { message } = w.expect().await else { panic!("expected MessageCreated") };
        assert_eq!(message.content, "olá, mundo");
        assert_eq!(message.author, a.user);
        assert_eq!(message.channel_id, text);
    }
    assert!(matches!(wb2.expect().await, ServerEvent::MessageCreated { .. }));

    // Validation errors go only to the sender.
    wa.send(&ClientEvent::SendMessage { channel_id: text, content: "   ".into(), mentions: vec![], reply_to: None, reply_mention: false }).await;
    assert!(matches!(wa.expect().await, ServerEvent::Error { .. }));
    wa.send(&ClientEvent::SendMessage { channel_id: text, content: "x".repeat(4001), mentions: vec![], reply_to: None, reply_mention: false }).await;
    assert!(matches!(wa.expect().await, ServerEvent::Error { .. }));
    wa.send(&ClientEvent::SendMessage { channel_id: room, content: "voz".into(), mentions: vec![], reply_to: None, reply_mention: false }).await;
    assert!(matches!(wa.expect().await, ServerEvent::Error { .. }));
    wa.send(&ClientEvent::Ping { nonce: 7 }).await;
    assert!(matches!(wa.expect().await, ServerEvent::Pong { nonce: 7 }));
    wb.expect_quiet().await;

    // History over HTTP, newest first.
    let hist: Vec<Message> = env.get(&format!("/api/channels/{text}/messages?limit=10"), &a.token).await.json().await.unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].content, "olá, mundo");

    // Voice state is fanned out and shows up in later snapshots.
    wb.send(&ClientEvent::VoiceState { channel_id: Some(room), muted: true, deafened: false, streaming: false, camera: false, audio_only: false }).await;
    let expected = VoiceMember { user: b.user.clone(), channel_id: Some(room), muted: true, deafened: false, streaming: false, watching: vec![], camera: false, server_muted: false, audio_only: false };
    assert!(matches!(wa.expect().await, ServerEvent::VoiceStateChanged { member } if member == expected));
    let mut wc = env.ws(&env.login("Cris").await.token).await;
    let ServerEvent::Ready { voice, .. } = wc.expect().await else { panic!("expected Ready") };
    assert_eq!(voice, vec![expected.clone()]);
    drop(wc);

    // Drain whatever Beto's other sockets queued, then close one: still online.
    drop(wb2);
    // Closing the last Beto connection clears voice, then announces offline.
    drop(wb);
    let mut seen = Vec::new();
    while let Some(ev) = wa.next().await {
        match ev {
            ServerEvent::VoiceStateChanged { member } if member.user == b.user => {
                assert_eq!(member.channel_id, None);
                seen.push("voice_cleared");
            }
            ServerEvent::UserOffline { user_id } if user_id == b.user.id => {
                seen.push("offline");
                break;
            }
            _ => {}
        }
    }
    assert_eq!(seen, ["voice_cleared", "offline"]);
    env.server.shutdown().await;
}

#[tokio::test]
async fn chat_rate_limit() {
    let env = setup().await;
    let a = env.login("Rapido").await;
    let mut w = env.ws(&a.token).await;
    let ServerEvent::Ready { channels, .. } = w.expect().await else { panic!() };
    let text = channel_id(&channels, "geral");
    for i in 0..20 {
        w.send(&ClientEvent::SendMessage { channel_id: text, content: format!("m{i}"), mentions: vec![], reply_to: None, reply_mention: false }).await;
    }
    let (mut created, mut limited) = (0, 0);
    for _ in 0..20 {
        match w.expect().await {
            ServerEvent::MessageCreated { .. } => created += 1,
            ServerEvent::Error { .. } => limited += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!((5..=8).contains(&created), "created {created}");
    assert_eq!(created + limited, 20);
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- who is watching (item 7)

impl Ws {
    /// Everything that arrives until the socket has been quiet for 250 ms.
    async fn settle(&mut self) -> Vec<ServerEvent> {
        let mut out = Vec::new();
        while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_millis(250), self.0.next()).await {
            if let WsMsg::Text(t) = m {
                out.push(serde_json::from_str(&t).unwrap());
            }
        }
        out
    }

    async fn voice(&mut self, channel: Option<i64>, muted: bool, streaming: bool) {
        self.send(&ClientEvent::VoiceState { channel_id: channel, muted, deafened: false, streaming, camera: false, audio_only: false }).await;
    }

    async fn voice_cam(&mut self, channel: Option<i64>, camera: bool) {
        self.send(&ClientEvent::VoiceState { channel_id: channel, muted: false, deafened: false, streaming: false, camera, audio_only: false }).await;
    }

    async fn voice_live(&mut self, channel: Option<i64>, streaming: bool, audio_only: bool) {
        self.send(&ClientEvent::VoiceState { channel_id: channel, muted: false, deafened: false, streaming, camera: false, audio_only }).await;
    }

    async fn watch(&mut self, ids: &[i64]) {
        self.send(&ClientEvent::WatchState { watching: ids.to_vec() }).await;
    }
}

/// The `watching` list of `who` in the last `VoiceStateChanged` for them (None: no such event).
fn watching_of(events: &[ServerEvent], who: i64) -> Option<Vec<i64>> {
    events.iter().rev().find_map(|e| match e {
        ServerEvent::VoiceStateChanged { member } if member.user.id == who => Some(member.watching.clone()),
        _ => None,
    })
}

/// A fresh snapshot, as a late joiner would see it.
async fn snapshot_voice(env: &Env, name: &str) -> Vec<VoiceMember> {
    let mut w = env.ws(&env.login(name).await.token).await;
    let ServerEvent::Ready { voice, .. } = w.expect().await else { panic!("expected Ready") };
    voice
}

#[tokio::test]
async fn watch_state_is_validated_broadcast_and_cleared() {
    let env = setup().await;
    let (ana, beto, cris, dani) = (env.login("Ana").await, env.login("Beto").await, env.login("Cris").await, env.login("Dani").await);
    let (mut wa, mut wb, mut wc, mut wd) =
        (env.ws(&ana.token).await, env.ws(&beto.token).await, env.ws(&cris.token).await, env.ws(&dani.token).await);
    let ServerEvent::Ready { channels, .. } = wa.expect().await else { panic!("expected Ready") };
    let (room1, room2) = (channel_id(&channels, "Sala 1"), channel_id(&channels, "Sala 2"));
    let (a, b, c, d) = (ana.user.id, beto.user.id, cris.user.id, dani.user.id);

    // Ana and Beto in room 1; Cris streams in room 2; Dani is online but not in voice.
    wa.voice(Some(room1), false, false).await;
    wb.voice(Some(room1), false, false).await;
    wc.voice(Some(room2), false, true).await;
    for w in [&mut wa, &mut wb, &mut wc, &mut wd] {
        w.settle().await;
    }

    // Nothing is streaming in Beto's room yet, Cris is in another room, and himself and a stranger
    // are not valid either: the set stays empty and nothing is announced.
    wb.watch(&[a, b, c, d, 987_654]).await;
    for w in [&mut wa, &mut wb, &mut wc, &mut wd] {
        assert!(watching_of(&w.settle().await, b).is_none(), "an invalid set must not be announced");
    }

    // Ana goes live; Beto (duplicates, himself and a bad id mixed in) watches her.
    wa.voice(Some(room1), false, true).await;
    for w in [&mut wa, &mut wb, &mut wc, &mut wd] {
        w.settle().await;
    }
    wb.watch(&[a, a, b, c, 987_654]).await;
    for w in [&mut wa, &mut wb, &mut wc, &mut wd] {
        let ev = w.settle().await;
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(watching_of(&ev, b), Some(vec![a]), "everyone is told");
    }
    // A late joiner's snapshot carries it.
    let snap = snapshot_voice(&env, "Edu").await;
    assert_eq!(snap.iter().find(|m| m.user.id == b).unwrap().watching, vec![a]);
    assert!(snap.iter().find(|m| m.user.id == a).unwrap().watching.is_empty());

    // The same set again is not news.
    wb.watch(&[a]).await;
    assert!(wa.settle().await.iter().all(|e| !matches!(e, ServerEvent::VoiceStateChanged { .. })));

    // Muting (a VoiceState without any watch info, as old clients send) keeps the watch.
    wb.voice(Some(room1), true, false).await;
    let ev = wa.settle().await;
    assert_eq!(watching_of(&ev, b), Some(vec![a]));
    // Same for the streamer flipping mute while live: nobody is dropped.
    wb.settle().await;
    wa.voice(Some(room1), true, true).await;
    let ev = wb.settle().await;
    assert!(ev.iter().all(|e| !matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == b)), "{ev:?}");
    assert_eq!(snapshot_voice(&env, "Fran").await.iter().find(|m| m.user.id == b).unwrap().watching, vec![a]);

    // The streamer stops: first her own update, then Beto's cleared list.
    wc.settle().await;
    wa.voice(Some(room1), true, false).await;
    let ev = wc.settle().await;
    assert!(
        matches!(&ev[..], [
            ServerEvent::VoiceStateChanged { member: m1 },
            ServerEvent::VoiceStateChanged { member: m2 },
        ] if m1.user.id == a && !m1.streaming && m2.user.id == b && m2.watching.is_empty()),
        "{ev:?}"
    );
    // Going live again does not resurrect the watch.
    wa.settle().await;
    wb.settle().await;
    wa.voice(Some(room1), true, true).await;
    for w in [&mut wa, &mut wb] {
        let ev = w.settle().await;
        assert!(watching_of(&ev, b).is_none(), "{ev:?}");
    }

    // Watch again, then the viewer leaves voice: his list goes away with him.
    wb.watch(&[a]).await;
    assert_eq!(watching_of(&wa.settle().await, b), Some(vec![a]));
    wb.voice(None, false, false).await;
    let ev = wa.settle().await;
    assert!(
        matches!(&ev[..], [ServerEvent::VoiceStateChanged { member }] if member.user.id == b && member.channel_id.is_none() && member.watching.is_empty()),
        "{ev:?}"
    );
    // Joining back does not restore it either; and a watch from outside voice is ignored.
    wb.voice(Some(room1), false, false).await;
    wa.settle().await;
    wd.watch(&[a]).await;
    wb.settle().await;
    assert!(watching_of(&wa.settle().await, d).is_none());

    // Moving rooms: Beto watches, then Ana moves to room 2 (still live): the watch is dropped.
    wb.watch(&[a]).await;
    wa.settle().await;
    wa.voice(Some(room2), true, true).await;
    let ev = wb.settle().await;
    assert_eq!(watching_of(&ev, b), Some(vec![]), "{ev:?}");
    wa.voice(Some(room1), true, true).await;
    wb.settle().await;

    // The streamer going offline clears the viewer; so does the viewer going offline.
    wb.watch(&[a]).await;
    wc.settle().await;
    drop(wa);
    let ev = wc.settle().await;
    assert_eq!(watching_of(&ev, b), Some(vec![]), "{ev:?}");
    assert!(ev.iter().any(|e| matches!(e, ServerEvent::UserOffline { user_id } if *user_id == a)));
    let mut wa = env.ws(&ana.token).await;
    wa.settle().await;
    wa.voice(Some(room1), false, true).await;
    wb.settle().await;
    wb.watch(&[a]).await;
    assert_eq!(watching_of(&wa.settle().await, b), Some(vec![a]));
    drop(wb);
    let ev = wa.settle().await;
    assert_eq!(watching_of(&ev, b), Some(vec![]), "{ev:?}");
    assert!(snapshot_voice(&env, "Gabi").await.iter().all(|m| m.watching.is_empty()));
    env.server.shutdown().await;
}

/// The member of `who` in the last `VoiceStateChanged` for them.
fn member_of(events: &[ServerEvent], who: i64) -> Option<VoiceMember> {
    events.iter().rev().find_map(|e| match e {
        ServerEvent::VoiceStateChanged { member } if member.user.id == who => Some(member.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn camera_flag_is_broadcast_and_cleared_on_leave_and_offline() {
    let env = setup().await;
    let (ana, beto, cris) = (env.login("Ana").await, env.login("Beto").await, env.login("Cris").await);
    let (mut wa, mut wb, mut wc) = (env.ws(&ana.token).await, env.ws(&beto.token).await, env.ws(&cris.token).await);
    let ServerEvent::Ready { channels, .. } = wa.expect().await else { panic!("expected Ready") };
    let room = channel_id(&channels, "Sala 1");
    let (a, b) = (ana.user.id, beto.user.id);
    wa.voice_cam(Some(room), false).await;
    wb.voice_cam(Some(room), false).await;
    for w in [&mut wa, &mut wb, &mut wc] {
        w.settle().await;
    }

    // Camera on: everybody hears it, and a late joiner's snapshot carries it.
    wa.voice_cam(Some(room), true).await;
    for w in [&mut wb, &mut wc] {
        assert!(member_of(&w.settle().await, a).is_some_and(|m| m.camera), "camera on is broadcast");
    }
    wa.settle().await;
    assert!(snapshot_voice(&env, "Dani").await.iter().any(|m| m.user.id == a && m.camera));
    assert!(snapshot_voice(&env, "Edu").await.iter().all(|m| m.user.id != b || !m.camera));

    // A mute flip from a client that says camera=false turns it off (the camera follows what the
    // client last said); an old client that never sends it is covered in the proto tests.
    wa.voice_cam(Some(room), false).await;
    assert!(member_of(&wb.settle().await, a).is_some_and(|m| !m.camera), "camera off is broadcast");
    assert!(snapshot_voice(&env, "Fran").await.iter().all(|m| !m.camera));

    // On again, then leaving voice clears it.
    wa.voice_cam(Some(room), true).await;
    wb.settle().await;
    wa.voice_cam(None, true).await; // even a (buggy) camera=true with no channel must not stick
    let m = member_of(&wb.settle().await, a).expect("leave is announced");
    assert!(m.channel_id.is_none() && !m.camera, "{m:?}");
    assert!(snapshot_voice(&env, "Gabi").await.iter().all(|m| !m.camera));

    // On again, then going offline clears it.
    wa.voice_cam(Some(room), true).await;
    wb.settle().await;
    drop(wa);
    let ev = wb.settle().await;
    let m = member_of(&ev, a).expect("voice cleared on disconnect");
    assert!(m.channel_id.is_none() && !m.camera, "{m:?}");
    assert!(ev.iter().any(|e| matches!(e, ServerEvent::UserOffline { user_id } if *user_id == a)));
    assert!(snapshot_voice(&env, "Hugo").await.iter().all(|m| !m.camera));
    env.server.shutdown().await;
}

#[tokio::test]
async fn audio_only_flag_is_broadcast_to_everybody_and_cleared() {
    let env = setup().await;
    let (ana, beto, cris) = (env.login("Ana").await, env.login("Beto").await, env.login("Cris").await);
    let (mut wa, mut wb, mut wc) = (env.ws(&ana.token).await, env.ws(&beto.token).await, env.ws(&cris.token).await);
    let ServerEvent::Ready { channels, .. } = wa.expect().await else { panic!("expected Ready") };
    let room = channel_id(&channels, "Sala 1");
    let a = ana.user.id;
    // Beto is in the room, Cris is not in any voice channel.
    wb.voice_live(Some(room), false, false).await;
    wa.voice_live(Some(room), false, false).await;
    for w in [&mut wa, &mut wb, &mut wc] {
        w.settle().await;
    }

    // Going live as audio-only: the room and the people outside it all hear it, and a late
    // joiner's snapshot carries it.
    wa.voice_live(Some(room), true, true).await;
    for w in [&mut wb, &mut wc] {
        let m = member_of(&w.settle().await, a).expect("the live is announced");
        assert!(m.streaming && m.audio_only, "{m:?}");
    }
    wa.settle().await;
    assert!(snapshot_voice(&env, "Dani").await.iter().any(|m| m.user.id == a && m.streaming && m.audio_only));

    // Switching to a screen share (the same flip a "Trocar" between kinds makes) clears it, and back.
    wa.voice_live(Some(room), true, false).await;
    for w in [&mut wb, &mut wc] {
        let m = member_of(&w.settle().await, a).expect("the switch is announced");
        assert!(m.streaming && !m.audio_only, "{m:?}");
    }
    wa.voice_live(Some(room), true, true).await;
    assert!(member_of(&wc.settle().await, a).is_some_and(|m| m.streaming && m.audio_only));
    wb.settle().await;

    // An old client never sends the flag: its live is a screen one (flag off).
    wa.send_raw(r#"{"type":"voice_state","channel_id":SALA,"muted":false,"deafened":false,"streaming":true}"#.replace("SALA", &room.to_string())).await;
    assert!(member_of(&wc.settle().await, a).is_some_and(|m| m.streaming && !m.audio_only));
    wa.voice_live(Some(room), true, true).await;
    wc.settle().await;
    wb.settle().await;

    // Stopping the live clears it, even if the client keeps saying audio_only=true.
    wa.voice_live(Some(room), false, true).await;
    for w in [&mut wb, &mut wc] {
        let m = member_of(&w.settle().await, a).expect("the end is announced");
        assert!(!m.streaming && !m.audio_only, "{m:?}");
    }
    assert!(snapshot_voice(&env, "Edu").await.iter().all(|m| !m.audio_only));

    // Leaving voice clears it.
    wa.voice_live(Some(room), true, true).await;
    wb.settle().await;
    wa.voice_live(None, true, true).await; // a (buggy) streaming + audio_only with no channel must not stick
    let m = member_of(&wb.settle().await, a).expect("leave is announced");
    assert!(m.channel_id.is_none() && !m.streaming && !m.audio_only, "{m:?}");
    assert!(snapshot_voice(&env, "Fran").await.iter().all(|m| !m.audio_only));

    // Going offline clears it.
    wa.voice_live(Some(room), true, true).await;
    wb.settle().await;
    drop(wa);
    let ev = wb.settle().await;
    let m = member_of(&ev, a).expect("voice cleared on disconnect");
    assert!(m.channel_id.is_none() && !m.streaming && !m.audio_only, "{m:?}");
    assert!(ev.iter().any(|e| matches!(e, ServerEvent::UserOffline { user_id } if *user_id == a)));
    assert!(snapshot_voice(&env, "Gabi").await.iter().all(|m| !m.audio_only));
    env.server.shutdown().await;
}

#[tokio::test]
async fn watch_state_spam_is_bounded() {
    let env = setup().await;
    let (ana, beto) = (env.login("Ana").await, env.login("Beto").await);
    let (mut wa, mut wb) = (env.ws(&ana.token).await, env.ws(&beto.token).await);
    let ServerEvent::Ready { channels, .. } = wa.expect().await else { panic!("expected Ready") };
    let room = channel_id(&channels, "Sala 1");
    wa.voice(Some(room), false, true).await;
    wb.voice(Some(room), false, false).await;
    wa.settle().await;
    wb.settle().await;
    for i in 0..200 {
        let set: Vec<i64> = if i % 2 == 0 { vec![ana.user.id] } else { vec![] };
        wb.watch(&set).await;
    }
    let changes = wa.settle().await.iter().filter(|e| matches!(e, ServerEvent::VoiceStateChanged { .. })).count();
    assert!((15..=30).contains(&changes), "{changes} announcements for 200 requests");
    env.server.shutdown().await;
}

/// A v0.2.0/v0.2.1 client speaks raw JSON without ever mentioning watching, and must neither
/// notice nor break anything; an unknown event still gets the exact error text new clients
/// recognise as "this hub is too old".
#[tokio::test]
async fn old_clients_and_the_unknown_event_error() {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum OldServerEvent {
        VoiceStateChanged { member: OldMember },
    }
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct OldMember {
        user: User,
        channel_id: Option<i64>,
        muted: bool,
        deafened: bool,
        streaming: bool,
    }

    let env = setup().await;
    let (ana, beto) = (env.login("Ana").await, env.login("Beto").await);
    let (mut old, mut wb) = (env.ws(&ana.token).await, env.ws(&beto.token).await);
    let ServerEvent::Ready { channels, .. } = old.expect().await else { panic!("expected Ready") };
    let room = channel_id(&channels, "Sala 1");
    wb.settle().await;
    old.settle().await;

    // The old client announces itself live with hand-written v0.2.0 JSON.
    let raw = format!(r#"{{"type":"voice_state","channel_id":{room},"muted":false,"deafened":false,"streaming":true}}"#);
    old.0.send(WsMsg::Text(raw.into())).await.unwrap();
    wb.settle().await;
    old.settle().await;
    // Nobody watches: the JSON it gets back carries no new field at all.
    wb.voice(Some(room), false, false).await;
    let Some(Ok(WsMsg::Text(t))) = tokio::time::timeout(Duration::from_secs(5), old.0.next()).await.unwrap() else { panic!() };
    assert!(!t.contains("watching"), "{t}");
    assert!(serde_json::from_str::<OldServerEvent>(&t).is_ok());

    // Beto watches; the old client gets the extra field and still parses the event as its own type.
    wb.watch(&[ana.user.id]).await;
    let Some(Ok(WsMsg::Text(t))) = tokio::time::timeout(Duration::from_secs(5), old.0.next()).await.unwrap() else { panic!() };
    assert!(t.contains(r#""watching":["#), "{t}");
    assert!(serde_json::from_str::<OldServerEvent>(&t).is_ok());

    // An event the hub does not know (what an old hub says to WatchState) is an Error, not a close.
    old.0.send(WsMsg::Text(r#"{"type":"from_the_future","x":1}"#.into())).await.unwrap();
    assert!(matches!(old.expect().await, ServerEvent::Error { error } if error == WATCH_STATE_REJECTED));
    old.send(&ClientEvent::Ping { nonce: 1 }).await;
    assert!(matches!(old.expect().await, ServerEvent::Pong { nonce: 1 }));
    env.server.shutdown().await;
}
