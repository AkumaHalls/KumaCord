//! Private channels (v0.9): a real hub on an ephemeral port, LiveKit faked by a small copy of its
//! server API (as in `voice_guard.rs`). Somebody without access must get nothing of a private
//! channel: no list entry, no event (messages, reactions, deletions, voice members, sounds), no
//! history, image, reaction or voice token (all `404` like an unknown id), no mention. Access by
//! role and by name, Dono and Admin always, live grants and revokes (taken out of the LiveKit room
//! at once), and an older app's view.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode as AxStatus;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use image::{Rgb, RgbImage};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start_tuned, Options, Server, Tuning};

const PASSWORD: &str = "hunter2";

// ---------------------------------------------------------------- the fake LiveKit

#[derive(Default)]
struct Fake {
    /// room -> identities
    rooms: BTreeMap<String, Vec<(String, i64)>>,
    /// (method, body) of every call.
    calls: Vec<(String, Value)>,
}

type FakeRef = Arc<Mutex<Fake>>;

fn not_found() -> Response {
    (AxStatus::NOT_FOUND, Json(json!({ "code": "not_found", "msg": "not found" }))).into_response()
}

async fn twirp(State(f): State<FakeRef>, Path(method): Path<String>, body: String) -> Response {
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let mut g = f.lock().unwrap();
    g.calls.push((method.clone(), v.clone()));
    let room = v["room"].as_str().unwrap_or_default().to_string();
    let identity = v["identity"].as_str().unwrap_or_default().to_string();
    let part = |(id, joined): &(String, i64)| json!({ "identity": id, "joinedAt": joined.to_string(), "permission": { "canPublish": true }, "tracks": [] });
    match method.as_str() {
        "ListRooms" => Json(json!({ "rooms": g.rooms.keys().map(|n| json!({ "name": n })).collect::<Vec<_>>() })).into_response(),
        "ListParticipants" => match g.rooms.get(&room) {
            Some(list) => Json(json!({ "participants": list.iter().map(part).collect::<Vec<_>>() })).into_response(),
            None => not_found(),
        },
        "GetParticipant" => match g.rooms.get(&room).and_then(|l| l.iter().find(|p| p.0 == identity)) {
            Some(p) => Json(part(p)).into_response(),
            None => not_found(),
        },
        "RemoveParticipant" => {
            let Some(list) = g.rooms.get_mut(&room) else { return not_found() };
            let before = list.len();
            list.retain(|p| p.0 != identity);
            if list.len() == before {
                return not_found();
            }
            if list.is_empty() {
                g.rooms.remove(&room);
            }
            Json(json!({})).into_response()
        }
        _ => Json(json!({})).into_response(),
    }
}

