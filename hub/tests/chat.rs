//! Mentions, replies and reactions (v0.8), end to end on a real hub (no LiveKit): what the hub
//! keeps of a mention (real people named in the text, `@todos` only from a Moderador or above),
//! replies (same channel / DM only, quotes in history, "mensagem apagada" once the original is
//! gone, the reply mention), reactions (one emoji, the per-message cap, live events, DM privacy,
//! blocking, deletion, rate limit), and that an app of the v0.7.0 shape keeps working.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "pw-chat";

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
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"chat\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n\
             [livekit]\napi_key = \"APIchat0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
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

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, who: &Who, path: &str) -> T {
        let r = self.http.get(self.url(path)).bearer_auth(&who.token).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        r.json().await.unwrap()
    }

    async fn geral(&self, who: &Who) -> ChannelId {
        let chs: Vec<Channel> = self.get(who, "/api/channels").await;
        chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id
    }

    async fn say(&self, who: &Who, ch: ChannelId, body: Value) -> Message {
        let r = self.post(who, &format!("/api/channels/{ch}/messages"), body).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn react(&self, who: &Who, id: MessageId, emoji: &str, add: bool) -> StatusCode {
        self.post(who, &format!("/api/messages/{id}/reactions"), json!({"emoji": emoji, "add": add})).await.status()
    }

    async fn history(&self, who: &Who, ch: ChannelId) -> Vec<Message> {
        self.get(who, &format!("/api/channels/{ch}/messages")).await
    }

    async fn ws(&self, who: &Who) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        let mut ws = Ws(ws);
        match ws.next_within(Duration::from_secs(5)).await {
            Some(ServerEvent::Ready { chat, .. }) => assert_eq!(chat, Some(ChatFeatures::ALL), "the hub announces the chat features"),
            other => panic!("expected Ready, got {other:?}"),
        }
        ws
    }

    /// A member promoted to Moderador, with the personal password that makes the role count.
    async fn moderator(&self, name: &str) -> Who {
        let owner = self.who("Dono").await;
        assert_eq!(self.post(&owner, "/api/account/password", json!({"new": "dono-senha"})).await.status(), StatusCode::OK);
        let m = self.who(name).await;
        assert_eq!(self.post(&m, "/api/account/password", json!({"new": "mod-senha"})).await.status(), StatusCode::OK);
        assert_eq!(self.post(&owner, &format!("/api/admin/users/{}/role", m.user.id), json!({"role": "moderator"})).await.status(), StatusCode::OK);
        m
    }
}

struct Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>);

