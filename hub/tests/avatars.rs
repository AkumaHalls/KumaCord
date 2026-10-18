//! Profile pictures end-to-end: upload validation, the stored sizes, serving and cache headers,
//! `UserUpdated` broadcasts, removal by the person and by a moderator (and its denial), the rate
//! limit, and the migration of a database written by the real v0.4.1 hub.
//! Real server on an ephemeral port, no LiveKit.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
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

fn write_config(dir: &Path, port: Option<u16>) {
    let bind = port.map(|p| format!("bind = \"127.0.0.1:{p}\"\n")).unwrap_or_default();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "server_name = \"avatars\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n{bind}\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
}

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), None);
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

    async fn upload(&self, who: &Who, bytes: Vec<u8>) -> reqwest::Response {
        self.http.put(self.url("/api/me/avatar")).bearer_auth(&who.token).body(bytes).send().await.unwrap()
    }

    async fn upload_ok(&self, who: &Who, bytes: Vec<u8>) -> String {
        let r = self.upload(who, bytes).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json::<AvatarResponse>().await.unwrap().avatar.expect("a version")
    }

    async fn remove_own(&self, who: &Who) -> reqwest::Response {
        self.http.delete(self.url("/api/me/avatar")).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn get_avatar(&self, who: &Who, id: i64, query: &str) -> reqwest::Response {
        self.http.get(self.url(&format!("/api/users/{id}/avatar{query}"))).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> StatusCode {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap().status()
    }

    async fn ws(&self, who: &Who) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        Ws(ws)
    }

    async fn first_text_channel(&self, who: &Who) -> i64 {
        let chs: Vec<Channel> = self.http.get(self.url("/api/channels")).bearer_auth(&who.token).send().await.unwrap().json().await.unwrap();
        chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id
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

    async fn updated(&mut self, id: i64, want: Option<&str>) -> User {
        let ev = self
            .wait_for("user_updated", |e| matches!(e, ServerEvent::UserUpdated { user } if user.id == id && user.avatar.as_deref() == want))
            .await;
        let ServerEvent::UserUpdated { user } = ev else { unreachable!() };
        user
    }
}

// ---------------------------------------------------------------- test images (generated)

fn encode(img: &DynamicImage, fmt: ImageFormat) -> Vec<u8> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), fmt).unwrap();
    out
}

/// A photo-ish gradient of any shape; `seed` makes different pictures.
fn picture(w: u32, h: u32, seed: u8) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| Rgb([(x % 256) as u8 ^ seed, (y % 256) as u8, seed.wrapping_mul(7)])))
}