impl Fake {
    fn join(f: &FakeRef, room: &str, user: UserId) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        f.lock().unwrap().rooms.entry(room.to_string()).or_default().push((user.to_string(), now));
    }

    fn present(f: &FakeRef, room: &str, user: UserId) -> bool {
        let id = user.to_string();
        f.lock().unwrap().rooms.get(room).is_some_and(|l| l.iter().any(|p| p.0 == id))
    }

    fn removals(f: &FakeRef, room: &str, user: UserId) -> usize {
        let id = user.to_string();
        f.lock().unwrap().calls.iter().filter(|(m, v)| m == "RemoveParticipant" && v["room"] == room && v["identity"] == id.as_str()).count()
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

/// `check_every`: the periodic LiveKit check (a long one proves a removal came at once; the
/// closer watch after a removal is off here, it checks at the same pace).
async fn setup(check_every: Duration) -> Env {
    setup_with(check_every, check_every).await
}

/// [`setup`] with the closer check that follows a removal (v1.0.1) every `fast`.
async fn setup_with(check_every: Duration, fast: Duration) -> Env {
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
    let tuning = Tuning { livekit_api: true, voice_check_every: check_every, voice_grace: Duration::from_secs(60), voice_check_fast: fast, ..Tuning::default() };
    let server = start_tuned(opts, Default::default(), tuning).await.unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { _server: server, base, http: reqwest::Client::new(), fake, _dir: dir }
}

fn room(ch: ChannelId) -> String {
    format!("voice-{ch}")
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn login(&self, name: &str) -> Who {
        let req = LoginRequest { username: name.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        let r = self.http.post(self.url("/api/login")).json(&req).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "login {name}");
        let l: LoginResponse = r.json().await.unwrap();
        Who { token: l.token, user: l.user }
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn get(&self, who: &Who, path: &str) -> reqwest::Response {
        self.http.get(self.url(path)).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn ok<T: serde::de::DeserializeOwned>(&self, who: &Who, path: &str, body: Value) -> T {
        let r = self.post(who, path, body).await;
        let status = r.status();
        let text = r.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{path}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn set_password(&self, who: &Who, pw: &str) {
        assert_eq!(self.post(who, "/api/account/password", json!({ "new": pw })).await.status(), StatusCode::OK);
    }

    async fn set_role(&self, by: &Who, who: &Who, role: Role) {
        let _: OkResponse = self.ok(by, &format!("/api/admin/users/{}/role", who.user.id), json!({ "role": role })).await;
    }

    /// The owner, an admin and a moderator (with personal passwords).
    async fn staff(&self) -> (Who, Who, Who) {
        let owner = self.login("Dono").await;
        self.set_password(&owner, "dono-senha").await;
        let admin = self.login("adm").await;
        self.set_password(&admin, "adm-senha").await;
        let moder = self.login("modi").await;
        self.set_password(&moder, "mod-senha").await;
        self.set_role(&owner, &admin, Role::Admin).await;
        self.set_role(&owner, &moder, Role::Moderator).await;
        (owner, admin, moder)
    }

    async fn channels(&self, who: &Who) -> Vec<Channel> {
        let r = self.get(who, "/api/channels").await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn sees(&self, who: &Who, ch: ChannelId) -> bool {
        self.channels(who).await.iter().any(|c| c.id == ch)
    }

    /// (first text channel, first voice channel), public ones.
    async fn public_ids(&self, who: &Who) -> (ChannelId, ChannelId) {
        let chs = self.channels(who).await;
        (chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id, chs.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id)
    }

    async fn create(&self, owner: &Who, name: &str, kind: ChannelKind, private: Option<PrivateAccess>) -> Channel {
        self.ok(owner, "/api/admin/channels", json!(ChannelCreateRequest { name: name.into(), kind, private })).await
    }

    async fn set_access(&self, owner: &Who, ch: ChannelId, private: Option<PrivateAccess>) {
        let _: OkResponse = self.ok(owner, &format!("/api/admin/channels/{ch}/access"), json!(ChannelAccessRequest { private })).await;
    }

    async fn say(&self, who: &Who, ch: ChannelId, text: &str, mentions: &[UserId], reply_to: Option<MessageId>) -> Message {
        self.ok(who, &format!("/api/channels/{ch}/messages"), json!({ "content": text, "mentions": mentions, "reply_to": reply_to, "reply_mention": reply_to.is_some() })).await
    }

    async fn photo(&self, who: &Who, ch: ChannelId) -> Message {
        let img = RgbImage::from_fn(320, 240, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 77]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85).encode_image(&img).unwrap();
        let r = self.http.post(self.url("/api/attachments")).bearer_auth(&who.token).body(bytes).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let a: Attachment = r.json().await.unwrap();
        self.ok(who, &format!("/api/channels/{ch}/messages"), json!({ "content": "foto secreta", "attachments": [a.id] })).await
    }

    async fn sound(&self, who: &Who) -> Sound {
        let frames = SOUND_RATE as usize * 300 / 1000;
        let samples: Vec<i16> = (0..frames).map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / SOUND_RATE as f32).sin() * 8000.0) as i16).collect();
        let mut url = url::Url::parse(&self.url("/api/sounds")).unwrap();
        url.query_pairs_mut().append_pair("name", "fanfarra").append_pair("emoji", "🎺");
        let r = self.http.post(url).header("content-type", "audio/wav").bearer_auth(&who.token).body(canonical_wav(1, &samples)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    /// Ask for a voice token; the status.
    async fn voice_token(&self, who: &Who, ch: ChannelId) -> StatusCode {
        self.post(who, &format!("/api/voice/{ch}/join"), json!({})).await.status()
    }

    /// Join a voice room as the app does: token, LiveKit, `VoiceState`.
    async fn enter_voice(&self, who: &Who, ws: &mut Ws, ch: ChannelId) {
        assert_eq!(self.voice_token(who, ch).await, StatusCode::OK);
        Fake::join(&self.fake, &room(ch), who.user.id);
        ws.voice(Some(ch)).await;
        ws.wait_for("my own voice state", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == who.user.id && member.channel_id == Some(ch))).await;
    }

    async fn ws(&self, who: &Who) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        let mut ws = Ws { inner: ws, frames: Vec::new() };
        ws.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await;
        ws
    }

    async fn audit(&self, who: &Who) -> Vec<AuditEntry> {
        self.get(who, "/api/admin/audit").await.json().await.unwrap()
    }
}

struct Ws {
    inner: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    /// Every text frame this connection got, as it came.
    frames: Vec<String>,
}

impl Ws {
    async fn next_within(&mut self, wait: Duration) -> Option<ServerEvent> {
        loop {
            match tokio::time::timeout(wait, self.inner.next()).await.ok()?? {
                Ok(WsMsg::Text(t)) => {
                    self.frames.push(t.to_string());
                    return Some(serde_json::from_str(&t).unwrap());
                }
                Ok(WsMsg::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&ServerEvent) -> bool) -> ServerEvent {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let left = until.checked_duration_since(Instant::now()).unwrap_or_else(|| panic!("no {what} within 5 s"));
            match self.next_within(left).await {
                Some(e) if pred(&e) => return e,
                Some(_) => {}
                None => panic!("no {what}: the socket ended or timed out"),
            }
        }
    }

    async fn send(&mut self, ev: &ClientEvent) {
        self.inner.send(WsMsg::Text(serde_json::to_string(ev).unwrap().into())).await.unwrap();
    }

    async fn voice(&mut self, ch: Option<ChannelId>) {
        self.send(&ClientEvent::VoiceState { channel_id: ch, muted: false, deafened: false, streaming: false, camera: false, audio_only: false }).await;
    }

    /// Wait for `text` said in a public channel: everything sent before it has been delivered.
    async fn barrier(&mut self, text: &str) {
        self.wait_for(text, |e| matches!(e, ServerEvent::MessageCreated { message } if message.content == text)).await;
    }

    /// The frames that say anything about these channels (or contain any of `words`).
    fn leaks(&self, channels: &[ChannelId], words: &[&str]) -> Vec<String> {
        self.frames.iter().filter(|f| frame_mentions(f, channels) || words.iter().any(|w| f.contains(w))).cloned().collect()
    }
}

/// Does this frame say anything about one of `channels`?
fn frame_mentions(frame: &str, channels: &[ChannelId]) -> bool {
    let has = |c: &ChannelId| channels.contains(c);
    match serde_json::from_str::<ServerEvent>(frame).unwrap() {
        ServerEvent::Ready { channels: list, voice, .. } => list.iter().any(|c| has(&c.id)) || voice.iter().any(|m| m.channel_id.as_ref().is_some_and(has)),
        ServerEvent::ChannelsChanged { channels: list } => list.iter().any(|c| has(&c.id)),
        ServerEvent::MessageCreated { message } => has(&message.channel_id),
        ServerEvent::MessageDeleted { channel_id, .. } | ServerEvent::ReactionChanged { channel_id, .. } | ServerEvent::SoundPlayed { channel_id, .. } => has(&channel_id),
        ServerEvent::VoiceStateChanged { member } => member.channel_id.as_ref().is_some_and(has),
        ServerEvent::Moderated { action: ModAction::Move { channel_id }, .. } => has(&channel_id),
        _ => false,
    }
}

fn is_channels_with(ch: ChannelId, yes: bool) -> impl Fn(&ServerEvent) -> bool {
    move |e| matches!(e, ServerEvent::ChannelsChanged { channels } if channels.iter().any(|c| c.id == ch) == yes)
}

async fn eventually(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("not within {secs} s: {what}");
}

fn people(users: &[&Who]) -> Option<PrivateAccess> {
    Some(PrivateAccess { roles: vec![], users: users.iter().map(|w| w.user.id).collect() })
}

// ---------------------------------------------------------------- tests

/// A Membro without access gets nothing of a private text and voice channel: not the channels,
/// not one event (messages, @todos, reactions, deletions, images, voice members, sounds), and every
/// read or write answers like an unknown id. The members with access get all of it.
#[tokio::test]
async fn a_member_without_access_gets_nothing_of_a_private_channel() {
    let env = setup(Duration::from_secs(60)).await;
    let (owner, admin, moder) = env.staff().await;
    let (ana, bia) = (env.login("ana").await, env.login("bia").await);
    let (geral, _) = env.public_ids(&bia).await;
    let fanfarra = env.sound(&owner).await;
    let mut wb = env.ws(&bia).await;
    let mut wa = env.ws(&ana).await;
    let mut wo = env.ws(&owner).await;
    let mut wm = env.ws(&moder).await;

    // A private text channel and a private voice channel for Ana (by name).
    let t = env.create(&owner, "cofre", ChannelKind::Text, people(&[&ana])).await;
    assert_eq!(t.private, people(&[&ana]), "created private right away");
    let v = env.create(&owner, "cofre-voz", ChannelKind::Voice, people(&[&ana])).await;
    let ev = wa.wait_for("the private channels", is_channels_with(v.id, true)).await;
    let ServerEvent::ChannelsChanged { channels } = ev else { unreachable!() };
    assert!(channels.iter().any(|c| c.id == t.id && c.private.is_some()), "with the lock information");
    for (who, sees) in [(&owner, true), (&admin, true), (&ana, true), (&moder, false), (&bia, false)] {
        assert_eq!(env.sees(who, t.id).await, sees, "{}", who.user.username);
        assert_eq!(env.sees(who, v.id).await, sees, "{}", who.user.username);
    }

    // Ana chats: a mention of Bia (no access) is dropped, the owner's kept; an image; reactions;
    // a reply; the owner's @todos; a deleted message.
    let m1 = env.say(&ana, t.id, "segredo um @bia @Dono", &[bia.user.id, owner.user.id], None).await;
    assert_eq!(m1.mentions, vec![owner.user.id], "only people with access are mentioned");
    let pic = env.photo(&ana, t.id).await;
    let att = pic.attachments[0].id;
    let _: OkResponse = env.ok(&owner, &format!("/api/messages/{}/reactions", m1.id), json!({ "emoji": "👍", "add": true })).await;
    let m2 = env.say(&owner, t.id, "@todos segredo dois", &[], Some(m1.id)).await;
    assert!(m2.mention_everyone);
    wa.send(&ClientEvent::SendMessage { channel_id: t.id, content: "segredo tres".into(), mentions: vec![bia.user.id], reply_to: None, reply_mention: false }).await;
    let ServerEvent::MessageCreated { message: m3 } = wa.wait_for("segredo tres", |e| matches!(e, ServerEvent::MessageCreated { message } if message.content == "segredo tres")).await else { unreachable!() };
    assert!(m3.mentions.is_empty(), "the WebSocket path drops her too");
    let _: OkResponse = env.ok(&ana, &format!("/api/messages/{}/delete", m3.id), json!({})).await;

    // Voice: Ana and the owner in the private room, the owner plays a sound.
    env.enter_voice(&ana, &mut wa, v.id).await;
    env.enter_voice(&owner, &mut wo, v.id).await;
    wo.send(&ClientEvent::PlaySound { sound_id: fanfarra.id }).await;
    wa.wait_for("the sound", |e| matches!(e, ServerEvent::SoundPlayed { channel_id, .. } if *channel_id == v.id)).await;

    // Bia: every way in is an unknown id.
    let nf = StatusCode::NOT_FOUND;
    assert_eq!(env.get(&bia, &format!("/api/channels/{}/messages", t.id)).await.status(), nf);
    assert_eq!(env.get(&bia, &format!("/api/channels/{}/messages", v.id)).await.status(), nf);
    assert_eq!(env.post(&bia, &format!("/api/channels/{}/messages", t.id), json!({ "content": "oi" })).await.status(), nf);
    assert_eq!(env.get(&bia, &format!("/api/attachments/{att}")).await.status(), nf);
    assert_eq!(env.get(&bia, &format!("/api/attachments/{att}/thumb")).await.status(), nf);
    assert_eq!(env.post(&bia, &format!("/api/messages/{}/reactions", m1.id), json!({ "emoji": "👍", "add": true })).await.status(), nf);
    assert_eq!(env.post(&bia, &format!("/api/messages/{}/delete", m1.id), json!({})).await.status(), nf);
    assert_eq!(env.voice_token(&bia, v.id).await, nf, "no LiveKit token");
    wb.send(&ClientEvent::SendMessage { channel_id: t.id, content: "invasao".into(), mentions: vec![], reply_to: None, reply_mention: false }).await;
    wb.wait_for("refused", |e| matches!(e, ServerEvent::Error { error } if error == "unknown text channel")).await;
    wb.voice(Some(v.id)).await;
    wb.wait_for("refused", |e| matches!(e, ServerEvent::Error { error } if error == "unknown voice channel")).await;
    // A reply from a public channel cannot quote the private message.
    let quoted = env.say(&bia, geral, "respondendo", &[], Some(m1.id)).await;
    assert!(quoted.reply.is_none(), "a message of another channel is not quoted: {:?}", quoted.reply);
    // The moderator without access cannot reach into it either.
    assert_eq!(env.post(&moder, &format!("/api/messages/{}/delete", m1.id), json!({})).await.status(), nf);
    assert_eq!(env.post(&moder, &format!("/api/mod/users/{}/move", bia.user.id), json!({ "channel_id": v.id })).await.status(), nf);
    assert_eq!(env.post(&moder, &format!("/api/mod/users/{}/disconnect", ana.user.id), json!({})).await.status(), StatusCode::CONFLICT, "Ana is not in voice as far as the moderator can see");
    assert!(env.get(&moder, &format!("/api/attachments/{att}")).await.status() == nf);

    // Everything before this message has reached Bia and the moderator: none of it was private.
    let _ = env.say(&owner, geral, "barreira 1", &[], None).await;
    wb.barrier("barreira 1").await;
    wm.barrier("barreira 1").await;
    let words = ["segredo", "cofre", "foto secreta"];
    assert_eq!(wb.leaks(&[t.id, v.id], &words), Vec::<String>::new(), "Bia got something of the private channels");
    assert_eq!(wm.leaks(&[t.id, v.id], &words), Vec::<String>::new(), "the moderator got something of the private channels");
    assert!(!wb.frames.iter().any(|f| f.contains("channels_changed")), "creating channels Bia cannot see tells her nothing");
    assert!(!wb.frames.iter().any(|f| f.contains(&format!("\"id\":{},\"username\":\"ana\"", ana.user.id)) && f.contains("voice_state_changed")), "Ana in the private room is not in voice for Bia");

    // The people with access got all of it.
    for (who, ws) in [("ana", &mut wa), ("owner", &mut wo)] {
        ws.barrier("barreira 1").await;
        let f = ws.frames.join("\n");
        for w in ["segredo um", "segredo dois", "foto secreta", "reaction_changed", "message_deleted", "sound_played"] {
            assert!(f.contains(w), "{who} missed {w}");
        }
    }
    // The history, for someone with access: mentions and the reply as stored.
    let hist: Vec<Message> = env.get(&admin, &format!("/api/channels/{}/messages", t.id)).await.json().await.unwrap();
    let r = hist.iter().find(|m| m.id == m2.id).unwrap();
    assert_eq!(r.reply.as_ref().map(|q| (q.id, q.deleted)), Some((m1.id, false)));
    assert_eq!(env.get(&admin, &format!("/api/attachments/{att}")).await.status(), StatusCode::OK);
}

/// Dono and Admin always; roles and names grant access, each on its own; a role change and a
/// list change apply live; public again shows it to everybody. All in the moderation log.
#[tokio::test]
async fn roles_and_names_grant_access_live() {
    let env = setup(Duration::from_secs(60)).await;
    let (owner, admin, moder) = env.staff().await;
    let (ana, bia) = (env.login("ana").await, env.login("bia").await);
    env.set_password(&ana, "ana-senha").await;
    let mut wm = env.ws(&moder).await;
    let mut wa = env.ws(&ana).await;
    let mut wb = env.ws(&bia).await;

    // Nobody named, no role: only Dono and Admin.
    let c = env.create(&owner, "staff", ChannelKind::Text, Some(PrivateAccess::default())).await;
    for (who, sees) in [(&owner, true), (&admin, true), (&moder, false), (&ana, false), (&bia, false)] {
        assert_eq!(env.sees(who, c.id).await, sees, "{}", who.user.username);
    }
    // The admin manages it like anybody else's channel.
    env.set_access(&admin, c.id, Some(PrivateAccess { roles: vec![Role::Moderator], users: vec![] })).await;
    wm.wait_for("granted by role", is_channels_with(c.id, true)).await;
    assert_eq!(env.get(&moder, &format!("/api/channels/{}/messages", c.id)).await.status(), StatusCode::OK, "history loads at once");
    assert!(!env.sees(&ana, c.id).await, "Moderador does not include Membro");

    // Membro instead of Moderador: the members get it, the moderator loses it, live.
    env.set_access(&owner, c.id, Some(PrivateAccess { roles: vec![Role::Member], users: vec![] })).await;
    wa.wait_for("granted to Membro", is_channels_with(c.id, true)).await;
    wb.wait_for("granted to Membro", is_channels_with(c.id, true)).await;
    wm.wait_for("revoked from Moderador", is_channels_with(c.id, false)).await;
    assert_eq!(env.get(&moder, &format!("/api/channels/{}/messages", c.id)).await.status(), StatusCode::NOT_FOUND);

    // A role change re-evaluates: Ana promoted to Moderador loses it, back to Membro gets it again.
    env.set_role(&owner, &ana, Role::Moderator).await;
    wa.wait_for("lost with the promotion", is_channels_with(c.id, false)).await;
    env.set_role(&owner, &ana, Role::Member).await;
    wa.wait_for("back as Membro", is_channels_with(c.id, true)).await;

    // By name only: Bia, whatever her role.
    env.set_access(&owner, c.id, people(&[&bia])).await;
    wa.wait_for("not named", is_channels_with(c.id, false)).await;
    assert!(env.sees(&bia, c.id).await && !env.sees(&ana, c.id).await);
    // The same list again changes nothing (no event, no log line).
    let log_before = env.audit(&owner).await.len();
    env.set_access(&owner, c.id, people(&[&bia])).await;
    assert_eq!(env.audit(&owner).await.len(), log_before);

    // Public again: everybody.
    env.set_access(&owner, c.id, None).await;
    let ServerEvent::ChannelsChanged { channels } = wm.wait_for("public again", is_channels_with(c.id, true)).await else { unreachable!() };
    assert!(channels.iter().find(|x| x.id == c.id).unwrap().private.is_none());
    assert!(env.sees(&ana, c.id).await);

    // Members and moderators cannot change any of it.
    for who in [&moder, &ana] {
        assert_eq!(env.post(who, &format!("/api/admin/channels/{}/access", c.id), json!({ "private": {} })).await.status(), StatusCode::FORBIDDEN);
    }
    let log = env.audit(&owner).await;
    let access: Vec<&AuditEntry> = log.iter().filter(|e| e.action == AUDIT_CHANNEL_ACCESS).collect();
    assert_eq!(access.len(), 4, "{access:?}");
    assert!(access[0].detail.ends_with("staff: público"), "{}", access[0].detail);
    assert!(access[1].detail.contains("pessoas: bia"), "{}", access[1].detail);
    assert!(log.iter().any(|e| e.action == AUDIT_CHANNEL_CREATE && e.detail.contains("privado")));
    // Admins and the owner always pass, even with a list that names nobody.
    assert!(PrivateAccess::default().allows(admin.user.id, Role::Admin));
}

/// Losing access takes the person out of the LiveKit room at once (not at the next periodic
/// check) and out of the voice list; getting it back shows the channel and who is in it. Making
/// a public room private takes out whoever may not stay.
#[tokio::test]
async fn losing_access_takes_people_out_of_the_voice_room_at_once() {
    let env = setup(Duration::from_secs(3600)).await; // the periodic check never runs here
    let owner = env.login("Dono").await;
    env.set_password(&owner, "dono-senha").await;
    let (ana, bia, cid) = (env.login("ana").await, env.login("bia").await, env.login("cid").await);
    let (_, sala) = env.public_ids(&owner).await;
    let mut wo = env.ws(&owner).await;
    let mut wa = env.ws(&ana).await;
    let mut wb = env.ws(&bia).await;
    let mut wc = env.ws(&cid).await;

    let v = env.create(&owner, "reuniao", ChannelKind::Voice, people(&[&ana, &bia])).await;
    wb.wait_for("granted", is_channels_with(v.id, true)).await;
    env.enter_voice(&ana, &mut wa, v.id).await;
    env.enter_voice(&bia, &mut wb, v.id).await;
    wo.wait_for("Bia in the room", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == bia.user.id && member.channel_id == Some(v.id))).await;

    // Revoke Bia: out of LiveKit at once, out of the list, the channel gone for her.
    let t0 = Instant::now();
    env.set_access(&owner, v.id, people(&[&ana])).await;
    eventually("Bia removed from the LiveKit room", 2, || !Fake::present(&env.fake, &room(v.id), bia.user.id)).await;
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert_eq!(Fake::removals(&env.fake, &room(v.id), bia.user.id), 1);
    assert!(Fake::present(&env.fake, &room(v.id), ana.user.id), "Ana stays");
    wb.wait_for("the channel gone", is_channels_with(v.id, false)).await;
    wb.wait_for("out of voice", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == bia.user.id && member.channel_id.is_none())).await;
    wo.wait_for("Bia left", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == bia.user.id && member.channel_id.is_none())).await;
    assert_eq!(env.voice_token(&bia, v.id).await, StatusCode::NOT_FOUND);

    // Back in the list: the channel and who is in it.
    env.set_access(&owner, v.id, people(&[&ana, &bia])).await;
    wb.wait_for("granted again", is_channels_with(v.id, true)).await;
    wb.wait_for("Ana in the room", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == ana.user.id && member.channel_id == Some(v.id))).await;

    // A public room made private: whoever is not on the list is taken out.
    env.enter_voice(&cid, &mut wc, sala).await;
    env.set_access(&owner, sala, people(&[&ana])).await;
    eventually("Cid removed from the room", 2, || !Fake::present(&env.fake, &room(sala), cid.user.id)).await;
    // (The list first: the hub tells everybody their new view before it takes people out.)
    wc.wait_for("the room gone", is_channels_with(sala, false)).await;
    wc.wait_for("out of voice", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == cid.user.id && member.channel_id.is_none())).await;
    // Bia (not on that list either, in another room) sees nothing of it any more.
    wb.wait_for("the room gone", is_channels_with(sala, false)).await;
    // Public again: everybody sees it.
    env.set_access(&owner, sala, None).await;
    wc.wait_for("public again", is_channels_with(sala, true)).await;
}