impl Ws {
    async fn next_raw(&mut self, wait: Duration) -> Option<String> {
        loop {
            match tokio::time::timeout(wait, self.0.next()).await.ok()?? {
                Ok(WsMsg::Text(t)) => return Some(t.to_string()),
                Ok(WsMsg::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn next_within(&mut self, wait: Duration) -> Option<ServerEvent> {
        self.next_raw(wait).await.map(|t| serde_json::from_str(&t).unwrap())
    }

    async fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&ServerEvent) -> bool) -> ServerEvent {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            match self.next_within(Duration::from_secs(5)).await {
                Some(ev) if pred(&ev) => return ev,
                Some(_) => {}
                None => break,
            }
        }
        panic!("no {what} event");
    }

    /// Nothing matching `pred` arrives in the next `ms`.
    async fn hears_no(&mut self, ms: u64, pred: impl Fn(&ServerEvent) -> bool) {
        let t = Instant::now();
        while let Some(left) = Duration::from_millis(ms).checked_sub(t.elapsed()) {
            match self.next_within(left).await {
                Some(ev) => assert!(!pred(&ev), "unexpected {ev:?}"),
                None => return,
            }
        }
    }

    async fn send_json(&mut self, v: Value) {
        self.0.send(WsMsg::Text(v.to_string().into())).await.unwrap();
    }
}

fn created(ev: &ServerEvent, text: &str) -> Option<Message> {
    match ev {
        ServerEvent::MessageCreated { message } if message.content == text => Some(message.clone()),
        _ => None,
    }
}

#[tokio::test]
async fn mentions_are_checked_by_the_hub() {
    let env = setup().await;
    let (ana, bia, caio) = (env.who("Ana").await, env.who("Bia").await, env.who("Caio").await);
    // Bia has a display name: "@Bia Souza" mentions her too.
    let r = env.http.put(env.url("/api/me/profile")).bearer_auth(&bia.token).json(&json!({"display_name": "Bia Souza"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let ch = env.geral(&ana).await;

    // Only people named in the text count; an unknown id, the author and a 404 id are dropped.
    let m = env.say(&ana, ch, json!({"content": "oi @bia e @Caio", "mentions": [bia.user.id, caio.user.id, ana.user.id, 999]})).await;
    assert_eq!(m.mentions, vec![bia.user.id, caio.user.id]);
    let m = env.say(&ana, ch, json!({"content": "valeu @Bia Souza!", "mentions": [bia.user.id, caio.user.id]})).await;
    assert_eq!(m.mentions, vec![bia.user.id], "Caio is not in the text: no ghost mention");
    let m = env.say(&ana, ch, json!({"content": "sem arroba", "mentions": [bia.user.id]})).await;
    assert!(m.mentions.is_empty());
    assert!(!m.mentions_me(bia.user.id));

    // A member's @todos is plain text, a moderator's mentions everybody (over the WebSocket too).
    let m = env.say(&ana, ch, json!({"content": "@todos reunião"})).await;
    assert!(!m.mention_everyone);
    let moder = env.moderator("Mod").await;
    let m = env.say(&moder, ch, json!({"content": "@todos reunião agora"})).await;
    assert!(m.mention_everyone && m.mentions_me(ana.user.id));
    let mut w = env.ws(&moder).await;
    w.send_json(json!({"type": "send_message", "channel_id": ch, "content": "aviso @TODOS"})).await;
    let ev = w.wait_for("message", |e| created(e, "aviso @TODOS").is_some()).await;
    assert!(created(&ev, "aviso @TODOS").unwrap().mention_everyone, "an old app's @todos works too");

    // History carries them.
    let h = env.history(&ana, ch).await;
    assert_eq!(h.iter().find(|m| m.content == "oi @bia e @Caio").unwrap().mentions, vec![bia.user.id, caio.user.id]);
    assert!(h.iter().find(|m| m.content == "@todos reunião agora").unwrap().mention_everyone);
    assert!(!h.iter().find(|m| m.content == "@todos reunião").unwrap().mention_everyone);
}

/// v1.0.1 (audit `voxhub:hub:mention-id-list-unbounded-db-work`): a mention list longer than
/// `MENTIONS_WIRE_MAX` is refused before any work, over HTTP and over the WebSocket, and a huge
/// body is not even read; up to that many ids (repeats and unknown ones included) still work.
#[tokio::test]
async fn huge_mention_lists_are_refused_before_any_work() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let ch = env.geral(&ana).await;

    let too_many: Vec<UserId> = (0..MENTIONS_WIRE_MAX as i64 + 1).map(|i| 1000 + i).collect();
    let r = env.post(&ana, &format!("/api/channels/{ch}/messages"), json!({"content": "oi @Bia", "mentions": too_many})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(r.json::<ApiError>().await.unwrap().error, ERR_TOO_MANY_MENTIONS);
    // The flood the audit describes (about a million ids in one body) is cut off by the body limit.
    let flood = format!("{{\"content\":\"oi\",\"mentions\":[{}2]}}", "2,".repeat(200_000));
    let sent = env.http.post(env.url(&format!("/api/channels/{ch}/messages"))).bearer_auth(&ana.token).header("content-type", "application/json").body(flood).send().await;
    match sent {
        Ok(r) => {
            assert_eq!(r.status(), StatusCode::BAD_REQUEST);
            let e = r.json::<ApiError>().await.unwrap().error;
            assert!(e.contains("length limit"), "{e}");
        }
        // The hub answered and closed before the rest of the body was even sent.
        Err(e) => assert!(e.is_request(), "{e}"),
    }

    let mut w = env.ws(&ana).await;
    w.send_json(json!({"type": "send_message", "channel_id": ch, "content": "pelo socket @Bia", "mentions": vec![bia.user.id; MENTIONS_WIRE_MAX + 1]})).await;
    w.wait_for("the refusal", |e| matches!(e, ServerEvent::Error { error } if error == ERR_TOO_MANY_MENTIONS)).await;
    assert!(env.history(&ana, ch).await.iter().all(|m| !m.content.contains("@Bia")), "nothing was saved");

    // Exactly the limit: repeats count once, unknown ids are dropped, Bia is mentioned.
    let mut ids: Vec<UserId> = vec![bia.user.id; 10];
    ids.extend((0..MENTIONS_WIRE_MAX as i64 - 10).map(|i| 5000 + i));
    let m = env.say(&ana, ch, json!({"content": "oi @Bia", "mentions": ids})).await;
    assert_eq!(m.mentions, vec![bia.user.id]);
}

#[tokio::test]
async fn replies_quote_the_original_and_survive_its_deletion() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let ch = env.geral(&ana).await;
    let original = env.say(&ana, ch, json!({"content": format!("primeira linha\n{}", "longa ".repeat(40))})).await;

    // Over the WebSocket, with the reply mention (Discord's "@ ligado").
    let mut wa = env.ws(&ana).await;
    let mut wb = env.ws(&bia).await;
    wb.send_json(json!({"type": "send_message", "channel_id": ch, "content": "concordo", "reply_to": original.id, "reply_mention": true})).await;
    let ev = wa.wait_for("reply", |e| created(e, "concordo").is_some()).await;
    let reply = created(&ev, "concordo").unwrap();
    let q = reply.reply.clone().unwrap();
    assert_eq!((q.id, q.author.as_ref().map(|u| u.id), q.deleted), (original.id, Some(ana.user.id), false));
    assert!(q.excerpt.starts_with("primeira linha longa") && q.excerpt.ends_with('…') && q.excerpt.chars().count() <= REPLY_EXCERPT_MAX_CHARS);
    assert_eq!(reply.mentions, vec![ana.user.id], "replying mentions the original's author");
    assert!(reply.mentions_me(ana.user.id));

    // "@ ligado" off: a reply, no mention. Replying to myself never mentions me.
    let quiet = env.say(&bia, ch, json!({"content": "sem ping", "reply_to": original.id})).await;
    assert!(quiet.reply.is_some() && quiet.mentions.is_empty());
    let own = env.say(&ana, ch, json!({"content": "eu mesma", "reply_to": original.id, "reply_mention": true})).await;
    assert!(own.mentions.is_empty());

    // A reply to nothing is a plain message.
    let plain = env.say(&bia, ch, json!({"content": "perdido", "reply_to": 999_999})).await;
    assert!(plain.reply.is_none());

    // An image-only original quotes as images.
    let pic = {
        let img = image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([x as u8, y as u8, 9]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&img).unwrap();
        let a: Attachment = env.http.post(env.url("/api/attachments")).bearer_auth(&ana.token).body(out).send().await.unwrap().json().await.unwrap();
        env.say(&ana, ch, json!({"content": "", "attachments": [a.id]})).await
    };
    let r = env.say(&bia, ch, json!({"content": "linda", "reply_to": pic.id})).await;
    let q = r.reply.unwrap();
    assert_eq!((q.excerpt.as_str(), q.images), ("", 1));

    // The history has the quotes; once the original is deleted they say so.
    let h = env.history(&bia, ch).await;
    assert_eq!(h.iter().find(|m| m.id == reply.id).unwrap().reply.as_ref().unwrap().author.as_ref().unwrap().id, ana.user.id);
    assert_eq!(env.post(&ana, &format!("/api/messages/{}/delete", original.id), json!({})).await.status(), StatusCode::OK);
    let h = env.history(&bia, ch).await;
    let q = h.iter().find(|m| m.id == reply.id).unwrap().reply.clone().unwrap();
    assert_eq!(q, ReplyRef::gone(original.id));
    // Somebody else's new message must never take the deleted one's place in the quote (SQLite
    // reuses the newest id after a delete: here it is not the newest, so the id stays free).
    let later = env.say(&ana, ch, json!({"content": "nova"})).await;
    assert_ne!(later.id, original.id);
}

#[tokio::test]
async fn reactions_live_capped_and_gone_with_the_message() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let ch = env.geral(&ana).await;
    let m = env.say(&ana, ch, json!({"content": "reaja"})).await;
    let mut wa = env.ws(&ana).await;

    assert_eq!(env.react(&bia, m.id, "👍", true).await, StatusCode::OK);
    match wa.wait_for("reaction", |e| matches!(e, ServerEvent::ReactionChanged { .. })).await {
        ServerEvent::ReactionChanged { channel_id, message_id, emoji, user_id, added } => {
            assert_eq!((channel_id, message_id, emoji.as_str(), user_id, added), (ch, m.id, "👍", bia.user.id, true));
        }
        _ => unreachable!(),
    }
    // The same again changes nothing and tells nobody.
    assert_eq!(env.react(&bia, m.id, "👍", true).await, StatusCode::OK);
    wa.hears_no(300, |e| matches!(e, ServerEvent::ReactionChanged { .. })).await;
    assert_eq!(env.react(&ana, m.id, "👍", true).await, StatusCode::OK);
    assert_eq!(env.react(&ana, m.id, "🎉", true).await, StatusCode::OK);
    let h = env.history(&bia, ch).await;
    assert_eq!(h[0].reactions, vec![Reaction { emoji: "👍".into(), users: vec![bia.user.id, ana.user.id] }, Reaction { emoji: "🎉".into(), users: vec![ana.user.id] }]);
    assert_eq!(env.react(&bia, m.id, "👍", false).await, StatusCode::OK);
    match wa.wait_for("removal", |e| matches!(e, ServerEvent::ReactionChanged { added: false, .. })).await {
        ServerEvent::ReactionChanged { user_id, .. } => assert_eq!(user_id, bia.user.id),
        _ => unreachable!(),
    }

    // Only a single emoji, on a message that exists.
    for bad in ["", "ok", "👍👍", "<script>", "👍 🎉"] {
        assert_eq!(env.react(&bia, m.id, bad, true).await, StatusCode::BAD_REQUEST, "{bad:?}");
    }
    assert_eq!(env.react(&bia, 999_999, "👍", true).await, StatusCode::NOT_FOUND);

    // At most REACTIONS_MAX_PER_MESSAGE distinct emoji (one already there still works).
    let m2 = env.say(&ana, ch, json!({"content": "muitas"})).await;
    let pool: Vec<String> = (0x1F600u32..0x1F600 + REACTIONS_MAX_PER_MESSAGE as u32).map(|c| char::from_u32(c).unwrap().to_string()).collect();
    for (i, e) in pool.iter().enumerate() {
        // (Two people, so the rate limit's burst is not the subject here.)
        let who = if i % 2 == 0 { &ana } else { &bia };
        assert_eq!(env.react(who, m2.id, e, true).await, StatusCode::OK, "{e}");
        tokio::time::sleep(Duration::from_millis(260)).await;
    }
    let r = env.post(&bia, &format!("/api/messages/{}/reactions", m2.id), json!({"emoji": "🎉", "add": true})).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(r.json::<ApiError>().await.unwrap().error, ERR_TOO_MANY_REACTIONS);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.react(&bia, m2.id, &pool[0], true).await, StatusCode::OK);

    // Deleting the message deletes its reactions.
    assert_eq!(env.post(&ana, &format!("/api/messages/{}/delete", m.id), json!({})).await.status(), StatusCode::OK);
    let h = env.history(&ana, ch).await;
    assert!(h.iter().all(|x| x.id != m.id));

    // Too fast: 429 (the bucket allows a burst, then about four a second).
    let flood = env.say(&bia, ch, json!({"content": "rápido"})).await;
    let mut limited = false;
    for i in 0..40 {
        let e = char::from_u32(0x1F600 + (i % 2)).unwrap().to_string();
        if env.react(&ana, flood.id, &e, i % 4 < 2).await == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(limited, "reactions are rate limited");
}

#[tokio::test]
async fn dm_replies_and_reactions_stay_between_the_two() {
    let env = setup().await;
    let (ana, bia, caio) = (env.who("Ana").await, env.who("Bia").await, env.who("Caio").await);
    let dm: DmConversation = env.post(&ana, &format!("/api/dms/{}", bia.user.id), json!({})).await.json().await.unwrap();
    let first: DmMessage = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({"content": "oi Bia"})).await.json().await.unwrap();
    let mut wb = env.ws(&bia).await;
    let mut wc = env.ws(&caio).await;

    let reply: DmMessage = env.post(&bia, &format!("/api/dms/{}/messages", dm.id), json!({"content": "oi!", "reply_to": first.id})).await.json().await.unwrap();
    let q = reply.reply.clone().unwrap();
    assert_eq!((q.id, q.excerpt.as_str(), q.author.unwrap().id), (first.id, "oi Bia", ana.user.id));

    // A reaction reaches the two of them only.
    let r = env.post(&ana, &format!("/api/dms/{}/messages/{}/reactions", dm.id, reply.id), json!({"emoji": "❤", "add": true})).await;
    assert_eq!(r.status(), StatusCode::OK);
    match wb.wait_for("dm reaction", |e| matches!(e, ServerEvent::DmReactionChanged { .. })).await {
        ServerEvent::DmReactionChanged { dm_id, message_id, emoji, user_id, added } => {
            assert_eq!((dm_id, message_id, emoji.as_str(), user_id, added), (dm.id, reply.id, "❤", ana.user.id, true));
        }
        _ => unreachable!(),
    }
    wc.hears_no(400, |e| matches!(e, ServerEvent::DmReactionChanged { .. } | ServerEvent::ReactionChanged { .. })).await;
    // Somebody else gets the same 404 as for a conversation that does not exist.
    let r = env.post(&caio, &format!("/api/dms/{}/messages/{}/reactions", dm.id, reply.id), json!({"emoji": "❤", "add": true})).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // The channel route cannot reach a DM line (separate id spaces).
    let lines: Vec<DmMessage> = env.get(&bia, &format!("/api/dms/{}/messages", dm.id)).await;
    let got = lines.iter().find(|l| l.id == reply.id).unwrap();
    assert_eq!(got.reactions, vec![Reaction { emoji: "❤".into(), users: vec![ana.user.id] }]);
    assert_eq!(got.reply.as_ref().unwrap().id, first.id);
    // The list's last message carries them too.
    let list: Vec<DmConversation> = env.get(&bia, "/api/dms").await;
    assert_eq!(list[0].last_message.as_ref().unwrap().reactions.len(), 1);

    // Blocked: no reactions either way.
    assert_eq!(env.post(&bia, &format!("/api/dms/blocks/{}", ana.user.id), json!({"blocked": true})).await.status(), StatusCode::OK);
    let r = env.post(&ana, &format!("/api/dms/{}/messages/{}/reactions", dm.id, first.id), json!({"emoji": "👍", "add": true})).await;
    assert_eq!((r.status(), r.json::<ApiError>().await.unwrap().error.as_str()), (StatusCode::FORBIDDEN, ERR_DM_BLOCKED));
    assert_eq!(env.post(&bia, &format!("/api/dms/blocks/{}", ana.user.id), json!({"blocked": false})).await.status(), StatusCode::OK);

    // Deleting the original: the reply's quote says so, and the deleted line's reactions are gone.
    assert_eq!(env.post(&bia, &format!("/api/dms/{}/messages/{}/reactions", dm.id, first.id), json!({"emoji": "👍", "add": true})).await.status(), StatusCode::OK);
    assert_eq!(env.post(&ana, &format!("/api/dms/{}/messages/{}/delete", dm.id, first.id), json!({})).await.status(), StatusCode::OK);
    let lines: Vec<DmMessage> = env.get(&bia, &format!("/api/dms/{}/messages", dm.id)).await;
    assert_eq!(lines.iter().find(|l| l.id == reply.id).unwrap().reply, Some(ReplyRef::gone(first.id)));
    let r = env.post(&bia, &format!("/api/dms/{}/messages/{}/reactions", dm.id, first.id), json!({"emoji": "👍", "add": false})).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // A reply to a line of another conversation is a plain line.
    let other: DmConversation = env.post(&ana, &format!("/api/dms/{}", caio.user.id), json!({})).await.json().await.unwrap();
    let x: DmMessage = env.post(&caio, &format!("/api/dms/{}/messages", other.id), json!({"content": "a", "reply_to": reply.id})).await.json().await.unwrap();
    assert!(x.reply.is_none());
}

/// An app of the v0.7.0 shape: its send has none of the new fields, it parses the events it
/// knows (the new fields ignored) and skips the ones it does not.
#[tokio::test]
async fn a_v070_app_keeps_working() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let ch = env.geral(&ana).await;
    let (mut old, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", env.base, ana.token)).await.unwrap();
    let mut wb = env.ws(&bia).await;
    old.send(WsMsg::Text(json!({"type": "send_message", "channel_id": ch, "content": "do app antigo @Bia"}).to_string().into())).await.unwrap();
    let ev = wb.wait_for("old message", |e| created(e, "do app antigo @Bia").is_some()).await;
    let m = created(&ev, "do app antigo @Bia").unwrap();
    assert!(m.mentions.is_empty() && m.reply.is_none(), "no ids from an old app: plain text");
    // New things happen; the old app's stream stays parseable by its own shapes.
    let reply = env.say(&bia, ch, json!({"content": "resposta", "reply_to": m.id, "reply_mention": true, "mentions": [ana.user.id]})).await;
    assert_eq!(env.react(&bia, m.id, "👍", true).await, StatusCode::OK);
    #[derive(serde::Deserialize, Debug)]
    #[serde(tag = "type", rename_all = "snake_case")]
    #[allow(dead_code)]
    enum V070Event {
        Ready { user: User },
        MessageCreated { message: V070Message },
        UserOnline { user: User },
    }
    #[derive(serde::Deserialize, Debug)]
    #[allow(dead_code)]
    struct V070Message {
        id: MessageId,
        content: String,
    }
    let (mut parsed, mut skipped) = (Vec::new(), 0);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) && !(parsed.iter().any(|c: &String| c == "resposta") && skipped > 0) {
        let Ok(Some(Ok(WsMsg::Text(t)))) = tokio::time::timeout(Duration::from_secs(3), old.next()).await else { break };
        match serde_json::from_str::<V070Event>(&t) {
            Ok(V070Event::MessageCreated { message }) => parsed.push(message.content),
            Ok(_) => {}
            Err(_) => skipped += 1, // what a v0.7.0 app does with an event it does not know
        }
    }
    assert!(parsed.contains(&"resposta".to_string()), "{parsed:?}");
    assert!(skipped > 0, "the reaction event is skipped");
    assert_eq!(reply.reply.unwrap().id, m.id);
    // Its history call still answers in a shape it reads.
    let raw: Vec<Value> = env.get(&ana, &format!("/api/channels/{ch}/messages")).await;
    let parsed: Vec<V070Message> = serde_json::from_value(Value::Array(raw)).unwrap();
    assert!(parsed.iter().any(|m| m.content == "resposta"));
}
