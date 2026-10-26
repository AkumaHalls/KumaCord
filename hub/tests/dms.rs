//! Direct messages and private 1:1 calls, end to end: a real hub on an ephemeral port (no
//! LiveKit; its calls are best-effort). The core promise is tested from every side: a DM, its
//! history and every event about it reach **only its two people**: a third member, an Admin and
//! the owner get `404`s and no events, and the channel moderation paths cannot reach DMs. Also:
//! unread counts and read pointers, deletes, blocking, size and rate limits, call token scoping
//! (only the two get a `dm-<a>-<b>` token) and the call flows (ring, answer, hang up, decline,
//! cancel, ring out, busy, joining a voice channel, ringing again on connect).

use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "pw-dm";
/// The tests ring for this long instead of 30 s.
const RING: Duration = Duration::from_millis(1500);

struct Env {
    _server: Server,
    base: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

async fn setup() -> Env {
    voxhub_server::calls::set_ring_timeout(RING);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"dm\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n\
             [livekit]\napi_key = \"APIdm000000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) })
        .await
        .unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { _server: server, base, http: reqwest::Client::new(), _dir: dir }
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn who(&self, name: &str) -> Who {
        let req = LoginRequest { username: name.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        let r = self.http.post(self.url("/api/login")).json(&req).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "login {name}");
        let l: LoginResponse = r.json().await.unwrap();
        Who { token: l.token, user: l.user }
    }

    /// The owner and an Admin with their powers on (personal passwords set).
    async fn staff(&self) -> (Who, Who) {
        let owner = self.who("Dono").await;
        assert_eq!(self.post(&owner, "/api/account/password", json!({"new": "dono-senha"})).await.status(), StatusCode::OK);
        let admin = self.who("Adm").await;
        assert_eq!(self.post(&admin, "/api/account/password", json!({"new": "adm-senha"})).await.status(), StatusCode::OK);
        let r = self.post(&owner, &format!("/api/admin/users/{}/role", admin.user.id), json!({"role": "admin"})).await;
        assert_eq!(r.status(), StatusCode::OK);
        // Fresh sessions see the roles (setting a password keeps the session that set it).
        (owner, admin)
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn get(&self, who: &Who, path: &str) -> reqwest::Response {
        self.http.get(self.url(path)).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn open(&self, who: &Who, other: &Who) -> DmConversation {
        let r = self.post(who, &format!("/api/dms/{}", other.user.id), json!({})).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn send(&self, who: &Who, dm: DmId, text: &str) -> reqwest::Response {
        self.post(who, &format!("/api/dms/{dm}/messages"), json!({ "content": text })).await
    }

    async fn send_ok(&self, who: &Who, dm: DmId, text: &str) -> DmMessage {
        let r = self.send(who, dm, text).await;
        assert_eq!(r.status(), StatusCode::OK, "send {text:?}");
        r.json().await.unwrap()
    }

    async fn list(&self, who: &Who) -> Vec<DmConversation> {
        let r = self.get(who, "/api/dms").await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn history(&self, who: &Who, dm: DmId, query: &str) -> Vec<DmMessage> {
        let r = self.get(who, &format!("/api/dms/{dm}/messages{query}")).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn call(&self, who: &Who, dm: DmId, action: &str) -> reqwest::Response {
        let path = if action.is_empty() { format!("/api/dms/{dm}/call") } else { format!("/api/dms/{dm}/call/{action}") };
        self.post(who, &path, json!({})).await
    }

    async fn ws(&self, who: &Who) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        let mut ws = Ws(ws);
        match ws.next_within(Duration::from_secs(5)).await {
            Some(ServerEvent::Ready { .. }) => {}
            other => panic!("expected Ready, got {other:?}"),
        }
        ws
    }
}

async fn error_of(r: reqwest::Response) -> (StatusCode, String) {
    let s = r.status();
    (s, r.json::<ApiError>().await.map(|e| e.error).unwrap_or_default())
}

/// The claims of a LiveKit token (the hub signs them; here we only read them).
fn claims(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

fn is_dm_event(ev: &ServerEvent) -> bool {
    matches!(
        ev,
        ServerEvent::DmMessageCreated { .. }
            | ServerEvent::DmMessageDeleted { .. }
            | ServerEvent::DmRead { .. }
            | ServerEvent::CallInvite { .. }
            | ServerEvent::CallAccepted { .. }
            | ServerEvent::CallDeclined { .. }
            | ServerEvent::CallEnded { .. }
    )
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

    /// Skip events until one matches (`secs` at most).
    async fn wait_for(&mut self, what: &str, secs: u64, mut pred: impl FnMut(&ServerEvent) -> bool) -> ServerEvent {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            match self.next_within(Duration::from_secs(secs)).await {
                Some(ev) if pred(&ev) => return ev,
                Some(_) => {}
                None => break,
            }
        }
        panic!("no {what} event");
    }

    /// Everything that arrives in the next `ms`: none of it may be about a DM or a call.
    async fn hears_nothing_private(&mut self, ms: u64) {
        let t = Instant::now();
        while let Some(left) = Duration::from_millis(ms).checked_sub(t.elapsed()) {
            match self.next_within(left).await {
                Some(ev) => assert!(!is_dm_event(&ev), "a third person got {ev:?}"),
                None => return,
            }
        }
    }

    async fn send(&mut self, ev: &ClientEvent) {
        self.0.send(WsMsg::Text(serde_json::to_string(ev).unwrap().into())).await.unwrap();
    }
}

// ---------------------------------------------------------------- privacy

#[tokio::test]
async fn only_the_two_people_ever_see_a_dm() {
    let env = setup().await;
    let (owner, admin) = env.staff().await;
    let (ana, bia, caio) = (env.who("Ana").await, env.who("Bia").await, env.who("Caio").await);
    let (mut ws_ana, mut ws_bia) = (env.ws(&ana).await, env.ws(&bia).await);
    let mut outsiders = [env.ws(&caio).await, env.ws(&owner).await, env.ws(&admin).await];

    let dm = env.open(&ana, &bia).await;
    assert_eq!((dm.peer.id, dm.unread, dm.last_message.clone()), (bia.user.id, 0, None));
    // Opening is idempotent and is the same conversation from both sides.
    assert_eq!(env.open(&ana, &bia).await.id, dm.id);
    assert!(env.list(&bia).await.is_empty(), "an empty DM shows only for whoever opened it");

    let sent = env.send_ok(&ana, dm.id, "segredo entre nós").await;
    assert_eq!((sent.author.id, sent.dm_id, sent.kind), (ana.user.id, dm.id, DmMessageKind::Text));
    for ws in [&mut ws_ana, &mut ws_bia] {
        let ev = ws.wait_for("dm message", 5, |e| matches!(e, ServerEvent::DmMessageCreated { .. })).await;
        let ServerEvent::DmMessageCreated { message } = ev else { unreachable!() };
        assert_eq!(message, sent);
    }
    // Bia's list: the conversation, unread, newest line.
    let list = env.list(&bia).await;
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].id, list[0].peer.id, list[0].unread), (dm.id, ana.user.id, 1));
    assert_eq!(list[0].last_message.as_ref().map(|m| m.content.as_str()), Some("segredo entre nós"));
    assert_eq!(env.list(&ana).await[0].unread, 0, "my own lines are read");

    // A third member, an Admin and the owner: nothing, through any door.
    for who in [&caio, &admin, &owner] {
        assert!(env.list(who).await.iter().all(|c| c.id != dm.id), "{}", who.user.username);
        let base = format!("/api/dms/{}", dm.id);
        assert_eq!(env.get(who, &format!("{base}/messages")).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(env.get(who, &format!("{base}/messages?before=999999&limit=100")).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(env.send(who, dm.id, "posso ler?").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(env.post(who, &format!("{base}/messages/{}/delete", sent.id), json!({})).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(env.post(who, &format!("{base}/read"), json!({"up_to": sent.id})).await.status(), StatusCode::NOT_FOUND);
        for action in ["", "accept", "decline", "end"] {
            assert_eq!(env.call(who, dm.id, action).await.status(), StatusCode::NOT_FOUND, "call/{action} by {}", who.user.username);
        }
        // A conversation id that does not exist answers the same.
        assert_eq!(env.get(who, "/api/dms/987654/messages").await.status(), StatusCode::NOT_FOUND);
    }
    // The channel moderation path cannot reach a DM line (separate id space), nor can the audit log show one.
    let _ = env.post(&owner, &format!("/api/messages/{}/delete", sent.id), json!({})).await;
    assert_eq!(env.history(&bia, dm.id, "").await.len(), 1, "the DM line is untouched");
    let audit: Vec<AuditEntry> = env.get(&owner, "/api/admin/audit").await.json().await.unwrap();
    assert!(audit.iter().all(|e| !e.detail.contains("segredo")), "{audit:?}");

    // A call: tokens only for the two; the outsiders hear about none of it.
    let r = env.call(&ana, dm.id, "").await;
    assert_eq!(r.status(), StatusCode::OK);
    let started: DmCallResponse = r.json().await.unwrap();
    let room = dm_room(ana.user.id, bia.user.id);
    assert_eq!(started.join.room, room);
    let c = claims(&started.join.livekit_token);
    assert_eq!((c["video"]["room"].as_str(), c["sub"].as_str()), (Some(room.as_str()), Some(ana.user.id.to_string().as_str())));
    ws_bia.wait_for("call invite", 5, |e| matches!(e, ServerEvent::CallInvite { call } if call.caller.id == ana.user.id)).await;
    for who in [&caio, &admin, &owner] {
        for action in ["", "accept"] {
            assert_eq!(env.call(who, dm.id, action).await.status(), StatusCode::NOT_FOUND);
        }
    }
    let r = env.call(&bia, dm.id, "accept").await;
    let answered: DmCallResponse = r.json().await.unwrap();
    let c = claims(&answered.join.livekit_token);
    assert_eq!((c["video"]["room"].as_str(), c["sub"].as_str()), (Some(room.as_str()), Some(bia.user.id.to_string().as_str())));
    // Nobody else's voice state ever mentions the call; a fresh snapshot shows nobody in voice.
    let (mut fresh, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", env.base, caio.token)).await.unwrap();
    let Some(Ok(WsMsg::Text(t))) = fresh.next().await else { panic!() };
    let ServerEvent::Ready { voice, .. } = serde_json::from_str::<ServerEvent>(&t).unwrap() else { panic!("{t}") };
    assert!(voice.is_empty(), "{voice:?}");
    assert_eq!(env.call(&ana, dm.id, "end").await.status(), StatusCode::OK);
    ws_bia.wait_for("call ended", 5, |e| matches!(e, ServerEvent::CallEnded { reason: CallEndReason::Hangup, .. })).await;

    for ws in outsiders.iter_mut() {
        ws.hears_nothing_private(800).await;
    }
    // The people list (for "Nova mensagem") has everybody but me.
    let people: Vec<User> = env.get(&ana, "/api/dms/people").await.json().await.unwrap();
    assert!(people.iter().any(|u| u.id == bia.user.id) && people.iter().any(|u| u.id == caio.user.id));
    assert!(people.iter().all(|u| u.id != ana.user.id));
    // No DM with myself, nor with somebody who does not exist.
    assert_eq!(env.post(&ana, &format!("/api/dms/{}", ana.user.id), json!({})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.post(&ana, "/api/dms/424242", json!({})).await.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------- history, unread, deletes

#[tokio::test]
async fn history_pages_unread_counts_and_deletes() {
    let env = setup().await;
    let (ana, bia, caio) = (env.who("Ana").await, env.who("Bia").await, env.who("Caio").await);
    let dm = env.open(&ana, &bia).await;
    let mut ids = Vec::new();
    for i in 0..7 {
        ids.push(env.send_ok(&ana, dm.id, &format!("m{i}")).await.id);
        tokio::time::sleep(Duration::from_millis(220)).await; // under the ~5 per second limit
    }
    let reply = env.send_ok(&bia, dm.id, "oi!").await;
    // Newest first, paged with `before`.
    let page = env.history(&bia, dm.id, "?limit=3").await;
    assert_eq!(page.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["oi!", "m6", "m5"]);
    let older = env.history(&bia, dm.id, &format!("?before={}&limit=3", page[2].id)).await;
    assert_eq!(older.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["m4", "m3", "m2"]);
    assert_eq!(env.history(&bia, dm.id, "?limit=1000").await.len(), 8, "the limit is clamped, not refused");
    // Bia wrote last, so she has read everything; Ana has one unread.
    let unread = |l: Vec<DmConversation>| l.iter().find(|c| c.id == dm.id).map(|c| (c.unread, c.read_up_to)).unwrap();
    assert_eq!(unread(env.list(&bia).await), (0, reply.id));
    assert_eq!(unread(env.list(&ana).await), (1, ids[6]));
    // Reading: forward only, never past the end, told to my own devices only.
    let mut ws_ana = env.ws(&ana).await;
    let mut ws_bia = env.ws(&bia).await;
    let r = env.post(&ana, &format!("/api/dms/{}/read", dm.id), json!({ "up_to": reply.id + 500 })).await;
    assert_eq!(r.status(), StatusCode::OK);
    let ev = ws_ana.wait_for("read", 5, |e| matches!(e, ServerEvent::DmRead { .. })).await;
    assert!(matches!(ev, ServerEvent::DmRead { dm_id, up_to } if dm_id == dm.id && up_to == reply.id));
    assert_eq!(unread(env.list(&ana).await), (0, reply.id));
    env.post(&ana, &format!("/api/dms/{}/read", dm.id), json!({ "up_to": 1 })).await;
    assert_eq!(unread(env.list(&ana).await), (0, reply.id), "a read pointer never goes back");
    ws_bia.hears_nothing_private(500).await;

    // Deletes: only my own lines.
    let (st, _) = error_of(env.post(&bia, &format!("/api/dms/{}/messages/{}/delete", dm.id, ids[0]), json!({})).await).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(env.post(&ana, &format!("/api/dms/{}/messages/{}/delete", dm.id, ids[0]), json!({})).await.status(), StatusCode::OK);
    for ws in [&mut ws_ana, &mut ws_bia] {
        let ev = ws.wait_for("deleted", 5, |e| matches!(e, ServerEvent::DmMessageDeleted { .. })).await;
        assert!(matches!(ev, ServerEvent::DmMessageDeleted { dm_id, message_id } if dm_id == dm.id && message_id == ids[0]));
    }
    assert!(env.history(&bia, dm.id, "").await.iter().all(|m| m.id != ids[0]));
    // A message id of another conversation cannot be deleted through this one.
    let other = env.open(&caio, &ana).await;
    let theirs = env.send_ok(&caio, other.id, "outra conversa").await;
    assert_eq!(env.post(&ana, &format!("/api/dms/{}/messages/{}/delete", dm.id, theirs.id), json!({})).await.status(), StatusCode::NOT_FOUND);
    // Lists are ordered by the last activity.
    let order: Vec<DmId> = env.list(&ana).await.iter().map(|c| c.id).collect();
    assert_eq!(order, vec![other.id, dm.id]);
}

// ---------------------------------------------------------------- blocking and limits

#[tokio::test]
async fn blocking_stops_messages_and_calls_both_ways() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let dm = env.open(&ana, &bia).await;
    env.send_ok(&ana, dm.id, "oi").await;
    assert_eq!(env.post(&bia, &format!("/api/dms/blocks/{}", ana.user.id), json!({"blocked": true})).await.status(), StatusCode::OK);
    assert_eq!(error_of(env.send(&ana, dm.id, "e aí?").await).await, (StatusCode::FORBIDDEN, ERR_DM_BLOCKED.to_string()));
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::FORBIDDEN);
    // The one who blocked cannot write either until they unblock; their list says so.
    assert_eq!(env.send(&bia, dm.id, "tchau").await.status(), StatusCode::FORBIDDEN);
    assert!(env.list(&bia).await[0].blocked);
    assert!(!env.list(&ana).await[0].blocked, "the blocked person is not told who blocked whom");
    assert_eq!(env.post(&bia, &format!("/api/dms/blocks/{}", ana.user.id), json!({"blocked": false})).await.status(), StatusCode::OK);
    env.send_ok(&ana, dm.id, "de novo").await;
    // Blocking during a call ends it.
    let mut ws_ana = env.ws(&ana).await;
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
    assert_eq!(env.call(&bia, dm.id, "accept").await.status(), StatusCode::OK);
    env.post(&bia, &format!("/api/dms/blocks/{}", ana.user.id), json!({"blocked": true})).await;
    ws_ana.wait_for("call ended", 5, |e| matches!(e, ServerEvent::CallEnded { .. })).await;
    assert_eq!(env.post(&bia, &format!("/api/dms/blocks/{}", bia.user.id), json!({"blocked": true})).await.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn size_and_rate_limits_are_the_channels_ones() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let dm = env.open(&ana, &bia).await;
    assert_eq!(env.send(&ana, dm.id, "   ").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.send(&ana, dm.id, &"x".repeat(DM_MESSAGE_MAX_CHARS + 1)).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.send(&ana, dm.id, &"é".repeat(DM_MESSAGE_MAX_CHARS)).await.status(), StatusCode::OK, "characters, not bytes");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut statuses = Vec::new();
    for i in 0..12 {
        statuses.push(env.send(&ana, dm.id, &format!("rajada {i}")).await.status());
    }
    assert!(statuses.contains(&StatusCode::TOO_MANY_REQUESTS), "{statuses:?}");
    assert!(statuses.iter().filter(|s| **s == StatusCode::OK).count() <= 6, "{statuses:?}");
}

// ---------------------------------------------------------------- calls

async fn call_lines(env: &Env, who: &Who, dm: DmId) -> Vec<(DmMessageKind, UserId)> {
    env.history(who, dm, "").await.into_iter().filter(|m| m.kind.is_call()).map(|m| (m.kind, m.author.id)).rev().collect()
}

#[tokio::test]
async fn a_call_rings_is_answered_and_hung_up() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let (mut ws_ana, mut ws_bia) = (env.ws(&ana).await, env.ws(&bia).await);
    let dm = env.open(&ana, &bia).await;
    let started: DmCallResponse = env.call(&ana, dm.id, "").await.json().await.unwrap();
    assert_eq!(started.call.state, CallState::Ringing);
    assert_eq!((started.call.caller.id, started.call.callee.id), (ana.user.id, bia.user.id));
    for ws in [&mut ws_ana, &mut ws_bia] {
        ws.wait_for("invite", 5, |e| matches!(e, ServerEvent::CallInvite { call } if call.id == started.call.id)).await;
    }
    // Only the callee answers; calling again as the caller just hands the token again.
    assert_eq!(env.call(&ana, dm.id, "accept").await.status(), StatusCode::CONFLICT);
    let again: DmCallResponse = env.call(&ana, dm.id, "").await.json().await.unwrap();
    assert_eq!((again.call.id, again.call.state), (started.call.id, CallState::Ringing));
    // "Ligar" from the callee's side answers it.
    let answered: DmCallResponse = env.call(&bia, dm.id, "").await.json().await.unwrap();
    assert_eq!((answered.call.id, answered.call.state), (started.call.id, CallState::Active));
    for ws in [&mut ws_ana, &mut ws_bia] {
        ws.wait_for("accepted", 5, |e| matches!(e, ServerEvent::CallAccepted { call } if call.state == CallState::Active)).await;
    }
    // It does not ring out once answered.
    tokio::time::sleep(RING + Duration::from_millis(500)).await;
    assert_eq!(env.call(&bia, dm.id, "decline").await.status(), StatusCode::CONFLICT, "an answered call is not declined");
    assert_eq!(env.call(&bia, dm.id, "end").await.status(), StatusCode::OK);
    for ws in [&mut ws_ana, &mut ws_bia] {
        ws.wait_for("ended", 5, |e| matches!(e, ServerEvent::CallEnded { reason: CallEndReason::Hangup, .. })).await;
    }
    let lines = env.history(&ana, dm.id, "").await;
    assert_eq!(lines[0].kind, DmMessageKind::CallEnded);
    assert!(lines[0].call_secs >= 1, "{:?}", lines[0]);
    assert_eq!(env.call(&ana, dm.id, "end").await.status(), StatusCode::OK, "ending twice is harmless");
    assert_eq!(env.call(&bia, dm.id, "accept").await.status(), StatusCode::CONFLICT, "no call any more");
}

#[tokio::test]
async fn missed_declined_cancelled_and_busy_calls_are_logged() {
    let env = setup().await;
    let (ana, bia, caio) = (env.who("Ana").await, env.who("Bia").await, env.who("Caio").await);
    let mut ws_bia = env.ws(&bia).await;
    let dm = env.open(&ana, &bia).await;

    // Nobody answers: it rings out.
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
    let ev = ws_bia.wait_for("missed", 5, |e| matches!(e, ServerEvent::CallEnded { .. })).await;
    assert!(matches!(ev, ServerEvent::CallEnded { reason: CallEndReason::Missed, .. }), "{ev:?}");
    // Declined.
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
    assert_eq!(env.call(&ana, dm.id, "decline").await.status(), StatusCode::CONFLICT, "the caller cannot decline");
    assert_eq!(env.call(&bia, dm.id, "decline").await.status(), StatusCode::OK);
    ws_bia.wait_for("declined", 5, |e| matches!(e, ServerEvent::CallDeclined { .. })).await;
    // Cancelled by the caller.
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
    assert_eq!(env.call(&ana, dm.id, "end").await.status(), StatusCode::OK);
    ws_bia.wait_for("cancelled", 5, |e| matches!(e, ServerEvent::CallEnded { reason: CallEndReason::Cancelled, .. })).await;
    let a = ana.user.id;
    assert_eq!(call_lines(&env, &bia, dm.id).await, [(DmMessageKind::CallMissed, a), (DmMessageKind::CallDeclined, a), (DmMessageKind::CallMissed, a)]);
    // Missed calls count as unread for the one who missed them.
    assert_eq!(env.list(&bia).await.iter().find(|c| c.id == dm.id).unwrap().unread, 3);

    // Busy: Bia is in a call with Ana, Caio calls Bia.
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
    let dm2 = env.open(&caio, &bia).await;
    assert_eq!(error_of(env.call(&caio, dm2.id, "").await).await, (StatusCode::CONFLICT, ERR_CALL_BUSY.to_string()));
    assert_eq!(call_lines(&env, &bia, dm2.id).await, [(DmMessageKind::CallMissed, caio.user.id)]);
    // Ana calling somebody else hangs up her own call first.
    let dm3 = env.open(&ana, &caio).await;
    assert_eq!(env.call(&ana, dm3.id, "").await.status(), StatusCode::OK);
    ws_bia.wait_for("cancelled", 5, |e| matches!(e, ServerEvent::CallEnded { dm_id, .. } if *dm_id == dm.id)).await;
    env.call(&ana, dm3.id, "end").await;
}

#[tokio::test]
async fn joining_a_voice_channel_leaves_the_call() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let (mut ws_ana, mut ws_bia) = (env.ws(&ana).await, env.ws(&bia).await);
    let dm = env.open(&ana, &bia).await;
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
    assert_eq!(env.call(&bia, dm.id, "accept").await.status(), StatusCode::OK);
    let channels: Vec<Channel> = env.get(&bia, "/api/channels").await.json().await.unwrap();
    let voice = channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    ws_bia.send(&ClientEvent::VoiceState { channel_id: Some(voice), muted: false, deafened: false, streaming: false, camera: false, audio_only: false }).await;
    let ev = ws_ana.wait_for("ended", 5, |e| matches!(e, ServerEvent::CallEnded { .. })).await;
    assert!(matches!(ev, ServerEvent::CallEnded { reason: CallEndReason::JoinedVoice, .. }), "{ev:?}");
}

/// v1.0.1 (audit `voxhub:hub:dm-call-end-reveals-private-voice-join`): leaving a call for a voice
/// channel the other person may not see is a plain hang-up for them; they hear "joined a voice
/// channel" only for a channel they can see. The one who left gets the real reason.
#[tokio::test]
async fn leaving_a_call_for_a_hidden_voice_channel_looks_like_a_hang_up() {
    let env = setup().await;
    let (owner, _) = env.staff().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let r = env.post(&owner, "/api/admin/channels", json!(ChannelCreateRequest { name: "segredo".into(), kind: ChannelKind::Voice, private: Some(PrivateAccess { roles: vec![], users: vec![bia.user.id] }) })).await;
    assert_eq!(r.status(), StatusCode::OK);
    let hidden: Channel = r.json().await.unwrap();
    let channels: Vec<Channel> = env.get(&ana, "/api/channels").await.json().await.unwrap();
    assert!(!channels.iter().any(|c| c.id == hidden.id), "Ana may not see it");
    let public = channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    let (mut ws_ana, mut ws_bia) = (env.ws(&ana).await, env.ws(&bia).await);
    let dm = env.open(&ana, &bia).await;
    let voice = |ch| ClientEvent::VoiceState { channel_id: Some(ch), muted: false, deafened: false, streaming: false, camera: false, audio_only: false };
    let ended = |e: &ServerEvent| matches!(e, ServerEvent::CallEnded { .. });

    // Into the private channel: Ana only hears a hang-up.
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
    assert_eq!(env.call(&bia, dm.id, "accept").await.status(), StatusCode::OK);
    ws_bia.send(&voice(hidden.id)).await;
    let ev = ws_ana.wait_for("ended", 5, ended).await;
    assert!(matches!(ev, ServerEvent::CallEnded { reason: CallEndReason::Hangup, .. }), "{ev:?}");
    let ev = ws_bia.wait_for("ended", 5, ended).await;
    assert!(matches!(ev, ServerEvent::CallEnded { reason: CallEndReason::JoinedVoice, .. }), "{ev:?}");

    // Into a channel Ana sees: the real reason, as before.
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
    assert_eq!(env.call(&bia, dm.id, "accept").await.status(), StatusCode::OK);
    ws_bia.send(&voice(public)).await;
    let ev = ws_ana.wait_for("ended", 5, ended).await;
    assert!(matches!(ev, ServerEvent::CallEnded { reason: CallEndReason::JoinedVoice, .. }), "{ev:?}");
}

#[tokio::test]
async fn a_callee_who_connects_while_it_rings_gets_the_invite() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let dm = env.open(&ana, &bia).await;
    let started: DmCallResponse = env.call(&ana, dm.id, "").await.json().await.unwrap();
    let mut ws_bia = env.ws(&bia).await;
    ws_bia.wait_for("invite after connecting", 5, |e| matches!(e, ServerEvent::CallInvite { call } if call.id == started.call.id)).await;
    env.call(&ana, dm.id, "end").await;
}

#[tokio::test]
async fn call_starts_are_rate_limited() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let dm = env.open(&ana, &bia).await;
    for _ in 0..CALL_RATE_LIMIT {
        assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::OK);
        assert_eq!(env.call(&ana, dm.id, "end").await.status(), StatusCode::OK);
    }
    assert_eq!(env.call(&ana, dm.id, "").await.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// Calling somebody who is busy, again and again: one missed-call line for them, and the
/// attempts count against the call budget (no endless lines, events and alerts).
#[tokio::test]
async fn retrying_a_busy_person_is_limited() {
    let env = setup().await;
    let (ana, bia, caio) = (env.who("Ana").await, env.who("Bia").await, env.who("Caio").await);
    let mut ws_bia = env.ws(&bia).await;
    let busy_dm = env.open(&caio, &bia).await;
    assert_eq!(env.call(&caio, busy_dm.id, "").await.status(), StatusCode::OK, "Bia's phone rings: she is busy");
    let dm = env.open(&ana, &bia).await;
    let mut statuses = Vec::new();
    for _ in 0..20 {
        statuses.push(env.call(&ana, dm.id, "").await.status());
    }
    assert!(statuses[..CALL_RATE_LIMIT].iter().all(|s| *s == StatusCode::CONFLICT), "{statuses:?}");
    assert!(statuses[CALL_RATE_LIMIT..].iter().all(|s| *s == StatusCode::TOO_MANY_REQUESTS), "{statuses:?}");
    assert_eq!(call_lines(&env, &bia, dm.id).await, [(DmMessageKind::CallMissed, ana.user.id)], "one line per busy call");
    let mut lines = 0;
    while let Some(ev) = ws_bia.next_within(Duration::from_millis(500)).await {
        if matches!(&ev, ServerEvent::DmMessageCreated { message, .. } if message.dm_id == dm.id) {
            lines += 1;
        }
    }
    assert_eq!(lines, 1);
    env.call(&caio, busy_dm.id, "end").await;
}