/// A token from before losing access does not get back in: the periodic check removes the
/// session without the grace. A role change does the same as a list change.
#[tokio::test]
async fn a_token_from_before_does_not_get_back_in() {
    let env = setup(Duration::from_millis(150)).await;
    let owner = env.login("Dono").await;
    env.set_password(&owner, "dono-senha").await;
    let ana = env.login("ana").await;
    env.set_password(&ana, "ana-senha").await;
    let mut wa = env.ws(&ana).await;
    let v = env.create(&owner, "membros", ChannelKind::Voice, Some(PrivateAccess { roles: vec![Role::Member], users: vec![] })).await;
    wa.wait_for("granted", is_channels_with(v.id, true)).await;
    env.enter_voice(&ana, &mut wa, v.id).await;

    // Promoted to Moderador: the list is for Membro only.
    env.set_role(&owner, &ana, Role::Moderator).await;
    eventually("Ana removed", 2, || !Fake::present(&env.fake, &room(v.id), ana.user.id)).await;
    wa.wait_for("the channel gone", is_channels_with(v.id, false)).await;
    let removed = Fake::removals(&env.fake, &room(v.id), ana.user.id);
    // Back into LiveKit with the old token: out again at the next check.
    Fake::join(&env.fake, &room(v.id), ana.user.id);
    eventually("the old token's session removed", 3, || Fake::removals(&env.fake, &room(v.id), ana.user.id) > removed).await;
    assert!(!Fake::present(&env.fake, &room(v.id), ana.user.id));
}