fn png(w: u32, h: u32, seed: u8) -> Vec<u8> {
    encode(&picture(w, h, seed), ImageFormat::Png)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A PNG whose header claims `w`x`h` (a decompression bomb in spirit: tiny file, huge picture).
fn png_claiming(w: u32, h: u32) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([1, 0, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &[0x78, 0x9c, 0x01, 0x00, 0x00, 0xff, 0xff]);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn dims(bytes: &[u8]) -> (u32, u32) {
    let img = image::load_from_memory(bytes).unwrap();
    (img.width(), img.height())
}

async fn error_of(r: reqwest::Response) -> (StatusCode, String) {
    let s = r.status();
    (s, r.json::<ApiError>().await.map(|e| e.error).unwrap_or_default())
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn upload_resizes_serves_and_broadcasts() {
    let env = setup().await;
    let ana = env.who("ana").await;
    let bia = env.who("bia").await;
    assert_eq!(ana.user.avatar, None);
    let mut bia_ws = env.ws(&bia).await;
    bia_ws.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await;

    // A wide JPEG goes in (any format and shape), square 256 and 64 come out.
    let v1 = env.upload_ok(&ana, encode(&picture(900, 300, 1), ImageFormat::Jpeg)).await;
    assert_eq!(v1.len(), 12);
    let updated = bia_ws.updated(ana.user.id, Some(&v1)).await;
    assert_eq!(updated.username, "ana");

    let big = env.get_avatar(&bia, ana.user.id, "?size=256").await;
    assert_eq!(big.status(), StatusCode::OK);
    assert_eq!(big.headers()["content-type"], "image/jpeg");
    assert_eq!(big.headers()["x-content-type-options"], "nosniff");
    let big = big.bytes().await.unwrap();
    assert_eq!(dims(&big), (256, 256));
    assert!(big.len() < 100 * 1024, "{} bytes", big.len());
    let small = env.get_avatar(&bia, ana.user.id, "?size=64").await.bytes().await.unwrap();
    assert_eq!(dims(&small), (64, 64));
    // The default size is the big one.
    assert_eq!(dims(&env.get_avatar(&bia, ana.user.id, "").await.bytes().await.unwrap()), (256, 256));

    // Cache headers: keyed by the version, immutable; without it, revalidate.
    let r = env.get_avatar(&bia, ana.user.id, &format!("?size=64&v={v1}")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["cache-control"], "private, max-age=31536000, immutable");
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert!(etag.contains(&v1), "{etag}");
    let r = env.get_avatar(&bia, ana.user.id, "?size=64").await;
    assert_eq!(r.headers()["cache-control"], "private, no-cache");
    let r = env.http.get(env.url(&format!("/api/users/{}/avatar?size=64", ana.user.id))).bearer_auth(&bia.token).header("If-None-Match", &etag).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    assert!(r.bytes().await.unwrap().is_empty());

    // Edges of the GET: bad size, stale version, nobody, no session.
    assert_eq!(env.get_avatar(&bia, ana.user.id, "?size=100").await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.get_avatar(&bia, ana.user.id, "?size=64&v=deadbeef0000").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.get_avatar(&ana, bia.user.id, "?size=64").await.status(), StatusCode::NOT_FOUND, "bia has no picture");
    assert_eq!(env.get_avatar(&ana, 9999, "").await.status(), StatusCode::NOT_FOUND);
    let anon = env.http.get(env.url(&format!("/api/users/{}/avatar", ana.user.id))).send().await.unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);

    // Everywhere the hub names Ana now carries the version: a fresh login, the snapshot, history.
    let again = env.who("ana").await;
    assert_eq!(again.user.avatar.as_deref(), Some(v1.as_str()));
    let mut ana_ws = env.ws(&again).await;
    let ServerEvent::Ready { user, .. } = ana_ws.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await else { unreachable!() };
    assert_eq!(user.avatar.as_deref(), Some(v1.as_str()));
    let mut bia_ws2 = env.ws(&bia).await;
    let ServerEvent::Ready { online, .. } = bia_ws2.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await else { unreachable!() };
    assert_eq!(online.iter().find(|u| u.id == ana.user.id).and_then(|u| u.avatar.clone()).as_deref(), Some(v1.as_str()));
    let ch = env.first_text_channel(&ana).await;
    ana_ws.0.send_text(&ClientEvent::SendMessage { channel_id: ch, content: "oi".into(), mentions: vec![], reply_to: None, reply_mention: false }).await;
    let ServerEvent::MessageCreated { message } = bia_ws.wait_for("message", |e| matches!(e, ServerEvent::MessageCreated { .. })).await else { unreachable!() };
    assert_eq!(message.author.avatar.as_deref(), Some(v1.as_str()), "live messages carry the picture");
    let hist: Vec<Message> = env.http.get(env.url(&format!("/api/channels/{ch}/messages"))).bearer_auth(&bia.token).send().await.unwrap().json().await.unwrap();
    assert_eq!(hist[0].author.avatar.as_deref(), Some(v1.as_str()), "and so does the history");

    // Another picture: a new version, broadcast again, and the old URL is gone.
    let v2 = env.upload_ok(&ana, png(400, 400, 9)).await;
    assert_ne!(v1, v2);
    bia_ws.updated(ana.user.id, Some(&v2)).await;
    assert_eq!(env.get_avatar(&bia, ana.user.id, &format!("?size=64&v={v1}")).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.get_avatar(&bia, ana.user.id, &format!("?size=64&v={v2}")).await.status(), StatusCode::OK);
    // The same picture again changes nothing and says nothing.
    assert_eq!(env.upload_ok(&ana, png(400, 400, 9)).await, v2);
    assert!(bia_ws.quiet_for(Duration::from_millis(400), |e| matches!(e, ServerEvent::UserUpdated { .. })).await);
    env.server.shutdown().await;
}

#[tokio::test]
async fn transparent_pictures_are_stored_as_png() {
    let env = setup().await;
    let ana = env.who("ana").await;
    let hole = DynamicImage::ImageRgba8(RgbaImage::from_fn(64, 64, |x, _| if x < 32 { Rgba([0, 0, 0, 0]) } else { Rgba([255, 0, 0, 255]) }));
    env.upload_ok(&ana, encode(&hole, ImageFormat::Png)).await;
    let r = env.get_avatar(&ana, ana.user.id, "?size=64").await;
    assert_eq!(r.headers()["content-type"], "image/png");
    let img = image::load_from_memory(&r.bytes().await.unwrap()).unwrap().to_rgba8();
    assert_eq!((img.get_pixel(3, 32)[3], img.get_pixel(60, 32)[3]), (0, 255));
    env.server.shutdown().await;
}

#[tokio::test]
async fn bad_uploads_are_refused_and_change_nothing() {
    let env = setup().await;
    let ana = env.who("ana").await;
    let mut ws = env.ws(&env.who("bia").await).await;
    ws.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await;

    let mut cut = png(800, 600, 3);
    cut.truncate(cut.len() / 2);
    // 5 MB + 1: refused by size before anything is decoded.
    let mut huge = png(64, 64, 1);
    huge.resize(AVATAR_MAX_UPLOAD_BYTES + 1, 0);
    let cases: Vec<(&str, Vec<u8>, StatusCode)> = vec![
        ("not an image", b"<html><script>alert(1)</script></html>".to_vec(), StatusCode::BAD_REQUEST),
        ("an executable", b"MZ\x90\x00\x03\x00\x00\x00 not a picture".to_vec(), StatusCode::BAD_REQUEST),
        ("empty", Vec::new(), StatusCode::BAD_REQUEST),
        ("truncated", cut, StatusCode::BAD_REQUEST),
        ("bomb 20000x20000", png_claiming(20_000, 20_000), StatusCode::BAD_REQUEST),
        ("bomb 9000x9000", png_claiming(9_000, 9_000), StatusCode::BAD_REQUEST),
        ("over 5 MB", huge, StatusCode::PAYLOAD_TOO_LARGE),
    ];
    for (what, bytes, want) in cases {
        let (status, msg) = error_of(env.upload(&ana, bytes).await).await;
        assert_eq!(status, want, "{what}: {msg}");
        assert!(!msg.is_empty(), "{what} has a message");
    }
    // Exactly at the limit is not "too large" (it is just not an image).
    let mut limit = png(64, 64, 1);
    limit.resize(AVATAR_MAX_UPLOAD_BYTES, 0);
    assert_ne!(env.upload(&ana, limit).await.status(), StatusCode::PAYLOAD_TOO_LARGE);

    assert_eq!(env.get_avatar(&ana, ana.user.id, "").await.status(), StatusCode::NOT_FOUND, "nothing was stored");
    assert!(ws.quiet_for(Duration::from_millis(300), |e| matches!(e, ServerEvent::UserUpdated { .. })).await, "nothing was broadcast");
    // No session, no upload.
    let r = env.http.put(env.url("/api/me/avatar")).body(png(64, 64, 1)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    env.server.shutdown().await;
}

#[tokio::test]
async fn removal_by_the_person_and_by_a_moderator() {
    let env = setup().await;
    // Staff needs personal passwords: the owner, a moderator and an admin.
    let owner = env.who("Dono").await;
    for (w, pw) in [(&owner, "dono-senha")] {
        let r = env.http.post(env.url("/api/account/password")).bearer_auth(&w.token).json(&json!({"new": pw})).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    let moder = env.who("modi").await;
    let admin = env.who("adm").await;
    for (w, pw) in [(&moder, "mod-senha"), (&admin, "adm-senha")] {
        let r = env.http.post(env.url("/api/account/password")).bearer_auth(&w.token).json(&json!({"new": pw})).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    assert_eq!(env.post(&owner, &format!("/api/admin/users/{}/role", moder.user.id), json!({"role": "moderator"})).await, StatusCode::OK);
    assert_eq!(env.post(&owner, &format!("/api/admin/users/{}/role", admin.user.id), json!({"role": "admin"})).await, StatusCode::OK);
    let mem = env.who("mem").await;
    let mem2 = env.who("mem2").await;
    let mut watcher = env.ws(&mem2).await;
    watcher.wait_for("ready", |e| matches!(e, ServerEvent::Ready { .. })).await;

    // Self-removal: idempotent, broadcast once, the picture is gone.
    let v = env.upload_ok(&mem, png(200, 200, 4)).await;
    watcher.updated(mem.user.id, Some(&v)).await;
    let r = env.remove_own(&mem).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<AvatarResponse>().await.unwrap().avatar, None);
    watcher.updated(mem.user.id, None).await;
    assert_eq!(env.get_avatar(&mem2, mem.user.id, "").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.remove_own(&mem).await.status(), StatusCode::OK, "removing nothing is fine");
    assert!(watcher.quiet_for(Duration::from_millis(300), |e| matches!(e, ServerEvent::UserUpdated { .. })).await);

    // A Membro cannot remove somebody else's picture; neither can a moderator remove a peer's
    // or a higher role's.
    let path = |id: i64| format!("/api/mod/users/{id}/remove-avatar");
    let v = env.upload_ok(&mem, png(200, 200, 5)).await;
    watcher.updated(mem.user.id, Some(&v)).await;
    env.upload_ok(&admin, png(200, 200, 6)).await;
    assert_eq!(env.post(&mem2, &path(mem.user.id), json!({})).await, StatusCode::FORBIDDEN, "a Membro is denied");
    assert_eq!(env.get_avatar(&mem2, mem.user.id, "").await.status(), StatusCode::OK, "and nothing changed");
    assert_eq!(env.post(&moder, &path(admin.user.id), json!({})).await, StatusCode::FORBIDDEN, "not above an Admin");
    assert_eq!(env.post(&moder, &path(moder.user.id), json!({})).await, StatusCode::BAD_REQUEST, "not on yourself: use Remover");
    assert_eq!(env.post(&moder, &path(9999), json!({})).await, StatusCode::NOT_FOUND);
    assert_eq!(env.post(&moder, &path(mem2.user.id), json!({})).await, StatusCode::NOT_FOUND, "no picture to remove");

    // The moderator removes the Membro's: broadcast, gone, audited.
    assert_eq!(env.post(&moder, &path(mem.user.id), json!({})).await, StatusCode::OK);
    watcher.updated(mem.user.id, None).await;
    assert_eq!(env.get_avatar(&mem2, mem.user.id, "").await.status(), StatusCode::NOT_FOUND);
    // The admin may remove the moderator's and the owner everybody's.
    env.upload_ok(&moder, png(100, 100, 8)).await;
    assert_eq!(env.post(&admin, &path(moder.user.id), json!({})).await, StatusCode::OK);
    assert_eq!(env.post(&owner, &path(admin.user.id), json!({})).await, StatusCode::OK);

    let log: Vec<AuditEntry> = env.http.get(env.url("/api/admin/audit")).bearer_auth(&owner.token).send().await.unwrap().json().await.unwrap();
    let removals: Vec<(&str, &str)> = log
        .iter()
        .filter(|e| e.action == AUDIT_REMOVE_AVATAR)
        .map(|e| (e.actor.as_ref().unwrap().username.as_str(), e.target.as_ref().unwrap().username.as_str()))
        .collect();
    assert_eq!(removals, [("Dono", "adm"), ("adm", "modi"), ("modi", "mem")], "newest first");
    assert!(log.iter().find(|e| e.action == AUDIT_REMOVE_AVATAR).is_some_and(|e| e.detail.len() == 12), "the audit keeps the removed version");
    // A Membro and a moderator cannot read the log (item 12), so nobody learns more than before.
    env.server.shutdown().await;
}

#[tokio::test]
async fn picture_changes_are_rate_limited_per_person() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    for i in 0..AVATAR_RATE_LIMIT as u8 {
        env.upload_ok(&ana, png(80, 80, i)).await;
    }
    let (status, msg) = error_of(env.upload(&ana, png(80, 80, 99)).await).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(msg.contains("wait"), "{msg}");
    // Somebody else is unaffected, and removing a picture is never blocked.
    env.upload_ok(&bia, png(80, 80, 1)).await;
    assert_eq!(env.remove_own(&ana).await.status(), StatusCode::OK);
    assert_eq!(env.upload(&ana, png(80, 80, 98)).await.status(), StatusCode::TOO_MANY_REQUESTS);
    // Refused files count too (they cost the host decoding time).
    let bia2 = env.who("bia2").await;
    for _ in 0..AVATAR_RATE_LIMIT {
        assert_eq!(env.upload(&bia2, b"junk".to_vec()).await.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(env.upload(&bia2, png(80, 80, 1)).await.status(), StatusCode::TOO_MANY_REQUESTS);
    env.server.shutdown().await;
}

#[tokio::test]
async fn pictures_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), None);
    let opts = || Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) };
    let server = start(opts()).await.unwrap();
    let http = reqwest::Client::new(); // after `start`, which installs the TLS provider
    let base = format!("http://127.0.0.1:{}", server.addr.port());
    let login = |name: &str| {
        let req = LoginRequest { username: name.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        http.post(format!("{base}/api/login")).json(&req).send()
    };
    let ana: LoginResponse = login("ana").await.unwrap().json().await.unwrap();
    let r = http.put(format!("{base}/api/me/avatar")).bearer_auth(&ana.token).body(png(300, 200, 2)).send().await.unwrap();
    let v = r.json::<AvatarResponse>().await.unwrap().avatar.unwrap();
    server.shutdown().await;

    let server = start(opts()).await.unwrap();
    let base = format!("http://127.0.0.1:{}", server.addr.port());
    let again: LoginResponse = http
        .post(format!("{base}/api/login"))
        .json(&LoginRequest { username: "ana".into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again.user.avatar.as_deref(), Some(v.as_str()));
    let r = http.get(format!("{base}/api/users/{}/avatar?size=256&v={v}", ana.user.id)).bearer_auth(&again.token).send().await.unwrap();
    assert_eq!(dims(&r.bytes().await.unwrap()), (256, 256));
    server.shutdown().await;
}

// ---------------------------------------------------------------- migration from the real v0.4.1 hub

fn old_hub() -> Option<PathBuf> {
    let p = std::env::var_os("VOX_V041_HUB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../releases/v0.4.1/voxhub-server.exe"));
    p.exists().then_some(p)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct OldHub(Child);

impl Drop for OldHub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn run_old(bin: &Path, dir: &Path, port: u16) -> OldHub {
    let child = Command::new(bin)
        .arg("--data-dir")
        .arg(dir)
        .arg("--no-livekit")
        .arg("--log-dir")
        .arg(dir.join("logs"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let hub = OldHub(child);
    let t = Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        assert!(t.elapsed() < Duration::from_secs(15), "the v0.4.1 hub did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    hub
}

/// The v0.4.1 wire shape of a login answer: no `avatar`, no `role`.
async fn old_login(http: &reqwest::Client, port: u16, name: &str) -> Value {
    let r = http
        .post(format!("http://127.0.0.1:{port}/api/login"))
        .json(&json!({"username": name, "server_password": PASSWORD, "protocol_version": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    r.json().await.unwrap()
}

async fn old_say(port: u16, token: &str, channel: i64, text: &str) {
    use futures_util::SinkExt;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws?token={token}")).await.unwrap();
    ws.send(WsMsg::Text(json!({"type": "send_message", "channel_id": channel, "content": text}).to_string().into())).await.unwrap();
    loop {
        let Some(Ok(WsMsg::Text(t))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap() else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "message_created" && v["message"]["content"] == text {
            return;
        }
    }
}

#[tokio::test]
async fn a_v041_database_gets_pictures_and_still_runs_the_old_hub() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.4.1 hub binary (set VOX_V041_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v041"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- the v0.4.1 hub writes its database
    let port = free_port();
    write_config(&old_dir_create(&old_dir), Some(port));
    let old = run_old(&bin, &old_dir, port).await;
    let ana = old_login(&http, port, "Ana").await;
    let bruno = old_login(&http, port, "Bruno").await;
    let chs: Vec<Channel> = http
        .get(format!("http://127.0.0.1:{port}/api/channels"))
        .bearer_auth(ana["token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    old_say(port, ana["token"].as_str().unwrap(), geral, "oi do v0.4.1").await;
    old_say(port, bruno["token"].as_str().unwrap(), geral, "olá").await;
    drop(old); // killed like a crash
    std::fs::create_dir_all(&new_dir).unwrap();
    for name in ["voxhub.db", "voxhub.db-wal", "voxhub.db-shm"] {
        if old_dir.join(name).exists() {
            std::fs::copy(old_dir.join(name), new_dir.join(name)).unwrap();
        }
    }

    // ---- the new hub opens the copy, twice
    let port = free_port();
    write_config(&new_dir, Some(port));
    let mut version = String::new();
    for round in 0..2 {
        let server = start(Options { data_dir: new_dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let login = |name: &str| {
            let req = LoginRequest { username: name.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
            http.post(format!("{base}/api/login")).json(&req).send()
        };
        let a: LoginResponse = login("Ana").await.unwrap().json().await.unwrap();
        assert_eq!(a.user.id, ana["user"]["id"].as_i64().unwrap(), "same user, same id");
        if round == 0 {
            assert_eq!(a.user.avatar, None, "no pictures in an old database");
            let r = http.put(format!("{base}/api/me/avatar")).bearer_auth(&a.token).body(png(500, 300, 3)).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            version = r.json::<AvatarResponse>().await.unwrap().avatar.unwrap();
        } else {
            assert_eq!(a.user.avatar.as_deref(), Some(version.as_str()), "the picture is still there after another start");
        }
        let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&a.token).send().await.unwrap().json().await.unwrap();
        let by: Vec<(&str, Option<&str>)> = msgs.iter().map(|m| (m.author.username.as_str(), m.author.avatar.as_deref())).collect();
        assert_eq!(by, [("Bruno", None), ("Ana", Some(version.as_str()))], "round {round}");
        server.shutdown().await;
    }

    // ---- downgrade: the v0.4.1 hub still runs on the migrated copy (it ignores the new column and table)
    let port = free_port();
    write_config(&new_dir, Some(port));
    let old = run_old(&bin, &new_dir, port).await;
    let carla = old_login(&http, port, "Carla").await;
    let msgs: Vec<Value> = http
        .get(format!("http://127.0.0.1:{port}/api/channels/{geral}/messages"))
        .bearer_auth(carla["token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(msgs.len(), 2);
    old_say(port, carla["token"].as_str().unwrap(), geral, "de volta ao v0.4.1").await;
    drop(old);
}

fn old_dir_create(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    dir.to_path_buf()
}

/// `send_text` for the raw socket, to keep the tests above short.
trait SendText {
    async fn send_text(&mut self, ev: &ClientEvent);
}

impl SendText for tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    async fn send_text(&mut self, ev: &ClientEvent) {
        use futures_util::SinkExt;
        self.send(WsMsg::Text(serde_json::to_string(ev).unwrap().into())).await.unwrap();
    }
}
