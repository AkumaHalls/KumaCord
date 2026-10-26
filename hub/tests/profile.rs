//! User profiles end-to-end: display name, bio, colour, custom status, presence; validation and
//! rate limits; the broadcasts; invisibility enforced in the snapshot and in every event; status
//! expiry; clearing by moderators (and its audit entry). Real server on an ephemeral port, no LiveKit.

use std::path::Path;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "pw";

struct Env {
    server: Server,
    base: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

fn write_config(dir: &Path) {
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "server_name = \"profiles\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
}

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path());
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) })
        .await
        .unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { server, base, http: reqwest::Client::new(), _dir: dir }
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
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

    async fn put(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.put(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> StatusCode {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap().status()
    }

    async fn profile_of(&self, viewer: &Who, id: i64) -> UserProfile {
        let r = self.http.get(self.url(&format!("/api/users/{id}/profile"))).bearer_auth(&viewer.token).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn edit(&self, who: &Who, body: Value) -> reqwest::Response {
        self.put(who, "/api/me/profile", body).await
    }

    async fn presence(&self, who: &Who, presence: &str, auto: bool) -> Presence {
        let r = self.put(who, "/api/me/presence", json!({"presence": presence, "auto": auto})).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json::<PresenceResponse>().await.unwrap().presence
    }

    async fn ws(&self, who: &Who) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        Ws(ws)
    }

    /// A connection that has already received its `Ready`.
    async fn ws_ready(&self, who: &Who) -> (Ws, Vec<User>) {
        let mut ws = self.ws(who).await;
        let ServerEvent::Ready { online, .. } = ws.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await else { unreachable!() };
        (ws, online)
    }

    async fn first_text_channel(&self, who: &Who) -> i64 {
        let chs: Vec<Channel> = self.http.get(self.url("/api/channels")).bearer_auth(&who.token).send().await.unwrap().json().await.unwrap();
        chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id
    }

    async fn staff(&self) -> (Who, Who, Who) {
        let owner = self.who("Dono").await;
        let moder = self.who("modi").await;
        let admin = self.who("adm").await;
        for (w, pw) in [(&owner, "dono-senha"), (&moder, "mod-senha"), (&admin, "adm-senha")] {
            let r = self.http.post(self.url("/api/account/password")).bearer_auth(&w.token).json(&json!({"new": pw})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        }
        assert_eq!(self.post(&owner, &format!("/api/admin/users/{}/role", moder.user.id), json!({"role": "moderator"})).await, StatusCode::OK);
        assert_eq!(self.post(&owner, &format!("/api/admin/users/{}/role", admin.user.id), json!({"role": "admin"})).await, StatusCode::OK);
        (owner, moder, admin)
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

    /// The next event matching `pred` (5 s at most).
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

    /// Nothing matching `pred` arrives within `wait`.
    async fn quiet_for(&mut self, wait: Duration, mut pred: impl FnMut(&ServerEvent) -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < wait {
            match self.next_within(wait.saturating_sub(t.elapsed())).await {
                Some(ev) if pred(&ev) => return false,
                Some(_) => {}
                None => break,
            }
        }
        true
    }

    /// Every event that arrives within `wait`.
    async fn drain(&mut self, wait: Duration) -> Vec<ServerEvent> {
        let t = Instant::now();
        let mut out = Vec::new();
        while t.elapsed() < wait {
            match self.next_within(wait.saturating_sub(t.elapsed())).await {
                Some(ev) => out.push(ev),
                None => break,
            }
        }
        out
    }

    async fn presence_of(&mut self, id: i64, want: Presence) -> Option<CustomStatus> {
        let ev = self.wait_for("presence_changed", |e| matches!(e, ServerEvent::PresenceChanged { user_id, presence, .. } if *user_id == id && *presence == want)).await;
        let ServerEvent::PresenceChanged { status, .. } = ev else { unreachable!() };
        status
    }

    async fn say(&mut self, channel_id: i64, text: &str) {
        let ev = ClientEvent::SendMessage { channel_id, content: text.into(), mentions: vec![], reply_to: None, reply_mention: false };
        self.0.send(WsMsg::Text(serde_json::to_string(&ev).unwrap().into())).await.unwrap();
    }
}

async fn error_of(r: reqwest::Response) -> (StatusCode, String) {
    let s = r.status();
    (s, r.json::<ApiError>().await.map(|e| e.error).unwrap_or_default())
}

fn is_about(ev: &ServerEvent, id: i64) -> bool {
    match ev {
        ServerEvent::PresenceChanged { user_id, .. } => *user_id == id,
        ServerEvent::UserOnline { user } | ServerEvent::UserUpdated { user } => user.id == id,
        ServerEvent::UserOffline { user_id } => *user_id == id,
        _ => false,
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn profile_fields_are_stored_validated_and_served() {
    let env = setup().await;
    let ana = env.who("ana").await;
    let bia = env.who("bia").await;
    assert_eq!(ana.user.display_name, None);

    // A new person: nothing set, "member since" is the first login.
    let p = env.profile_of(&bia, ana.user.id).await;
    assert_eq!((p.user.username.as_str(), p.bio.as_str(), p.accent.clone(), p.status.clone(), p.online), ("ana", "", None, None, false));
    assert!(p.member_since_ms > 1_600_000_000_000, "{}", p.member_since_ms);

    // Edit all three fields; the answer is my own profile, the colour is normalised.
    let r = env.edit(&ana, json!({"display_name": "  Ana Maria ", "bio": "Oi!\nVeja https://example.com/x", "accent": "#FF8800"})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let mine: UserProfile = r.json().await.unwrap();
    assert_eq!((mine.user.display_name.as_deref(), mine.bio.as_str(), mine.accent.as_deref()), (Some("Ana Maria"), "Oi!\nVeja https://example.com/x", Some("#ff8800")));
    assert_eq!(mine.user.shown_name(), "Ana Maria");
    assert_eq!(mine.user.username, "ana", "the login identity never changes");
    // Everybody reads the same, and so does a fresh login.
    assert_eq!(env.profile_of(&bia, ana.user.id).await, mine);
    assert_eq!(env.who("ana").await.user.display_name.as_deref(), Some("Ana Maria"));

    // A partial update leaves the rest alone; "" clears.
    let r = env.edit(&ana, json!({"bio": ""})).await;
    let p: UserProfile = r.json().await.unwrap();
    assert_eq!((p.user.display_name.as_deref(), p.bio.as_str(), p.accent.as_deref()), (Some("Ana Maria"), "", Some("#ff8800")));
    let p: UserProfile = env.edit(&ana, json!({"display_name": "", "accent": ""})).await.json().await.unwrap();
    assert_eq!((p.user.display_name, p.accent), (None, None));

    // Validation: each refusal is a 400 with a reason and changes nothing.
    env.edit(&ana, json!({"display_name": "Ana", "bio": "fica"})).await;
    let long_bio = "x".repeat(BIO_MAX + 1);
    let bad: Vec<(&str, Value)> = vec![
        ("name too long", json!({"display_name": "n".repeat(DISPLAY_NAME_MAX + 1)})),
        ("name with a control character", json!({"display_name": "a\u{7}b"})),
        ("name with a newline", json!({"display_name": "a\nb"})),
        ("name with a direction override", json!({"display_name": "x\u{202E}yz"})),
        ("name with zero-width space", json!({"display_name": "ad\u{200B}min"})),
        ("bio too long", json!({"bio": long_bio})),
        ("bio with a control character", json!({"bio": "a\u{1b}[31m"})),
        ("bio with too many lines", json!({"bio": "a\n".repeat(20)})),
        ("colour words", json!({"accent": "red"})),
        ("colour not hex", json!({"accent": "#gg0000"})),
        ("colour wrong length", json!({"accent": "#12345"})),
        ("one bad field spoils the whole update", json!({"bio": "novo", "accent": "nope"})),
    ];
    for (what, body) in bad {
        let (status, msg) = error_of(env.edit(&ana, body).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {msg}");
        assert!(!msg.is_empty(), "{what}");
    }
    let p = env.profile_of(&bia, ana.user.id).await;
    assert_eq!((p.user.display_name.as_deref(), p.bio.as_str()), (Some("Ana"), "fica"), "refused updates changed nothing");
    // Exactly at the limits is fine.
    let ok = env.edit(&ana, json!({"display_name": "n".repeat(DISPLAY_NAME_MAX), "bio": "x".repeat(BIO_MAX)})).await;
    assert_eq!(ok.status(), StatusCode::OK);
    // A body that is not JSON for the type.
    assert_eq!(env.put(&ana, "/api/me/profile", json!({"bio": 5})).await.status(), StatusCode::BAD_REQUEST);

    // Nobody can take another account's username as a display name; their own is fine.
    let (status, msg) = error_of(env.edit(&ana, json!({"display_name": "BIA"})).await).await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    assert_eq!(env.edit(&ana, json!({"display_name": "ANA"})).await.status(), StatusCode::OK);

    // Edges of the GET and the auth.
    let r = env.http.get(env.url("/api/users/9999/profile")).bearer_auth(&ana.token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = env.http.get(env.url(&format!("/api/users/{}/profile", ana.user.id))).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = env.http.put(env.url("/api/me/profile")).json(&json!({"bio": "x"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    env.server.shutdown().await;
}

#[tokio::test]
async fn the_display_name_reaches_lists_chat_and_history() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    let (mut bia_ws, _) = env.ws_ready(&bia).await;
    let (mut ana_ws, _) = env.ws_ready(&ana).await;
    assert_eq!(env.edit(&ana, json!({"display_name": "Aninha"})).await.status(), StatusCode::OK);
    let ev = bia_ws.wait_for("user_updated", |e| matches!(e, ServerEvent::UserUpdated { user } if user.id == ana.user.id)).await;
    let ServerEvent::UserUpdated { user } = ev else { unreachable!() };
    assert_eq!((user.username.as_str(), user.display_name.as_deref()), ("ana", Some("Aninha")));
    // The same text again says nothing.
    env.edit(&ana, json!({"display_name": "Aninha"})).await;
    assert!(bia_ws.quiet_for(Duration::from_millis(300), |e| matches!(e, ServerEvent::UserUpdated { .. })).await);
    // A new snapshot, a live message and the history all carry it.
    let (_carla_ws, online) = env.ws_ready(&env.who("carla").await).await;
    assert_eq!(online.iter().find(|u| u.id == ana.user.id).and_then(|u| u.display_name.as_deref()), Some("Aninha"));
    let ch = env.first_text_channel(&ana).await;
    ana_ws.say(ch, "oi").await;
    let ServerEvent::MessageCreated { message } = bia_ws.wait_for("message", |e| matches!(e, ServerEvent::MessageCreated { .. })).await else { unreachable!() };
    assert_eq!(message.author.display_name.as_deref(), Some("Aninha"));
    let hist: Vec<Message> = env.http.get(env.url(&format!("/api/channels/{ch}/messages"))).bearer_auth(&bia.token).send().await.unwrap().json().await.unwrap();
    assert_eq!((hist[0].author.username.as_str(), hist[0].author.display_name.as_deref()), ("ana", Some("Aninha")));
    // Removing it is broadcast too.
    env.edit(&ana, json!({"display_name": ""})).await;
    let ev = bia_ws.wait_for("user_updated", |e| matches!(e, ServerEvent::UserUpdated { user } if user.id == ana.user.id && user.display_name.is_none())).await;
    assert!(matches!(ev, ServerEvent::UserUpdated { .. }));
    env.server.shutdown().await;
}

#[tokio::test]
async fn custom_status_validation_and_clearing() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    let r = env.put(&ana, "/api/me/status", json!({"emoji": "🎮", "text": " jogando ", "expires_in_secs": 3600})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let st = r.json::<StatusResponse>().await.unwrap().status.unwrap();
    assert_eq!((st.emoji.as_str(), st.text.as_str()), ("🎮", "jogando"));
    let left = st.expires_at_ms.unwrap() - voxhub_now();
    assert!((3_500_000..=3_600_500).contains(&left), "{left}");
    let p = env.profile_of(&bia, ana.user.id).await;
    assert_eq!(p.status, Some(st));

    let bad: Vec<(&str, Value)> = vec![
        ("text too long", json!({"emoji": "", "text": "t".repeat(STATUS_TEXT_MAX + 1)})),
        ("text with a control character", json!({"text": "a\nb"})),
        ("emoji that is a word", json!({"emoji": "oi", "text": "x"})),
        ("emoji that is a letter", json!({"emoji": "a", "text": "x"})),
        ("two emoji with a space", json!({"emoji": "🎮 🎮", "text": "x"})),
        ("emoji too long", json!({"emoji": "😀".repeat(STATUS_EMOJI_MAX_CHARS + 1)})),
        ("clears itself too late", json!({"text": "x", "expires_in_secs": STATUS_MAX_SECS + 1})),
    ];
    for (what, body) in bad {
        let (status, msg) = error_of(env.put(&ana, "/api/me/status", body).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {msg}");
    }
    assert!(env.profile_of(&bia, ana.user.id).await.status.is_some(), "refused statuses changed nothing");
    // Only an emoji, or only text, is fine; the longest allowed too.
    for body in [json!({"emoji": "👨\u{200D}👩\u{200D}👧\u{200D}👦"}), json!({"text": "t".repeat(STATUS_TEXT_MAX)})] {
        assert_eq!(env.put(&ana, "/api/me/status", body).await.status(), StatusCode::OK);
    }
    // Empty means cleared, and so does DELETE.
    let r = env.put(&ana, "/api/me/status", json!({"emoji": "", "text": ""})).await;
    assert_eq!(r.json::<StatusResponse>().await.unwrap().status, None);
    assert_eq!(env.profile_of(&bia, ana.user.id).await.status, None);
    env.put(&ana, "/api/me/status", json!({"text": "de novo"})).await;
    let r = env.http.delete(env.url("/api/me/status")).bearer_auth(&ana.token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(env.profile_of(&bia, ana.user.id).await.status, None);
    env.server.shutdown().await;
}

fn voxhub_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

#[tokio::test]
async fn presence_and_status_are_broadcast_and_part_of_the_snapshot() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    let (mut bia_ws, _) = env.ws_ready(&bia).await;
    let (_ana_ws, _) = env.ws_ready(&ana).await;
    // Plain "online, no status" is not news: nothing is sent.
    assert!(bia_ws.quiet_for(Duration::from_millis(200), |e| matches!(e, ServerEvent::PresenceChanged { .. })).await);

    assert_eq!(env.presence(&ana, "dnd", false).await, Presence::Dnd);
    assert_eq!(bia_ws.presence_of(ana.user.id, Presence::Dnd).await, None);
    assert_eq!(env.put(&ana, "/api/me/status", json!({"emoji": "🎮", "text": "no jogo"})).await.status(), StatusCode::OK);
    let st = bia_ws.presence_of(ana.user.id, Presence::Dnd).await.expect("the status rides on the event");
    assert_eq!(st.text, "no jogo");
    let p = env.profile_of(&bia, ana.user.id).await;
    assert_eq!((p.online, p.presence), (true, Presence::Dnd));

    // Somebody who connects afterwards learns it right after Ready; people it is no news about are not listed.
    let carla = env.who("carla").await;
    let mut carla_ws = env.ws(&carla).await;
    let mut seen = Vec::new();
    let evs = carla_ws.drain(Duration::from_millis(500)).await;
    let ready_at = evs.iter().position(|e| matches!(e, ServerEvent::Ready { .. })).expect("ready");
    for e in &evs[ready_at + 1..] {
        if let ServerEvent::PresenceChanged { user_id, presence, status } = e {
            seen.push((*user_id, *presence, status.as_ref().map(|s| s.text.clone())));
        }
    }
    assert_eq!(seen, [(ana.user.id, Presence::Dnd, Some("no jogo".to_string()))]);

    // Away by hand, and back: the status stays.
    assert_eq!(env.presence(&ana, "away", false).await, Presence::Away);
    assert!(bia_ws.presence_of(ana.user.id, Presence::Away).await.is_some());
    assert_eq!(env.presence(&ana, "online", false).await, Presence::Online);
    assert!(bia_ws.presence_of(ana.user.id, Presence::Online).await.is_some());
    // The chosen presence outlives the connection: a new session starts as it was left.
    env.presence(&ana, "dnd", false).await;
    drop(_ana_ws);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_again, _) = env.ws_ready(&ana).await;
    assert!(bia_ws.presence_of(ana.user.id, Presence::Dnd).await.is_some());
    assert_eq!(env.profile_of(&bia, ana.user.id).await.presence, Presence::Dnd);
    env.server.shutdown().await;
}

#[tokio::test]
async fn idle_detection_only_moves_between_online_and_away() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    let (mut bia_ws, _) = env.ws_ready(&bia).await;
    let (_ana_ws, _) = env.ws_ready(&ana).await;
    assert_eq!(env.presence(&ana, "away", true).await, Presence::Away, "idle: away");
    bia_ws.presence_of(ana.user.id, Presence::Away).await;
    assert_eq!(env.presence(&ana, "away", true).await, Presence::Away, "the same again is not news");
    assert_eq!(env.presence(&ana, "online", true).await, Presence::Online, "active again");
    bia_ws.presence_of(ana.user.id, Presence::Online).await;
    // A chosen presence is never overridden by the automatic one, in either direction.
    env.presence(&ana, "dnd", false).await;
    bia_ws.presence_of(ana.user.id, Presence::Dnd).await;
    assert_eq!(env.presence(&ana, "away", true).await, Presence::Dnd);
    assert_eq!(env.presence(&ana, "online", true).await, Presence::Dnd);
    assert!(bia_ws.quiet_for(Duration::from_millis(300), |e| matches!(e, ServerEvent::PresenceChanged { .. })).await);
    // A manual choice after an automatic away takes over for good.
    env.presence(&ana, "online", false).await;
    bia_ws.presence_of(ana.user.id, Presence::Online).await;
    env.presence(&ana, "away", true).await;
    bia_ws.presence_of(ana.user.id, Presence::Away).await;
    env.presence(&ana, "dnd", false).await;
    bia_ws.presence_of(ana.user.id, Presence::Dnd).await;
    assert_eq!(env.presence(&ana, "online", true).await, Presence::Dnd, "idle detection cannot undo a manual choice");
    // An automatic away is not remembered across sessions.
    env.presence(&ana, "online", false).await;
    env.presence(&ana, "away", true).await;
    drop(_ana_ws);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = env.ws_ready(&ana).await;
    assert_eq!(env.profile_of(&bia, ana.user.id).await.presence, Presence::Online);
    env.server.shutdown().await;
}

#[tokio::test]
async fn an_invisible_person_is_offline_to_everybody_else_and_works_normally() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    let (mut bia_ws, online) = env.ws_ready(&bia).await;
    assert!(online.iter().all(|u| u.id != ana.user.id));
    let (mut ana_ws, _) = env.ws_ready(&ana).await;
    bia_ws.wait_for("ana online", |e| matches!(e, ServerEvent::UserOnline { user } if user.id == ana.user.id)).await;
    let mut ana2_ws = env.ws(&ana).await; // a second device
    ana2_ws.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await;

    // Going invisible looks like leaving - to a client that knows nothing about presence as well.
    assert_eq!(env.presence(&ana, "invisible", false).await, Presence::Invisible);
    bia_ws.wait_for("ana offline", |e| matches!(e, ServerEvent::UserOffline { user_id } if *user_id == ana.user.id)).await;
    // ...and nothing else about Ana is said to anyone but Ana's own connections.
    ana_ws.presence_of(ana.user.id, Presence::Invisible).await;
    ana2_ws.presence_of(ana.user.id, Presence::Invisible).await;
    assert!(bia_ws.quiet_for(Duration::from_millis(300), |e| is_about(e, ana.user.id)).await);

    // The profile says offline to others (and hides the status), and the truth to Ana.
    env.put(&ana, "/api/me/status", json!({"emoji": "🎮", "text": "escondida"})).await;
    let seen = env.profile_of(&bia, ana.user.id).await;
    assert_eq!((seen.online, seen.presence, seen.status.clone()), (false, Presence::Online, None));
    let own = env.profile_of(&ana, ana.user.id).await;
    assert_eq!((own.online, own.presence), (true, Presence::Invisible));
    assert_eq!(own.status.map(|s| s.text), Some("escondida".to_string()));
    assert!(bia_ws.quiet_for(Duration::from_millis(300), |e| is_about(e, ana.user.id)).await, "a status change while invisible is private");

    // New snapshots leave her out and do not mention her.
    let carla = env.who("carla").await;
    let mut carla_ws = env.ws(&carla).await;
    let evs = carla_ws.drain(Duration::from_millis(500)).await;
    let Some(ServerEvent::Ready { online, .. }) = evs.iter().find(|e| matches!(e, ServerEvent::Ready { .. })) else { panic!("no ready") };
    assert!(online.iter().all(|u| u.id != ana.user.id), "{online:?}");
    assert!(evs.iter().all(|e| !is_about(e, ana.user.id)), "{evs:?}");

    // She still works normally: she chats, and people see the message (with her name).
    let ch = env.first_text_channel(&ana).await;
    ana_ws.say(ch, "estou aqui, invisível").await;
    let ServerEvent::MessageCreated { message } = bia_ws.wait_for("message", |e| matches!(e, ServerEvent::MessageCreated { .. })).await else { unreachable!() };
    assert_eq!((message.author.username.as_str(), message.content.as_str()), ("ana", "estou aqui, invisível"));
    // ...and her own snapshot does not list her either, but tells her she is invisible.
    let mut ana3_ws = env.ws(&ana).await;
    let evs = ana3_ws.drain(Duration::from_millis(500)).await;
    let Some(ServerEvent::Ready { online, .. }) = evs.iter().find(|e| matches!(e, ServerEvent::Ready { .. })) else { panic!("no ready") };
    assert!(online.iter().all(|u| u.id != ana.user.id));
    assert!(evs.iter().any(|e| matches!(e, ServerEvent::PresenceChanged { user_id, presence: Presence::Invisible, .. } if *user_id == ana.user.id)));
    drop(ana3_ws);

    // Disconnecting while invisible is silent: she was already gone.
    drop(ana_ws);
    drop(ana2_ws);
    assert!(bia_ws.quiet_for(Duration::from_millis(600), |e| is_about(e, ana.user.id)).await);
    // Coming back invisible (the choice is stored) announces nothing.
    let (_back, online) = env.ws_ready(&ana).await;
    assert!(online.iter().all(|u| u.id != ana.user.id));
    assert!(bia_ws.quiet_for(Duration::from_millis(400), |e| is_about(e, ana.user.id)).await);

    // Becoming visible again is a normal arrival, with her status.
    assert_eq!(env.presence(&ana, "online", false).await, Presence::Online);
    bia_ws.wait_for("ana online", |e| matches!(e, ServerEvent::UserOnline { user } if user.id == ana.user.id)).await;
    let st = bia_ws.presence_of(ana.user.id, Presence::Online).await;
    assert_eq!(st.map(|s| s.text), Some("escondida".to_string()));
    let seen = env.profile_of(&bia, ana.user.id).await;
    assert_eq!((seen.online, seen.presence), (true, Presence::Online));
    // Invisible is not something idle detection can set or undo.
    env.presence(&ana, "invisible", false).await;
    bia_ws.wait_for("ana offline", |e| matches!(e, ServerEvent::UserOffline { user_id } if *user_id == ana.user.id)).await;
    assert_eq!(env.presence(&ana, "online", true).await, Presence::Invisible);
    assert_eq!(env.presence(&ana, "away", true).await, Presence::Invisible);
    assert!(bia_ws.quiet_for(Duration::from_millis(300), |e| is_about(e, ana.user.id)).await);
    env.server.shutdown().await;
}

#[tokio::test]
async fn an_invisible_person_in_voice_stays_in_the_room_list() {
    // Invisible hides the online dot, not the fact that someone is in a voice room (the room
    // shows who is in it, and the person chose to join).
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    let (mut bia_ws, _) = env.ws_ready(&bia).await;
    let (mut ana_ws, _) = env.ws_ready(&ana).await;
    let chs: Vec<Channel> = env.http.get(env.url("/api/channels")).bearer_auth(&ana.token).send().await.unwrap().json().await.unwrap();
    let voice = chs.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    env.presence(&ana, "invisible", false).await;
    bia_ws.wait_for("ana offline", |e| matches!(e, ServerEvent::UserOffline { user_id } if *user_id == ana.user.id)).await;
    let ev = ClientEvent::VoiceState { channel_id: Some(voice), muted: false, deafened: false, streaming: false, camera: false, audio_only: false };
    ana_ws.0.send(WsMsg::Text(serde_json::to_string(&ev).unwrap().into())).await.unwrap();
    let ServerEvent::VoiceStateChanged { member } = bia_ws.wait_for("voice", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == ana.user.id)).await else { unreachable!() };
    assert_eq!(member.channel_id, Some(voice));
    env.server.shutdown().await;
}

#[tokio::test]
async fn a_status_clears_itself_and_everybody_hears_it() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    let (mut bia_ws, _) = env.ws_ready(&bia).await;
    let (_ana_ws, _) = env.ws_ready(&ana).await;
    env.put(&ana, "/api/me/status", json!({"emoji": "⏳", "text": "volto já", "expires_in_secs": 1})).await;
    assert!(bia_ws.presence_of(ana.user.id, Presence::Online).await.is_some());
    let t = Instant::now();
    // The next PresenceChanged about Ana is the status going away.
    let st = bia_ws.presence_of(ana.user.id, Presence::Online).await;
    assert_eq!(st, None, "the status cleared itself");
    assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
    assert_eq!(env.profile_of(&bia, ana.user.id).await.status, None);
    // Offline people's expired statuses are not served either.
    env.put(&bia, "/api/me/status", json!({"text": "efêmero", "expires_in_secs": 1})).await;
    drop(bia_ws);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(env.profile_of(&ana, bia.user.id).await.status, None);
    env.server.shutdown().await;
}

#[tokio::test]
async fn edits_are_rate_limited_per_person() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    for i in 0..PROFILE_RATE_LIMIT {
        assert_eq!(env.edit(&ana, json!({"bio": format!("v{i}")})).await.status(), StatusCode::OK);
    }
    let (status, msg) = error_of(env.edit(&ana, json!({"bio": "demais"})).await).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(msg.contains("wait"), "{msg}");
    assert_eq!(env.put(&ana, "/api/me/status", json!({"text": "x"})).await.status(), StatusCode::TOO_MANY_REQUESTS, "statuses share the edit limit");
    // Somebody else is unaffected; a refused (invalid) edit costs nothing; presence has its own limit.
    assert_eq!(env.edit(&bia, json!({"bio": "ok"})).await.status(), StatusCode::OK);
    assert_eq!(env.edit(&bia, json!({"accent": "nope"})).await.status(), StatusCode::BAD_REQUEST);
    for i in 0..PRESENCE_RATE_LIMIT {
        let p = if i % 2 == 0 { "away" } else { "online" };
        assert_eq!(env.put(&ana, "/api/me/presence", json!({"presence": p})).await.status(), StatusCode::OK, "{i}");
    }
    assert_eq!(env.put(&ana, "/api/me/presence", json!({"presence": "dnd"})).await.status(), StatusCode::TOO_MANY_REQUESTS);
    env.server.shutdown().await;
}

#[tokio::test]
async fn moderators_clear_a_profile_and_it_is_audited() {
    let env = setup().await;
    let (owner, moder, admin) = env.staff().await;
    let mem = env.who("mem").await;
    let other = env.who("outra").await;
    let (mut watcher, _) = env.ws_ready(&other).await;
    let (_mem_ws, _) = env.ws_ready(&mem).await;
    env.edit(&mem, json!({"display_name": "Nome Abusivo", "bio": "texto abusivo aqui", "accent": "#ff0000"})).await;
    env.put(&mem, "/api/me/status", json!({"emoji": "💢", "text": "status abusivo"})).await;
    watcher.presence_of(mem.user.id, Presence::Online).await;
    let path = |id: i64| format!("/api/mod/users/{id}/clear-profile");

    // Who may: not a Membro, not a moderator over a peer or above, not on yourself, nothing to clear.
    assert_eq!(env.post(&other, &path(mem.user.id), json!({"bio": true})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.post(&moder, &path(admin.user.id), json!({"bio": true})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.post(&moder, &path(moder.user.id), json!({"bio": true})).await, StatusCode::BAD_REQUEST);
    assert_eq!(env.post(&moder, &path(9999), json!({"bio": true})).await, StatusCode::NOT_FOUND);
    assert_eq!(env.post(&moder, &path(mem.user.id), json!({})).await, StatusCode::BAD_REQUEST);
    assert_eq!(env.profile_of(&other, mem.user.id).await.bio, "texto abusivo aqui", "nothing changed yet");

    // The bio and the status go; the display name and the colour stay; the status removal is broadcast.
    assert_eq!(env.post(&moder, &path(mem.user.id), json!({"bio": true, "status": true})).await, StatusCode::OK);
    let p = env.profile_of(&other, mem.user.id).await;
    assert_eq!((p.bio.as_str(), p.status.clone(), p.user.display_name.as_deref(), p.accent.as_deref()), ("", None, Some("Nome Abusivo"), Some("#ff0000")));
    assert_eq!(watcher.presence_of(mem.user.id, Presence::Online).await, None);
    // The display name is clearable too, and broadcast as a user update.
    assert_eq!(env.post(&admin, &path(mem.user.id), json!({"display_name": true})).await, StatusCode::OK);
    watcher.wait_for("user_updated", |e| matches!(e, ServerEvent::UserUpdated { user } if user.id == mem.user.id && user.display_name.is_none())).await;
    assert_eq!(env.profile_of(&other, mem.user.id).await.user.display_name, None);
    // The owner clears an admin's.
    env.edit(&admin, json!({"bio": "bio do admin"})).await;
    assert_eq!(env.post(&owner, &path(admin.user.id), json!({"bio": true})).await, StatusCode::OK);
    assert_eq!(env.profile_of(&owner, admin.user.id).await.bio, "");

    // The person can write again afterwards; the log says who removed what.
    assert_eq!(env.edit(&mem, json!({"bio": "novo"})).await.status(), StatusCode::OK);
    let log: Vec<AuditEntry> = env.http.get(env.url("/api/admin/audit")).bearer_auth(&owner.token).send().await.unwrap().json().await.unwrap();
    let entries: Vec<(&str, &str, &str)> = log
        .iter()
        .filter(|e| e.action == AUDIT_CLEAR_PROFILE)
        .map(|e| (e.actor.as_ref().unwrap().username.as_str(), e.target.as_ref().unwrap().username.as_str(), e.detail.as_str()))
        .collect();
    assert_eq!(
        entries,
        [
            ("Dono", "adm", "bio: bio do admin"),
            ("adm", "mem", "display_name: Nome Abusivo"),
            ("modi", "mem", "bio: texto abusivo aqui | status: 💢 status abusivo"),
        ],
        "newest first, with what was removed"
    );
    env.server.shutdown().await;
}