/// v1.0.1 (audit `voxhub:hub:private-voice-rejoin-window-after-access-loss`): a join token is good
/// for a minute; after losing access no new one is given, and while an old one could still be
/// used the hub checks the rooms every `voice_check_fast`, so each rejoin with it is cut short
/// (here the regular check never runs: every removal comes from the closer watch).
#[tokio::test]
async fn old_tokens_keep_being_cut_short_after_losing_access() {
    let env = setup_with(Duration::from_secs(600), Duration::from_millis(100)).await;
    let owner = env.login("Dono").await;
    env.set_password(&owner, "dono-senha").await;
    let ana = env.login("ana").await;
    env.set_password(&ana, "ana-senha").await;
    let mut wa = env.ws(&ana).await;
    let v = env.create(&owner, "membros", ChannelKind::Voice, Some(PrivateAccess { roles: vec![Role::Member], users: vec![] })).await;
    wa.wait_for("granted", is_channels_with(v.id, true)).await;

    // The token the hub gives expires within a minute (plus LiveKit's leeway, nothing more).
    let join: VoiceJoinResponse = env.post(&ana, &format!("/api/voice/{}/join", v.id), json!({})).await.json().await.unwrap();
    let claims = join.livekit_token.split('.').nth(1).unwrap();
    use base64::Engine as _;
    let claims: Value = serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let left = claims["exp"].as_i64().unwrap() - now;
    assert!((50..=61).contains(&left), "the token is good for {left} s more");
    env.enter_voice(&ana, &mut wa, v.id).await;

    env.set_role(&owner, &ana, Role::Moderator).await;
    eventually("Ana removed", 2, || !Fake::present(&env.fake, &room(v.id), ana.user.id)).await;
    wa.wait_for("the channel gone", is_channels_with(v.id, false)).await;
    assert_eq!(env.voice_token(&ana, v.id).await, StatusCode::NOT_FOUND, "no new token without access");
    // Back in with the old token, again and again: each time out within about a second.
    for round in 0..3 {
        let removed = Fake::removals(&env.fake, &room(v.id), ana.user.id);
        Fake::join(&env.fake, &room(v.id), ana.user.id);
        eventually(&format!("rejoin {round} cut short"), 2, || Fake::removals(&env.fake, &room(v.id), ana.user.id) > removed).await;
        assert!(!Fake::present(&env.fake, &room(v.id), ana.user.id));
    }
}

/// Mentions and @todos in a private channel never reach people without access, replies to them
/// included; somebody who lost access is not mentioned any more.
#[tokio::test]
async fn mentions_reach_only_people_with_access() {
    let env = setup(Duration::from_secs(60)).await;
    let owner = env.login("Dono").await;
    env.set_password(&owner, "dono-senha").await;
    let (ana, bia) = (env.login("ana").await, env.login("bia").await);
    let (geral, _) = env.public_ids(&owner).await;
    let t = env.create(&owner, "projeto", ChannelKind::Text, people(&[&ana, &bia])).await;
    let from_bia = env.say(&bia, t.id, "eu ainda estou aqui", &[], None).await;
    env.set_access(&owner, t.id, people(&[&ana])).await;
    let mut wb = env.ws(&bia).await;

    // A reply to Bia's old message with "@ ligado", and her name typed: she is not mentioned.
    let reply = env.say(&ana, t.id, "@bia @Dono segredo", &[bia.user.id, owner.user.id], Some(from_bia.id)).await;
    assert_eq!(reply.mentions, vec![owner.user.id]);
    assert!(reply.reply.is_some(), "the reply itself stays");
    let all = env.say(&owner, t.id, "@todos reuniao secreta", &[], None).await;
    assert!(all.mention_everyone);
    // In the history too (what the apps count unread mentions from).
    let hist: Vec<Message> = env.get(&owner, &format!("/api/channels/{}/messages", t.id)).await.json().await.unwrap();
    assert!(hist.iter().all(|m| !m.mentions.contains(&bia.user.id)));
    let _ = env.say(&owner, geral, "barreira", &[], None).await;
    wb.barrier("barreira").await;
    assert_eq!(wb.leaks(&[t.id], &["segredo", "reuniao secreta"]), Vec::<String>::new());
    // Bia's own old message cannot be reached by id either.
    assert_eq!(env.post(&bia, &format!("/api/messages/{}/delete", from_bia.id), json!({})).await.status(), StatusCode::NOT_FOUND);
}

/// An app that predates private channels (the v0.8 wire shapes, no `private` field known) is
/// filtered by the hub: it never sees a private channel without access, and one with access reads
/// as an ordinary channel.
#[tokio::test]
async fn an_older_app_sees_nothing_private() {
    #[derive(serde::Deserialize, Debug)]
    struct OldChannel {
        id: i64,
        #[allow(dead_code)]
        name: String,
        #[allow(dead_code)]
        kind: ChannelKind,
        #[allow(dead_code)]
        position: i32,
    }
    #[derive(serde::Deserialize, Debug)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum OldEvent {
        Ready { channels: Vec<OldChannel> },
        ChannelsChanged { channels: Vec<OldChannel> },
        #[serde(other)]
        Other,
    }
    let env = setup(Duration::from_secs(60)).await;
    let owner = env.login("Dono").await;
    env.set_password(&owner, "dono-senha").await;
    let (ana, bia) = (env.login("ana").await, env.login("bia").await);
    let t = env.create(&owner, "cofre", ChannelKind::Text, people(&[&ana])).await;
    let _ = env.say(&ana, t.id, "segredo", &[], None).await;

    for (who, sees) in [(&bia, false), (&ana, true)] {
        // The old app's HTTP list.
        let list: Vec<OldChannel> = env.get(who, "/api/channels").await.json().await.unwrap();
        assert_eq!(list.iter().any(|c| c.id == t.id), sees);
        // Its WebSocket: a v0.8 parse of every frame.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", env.base, who.token)).await.unwrap();
        let Some(Ok(WsMsg::Text(first))) = ws.next().await else { panic!("no ready") };
        let Ok(OldEvent::Ready { channels }) = serde_json::from_str::<OldEvent>(&first) else { panic!("an old app reads Ready: {first}") };
        assert_eq!(channels.iter().any(|c| c.id == t.id), sees, "{}", who.user.username);
        if !sees {
            assert!(!first.contains("\"private\""), "nothing private in Bia's snapshot: {first}");
        }
        // A change of the list reaches it in the old shape too.
        env.set_access(&owner, t.id, people(&[&ana, &bia])).await;
        env.set_access(&owner, t.id, people(&[&ana])).await;
        let mut lists = Vec::new();
        let until = Instant::now() + Duration::from_millis(800);
        while let Ok(Some(Ok(WsMsg::Text(f)))) = tokio::time::timeout(until.saturating_duration_since(Instant::now()), ws.next()).await {
            if let Ok(OldEvent::ChannelsChanged { channels }) = serde_json::from_str::<OldEvent>(&f) {
                lists.push(channels.iter().any(|c| c.id == t.id));
            }
            assert!(sees || !f.contains("segredo"), "{f}");
        }
        if sees {
            assert_eq!(lists, vec![true, true], "Ana keeps it while its list changes");
        } else {
            assert_eq!(lists, vec![true, false], "Bia: granted, then revoked");
        }
    }
}
