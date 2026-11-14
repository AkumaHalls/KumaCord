//! Profile banners end-to-end: upload validation, the stored box, serving and cache headers,
//! `UserUpdated` broadcasts, the animation path (`?anim=1`), removal by the person and by a
//! moderator (and its denial), and the rate limit.
//! Real server on an ephemeral port, no LiveKit.

use std::io::Cursor;
use std::path::Path;

use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::{AnimationDecoder, DynamicImage, Frame, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use reqwest::StatusCode;
use serde_json::{json, Value};
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
            "server_name = \"banners\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n{bind}\
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
        self.http.put(self.url("/api/me/banner")).bearer_auth(&who.token).body(bytes).send().await.unwrap()
    }

    async fn upload_ok(&self, who: &Who, bytes: Vec<u8>) -> String {
        let r = self.upload(who, bytes).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json::<BannerResponse>().await.unwrap().banner.expect("a version")
    }

    async fn remove_own(&self, who: &Who) -> reqwest::Response {
        self.http.delete(self.url("/api/me/banner")).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn get_banner(&self, who: &Who, id: UserId, query: &str) -> reqwest::Response {
        self.http.get(self.url(&format!("/api/users/{id}/banner{query}"))).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> StatusCode {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap().status()
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

/// An animated GIF of `frames` colours, 64x64, looping.
fn gif(colours: &[Rgba<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut out);
        enc.set_repeat(Repeat::Infinite).unwrap();
        for c in colours {
            enc.encode_frame(Frame::new(RgbaImage::from_pixel(64, 64, *c))).unwrap();
        }
    }
    out
}

fn frames_of(bytes: &[u8]) -> usize {
    GifDecoder::new(Cursor::new(bytes)).unwrap().into_frames().collect_frames().unwrap().len()
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
async fn upload_fits_the_box_serves_and_broadcasts() {
    let env = setup().await;
    let ana = env.who("ana").await;
    let bia = env.who("bia").await;
    assert_eq!(ana.user.banner, None);
    assert!(!ana.user.banner_animated);

    // A 16:9 photo is height-bound inside the box: 240 tall, the width scaled to keep the aspect
    // (nothing cropped away, unlike a square avatar).
    let v1 = env.upload_ok(&ana, encode(&picture(1600, 900, 1), ImageFormat::Jpeg)).await;
    assert_eq!(v1.len(), 12);
    let r = env.get_banner(&bia, ana.user.id, &format!("?v={v1}")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "image/jpeg");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    let bytes = r.bytes().await.unwrap();
    assert_eq!(dims(&bytes), (427, 240));
    assert!(bytes.len() < 200 * 1024, "{} bytes", bytes.len());

    // A tall phone shot keeps its aspect inside the same box: 240 tall again, 120 wide.
    let v2 = env.upload_ok(&ana, encode(&picture(600, 1200, 2), ImageFormat::Jpeg)).await;
    assert_ne!(v1, v2);
    let r = env.get_banner(&bia, ana.user.id, &format!("?v={v2}")).await;
    assert_eq!(dims(&r.bytes().await.unwrap()), (120, 240));

    // The version rides every `User`: a fresh login says so.
    let again = env.who("ana").await;
    assert_eq!(again.user.banner.as_deref(), Some(v2.as_str()));
    assert!(!again.user.banner_animated, "a JPEG is a still");

    // Cache headers: keyed by the version, immutable; without it, revalidate.
    let r = env.get_banner(&bia, ana.user.id, &format!("?v={v2}")).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["cache-control"], "private, max-age=31536000, immutable");
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert!(etag.contains(&v2), "{etag}");
    assert_eq!(env.get_banner(&bia, ana.user.id, "").await.headers()["cache-control"], "private, no-cache");
    let r = env.http.get(env.url(&format!("/api/users/{}/banner", ana.user.id))).bearer_auth(&bia.token).header("If-None-Match", &etag).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    assert!(r.bytes().await.unwrap().is_empty());

    // Edges of the GET: stale version, nobody, no session.
    assert_eq!(env.get_banner(&bia, ana.user.id, "?v=deadbeef0000").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.get_banner(&bia, bia.user.id, "").await.status(), StatusCode::NOT_FOUND, "bia has no banner");
    assert_eq!(env.get_banner(&ana, UserId(9999), "").await.status(), StatusCode::NOT_FOUND);
    let anon = env.http.get(env.url(&format!("/api/users/{}/banner", ana.user.id))).send().await.unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
    env.server.shutdown().await;
}

#[tokio::test]
async fn an_animated_banner_is_served_as_an_animation_and_a_still_is_not() {
    let env = setup().await;
    let ana = env.who("ana").await;
    let bia = env.who("bia").await;

    // A still first: `?anim=1` is the client's "is there an animation?" question, and 404 is the
    // honest answer (that is how it knows to draw the still).
    let still = env.upload_ok(&ana, png(600, 240, 3)).await;
    let r = env.get_banner(&bia, ana.user.id, "?anim=1").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // Now a 3-frame GIF.
    let v = env.upload_ok(&ana, gif(&[Rgba([255, 0, 0, 255]), Rgba([0, 0, 255, 255]), Rgba([0, 255, 0, 255])])).await;
    assert_ne!(still, v);
    assert!(env.who("ana").await.user.banner_animated, "the flag rides the wire");

    let anim = env.get_banner(&bia, ana.user.id, "?anim=1").await;
    assert_eq!(anim.status(), StatusCode::OK);
    assert_eq!(anim.headers()["content-type"], "image/gif");
    assert_eq!(anim.headers()["cache-control"], "private, no-cache");
    let anim_bytes = anim.bytes().await.unwrap();
    assert_eq!(&anim_bytes[..3], b"GIF");
    assert_eq!(frames_of(&anim_bytes), 3, "every frame survives the cleaning");

    // The still is still there, first frame (red), height-bound inside the box. Its container is
    // the re-encoded one (JPEG for an opaque frame), not the GIF: the animation is the `?anim=1`
    // answer, and the still is what everybody who cannot play it downloads.
    let r = env.get_banner(&bia, ana.user.id, "").await;
    assert_eq!(r.headers()["content-type"], "image/jpeg");
    let still_bytes = r.bytes().await.unwrap();
    assert_eq!(dims(&still_bytes), (240, 240));
    let px = *image::load_from_memory(&still_bytes).unwrap().to_rgb8().get_pixel(120, 120);
    assert!(px[0] > 200 && px[2] < 60, "first frame (red), got {px:?}");

    // The still and the animation never share an ETag.
    let (still_etag, anim_etag) = (
        env.get_banner(&bia, ana.user.id, "").await.headers()["etag"].to_str().unwrap().to_string(),
        env.get_banner(&bia, ana.user.id, "?anim=1").await.headers()["etag"].to_str().unwrap().to_string(),
    );
    assert_ne!(still_etag, anim_etag);
    // A stale animation URL is gone, not the still.
    assert_eq!(env.get_banner(&bia, ana.user.id, "?anim=1&v=deadbeef0000").await.status(), StatusCode::NOT_FOUND);

    // Removing it takes the animation with it.
    assert_eq!(env.remove_own(&ana).await.status(), StatusCode::OK);
    assert_eq!(env.get_banner(&bia, ana.user.id, "?anim=1").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.get_banner(&bia, ana.user.id, "").await.status(), StatusCode::NOT_FOUND);
    env.server.shutdown().await;
}

#[tokio::test]
async fn a_single_frame_gif_is_a_still() {
    let env = setup().await;
    let ana = env.who("ana").await;
    let bia = env.who("bia").await;
    env.upload_ok(&ana, gif(&[Rgba([9, 9, 9, 255])])).await;
    assert!(!env.who("ana").await.user.banner_animated, "one frame is a still whatever the container says");
    assert_eq!(env.get_banner(&bia, ana.user.id, "?anim=1").await.status(), StatusCode::NOT_FOUND);
    let r = env.get_banner(&bia, ana.user.id, "").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(dims(&r.bytes().await.unwrap()), (240, 240));
    env.server.shutdown().await;
}

#[tokio::test]
async fn bad_uploads_are_refused_and_change_nothing() {
    let env = setup().await;
    let ana = env.who("ana").await;
    let mut cut = png(800, 600, 3);
    cut.truncate(cut.len() / 2);
    let mut huge = png(64, 64, 1);
    huge.resize(BANNER_MAX_UPLOAD_BYTES + 1, 0);
    let cases: Vec<(&str, Vec<u8>, StatusCode)> = vec![
        ("not an image", b"<html><script>alert(1)</script></html>".to_vec(), StatusCode::BAD_REQUEST),
        ("an executable", b"MZ\x90\x00\x03\x00\x00\x00 not a picture".to_vec(), StatusCode::BAD_REQUEST),
        ("empty", Vec::new(), StatusCode::BAD_REQUEST),
        ("truncated", cut, StatusCode::BAD_REQUEST),
        ("over the limit", huge, StatusCode::PAYLOAD_TOO_LARGE),
    ];
    for (what, bytes, want) in cases {
        let (status, msg) = error_of(env.upload(&ana, bytes).await).await;
        assert_eq!(status, want, "{what}: {msg}");
        assert!(!msg.is_empty(), "{what} has a message");
    }
    assert_eq!(env.get_banner(&ana, ana.user.id, "").await.status(), StatusCode::NOT_FOUND, "nothing was stored");
    let r = env.http.put(env.url("/api/me/banner")).body(png(64, 64, 1)).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    env.server.shutdown().await;
}

#[tokio::test]
async fn removal_by_the_person_and_by_a_moderator() {
    let env = setup().await;
    // Staff needs personal passwords: the owner, a moderator and an admin.
    let owner = env.who("Dono").await;
    let moder = env.who("modi").await;
    let admin = env.who("adm").await;
    for (w, pw) in [(&owner, "dono-senha"), (&moder, "mod-senha"), (&admin, "adm-senha")] {
        let r = env.http.post(env.url("/api/account/password")).bearer_auth(&w.token).json(&json!({"new": pw})).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    assert_eq!(env.post(&owner, &format!("/api/admin/users/{}/role", moder.user.id), json!({"role": "moderator"})).await, StatusCode::OK);
    assert_eq!(env.post(&owner, &format!("/api/admin/users/{}/role", admin.user.id), json!({"role": "admin"})).await, StatusCode::OK);
    let mem = env.who("mem").await;
    let mem2 = env.who("mem2").await;

    // Self-removal: idempotent, the banner is gone.
    env.upload_ok(&mem, png(600, 240, 4)).await;
    let r = env.remove_own(&mem).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<BannerResponse>().await.unwrap().banner, None);
    assert_eq!(env.get_banner(&mem2, mem.user.id, "").await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.remove_own(&mem).await.status(), StatusCode::OK, "removing nothing is fine");

    // A Membro cannot remove somebody else's banner; neither can a moderator remove a peer's or a
    // higher role's.
    let path = |id: UserId| format!("/api/mod/users/{id}/remove-banner");
    env.upload_ok(&mem, png(600, 240, 5)).await;
    env.upload_ok(&admin, png(600, 240, 6)).await;
    assert_eq!(env.post(&mem2, &path(mem.user.id), json!({})).await, StatusCode::FORBIDDEN, "a Membro is denied");
    assert_eq!(env.get_banner(&mem2, mem.user.id, "").await.status(), StatusCode::OK, "and nothing changed");
    assert_eq!(env.post(&moder, &path(admin.user.id), json!({})).await, StatusCode::FORBIDDEN, "not above an Admin");
    assert_eq!(env.post(&moder, &path(moder.user.id), json!({})).await, StatusCode::BAD_REQUEST, "not on yourself: use Remover");
    assert_eq!(env.post(&moder, &path(UserId(9999)), json!({})).await, StatusCode::NOT_FOUND);
    assert_eq!(env.post(&moder, &path(mem2.user.id), json!({})).await, StatusCode::NOT_FOUND, "no banner to remove");

    // The moderator removes the Membro's: gone, audited.
    assert_eq!(env.post(&moder, &path(mem.user.id), json!({})).await, StatusCode::OK);
    assert_eq!(env.get_banner(&mem2, mem.user.id, "").await.status(), StatusCode::NOT_FOUND);
    // The admin may remove the moderator's and the owner everybody's.
    env.upload_ok(&moder, png(600, 240, 8)).await;
    assert_eq!(env.post(&admin, &path(moder.user.id), json!({})).await, StatusCode::OK);
    assert_eq!(env.post(&owner, &path(admin.user.id), json!({})).await, StatusCode::OK);

    let log: Vec<AuditEntry> = env.http.get(env.url("/api/admin/audit")).bearer_auth(&owner.token).send().await.unwrap().json().await.unwrap();
    let removals: Vec<(&str, &str)> = log
        .iter()
        .filter(|e| e.action == AUDIT_REMOVE_BANNER)
        .map(|e| (e.actor.as_ref().unwrap().username.as_str(), e.target.as_ref().unwrap().username.as_str()))
        .collect();
    assert_eq!(removals, [("Dono", "adm"), ("adm", "modi"), ("modi", "mem")], "newest first");
    assert!(log.iter().find(|e| e.action == AUDIT_REMOVE_BANNER).is_some_and(|e| e.detail.len() == 12), "the audit keeps the removed version");
    env.server.shutdown().await;
}

#[tokio::test]
async fn banner_changes_are_rate_limited_per_person() {
    let env = setup().await;
    let (ana, bia) = (env.who("ana").await, env.who("bia").await);
    for i in 0..BANNER_RATE_LIMIT as u8 {
        env.upload_ok(&ana, png(600, 240, i)).await;
    }
    let (status, msg) = error_of(env.upload(&ana, png(600, 240, 99)).await).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(msg.contains("wait"), "{msg}");
    // Somebody else is unaffected, and removing a banner is never blocked.
    env.upload_ok(&bia, png(600, 240, 1)).await;
    assert_eq!(env.remove_own(&ana).await.status(), StatusCode::OK);
    assert_eq!(env.upload(&ana, png(600, 240, 98)).await.status(), StatusCode::TOO_MANY_REQUESTS);
    env.server.shutdown().await;
}

#[tokio::test]
async fn banners_survive_a_restart() {
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
    let r = http.put(format!("{base}/api/me/banner")).bearer_auth(&ana.token).body(gif(&[Rgba([255, 0, 0, 255]), Rgba([0, 0, 255, 255])])).send().await.unwrap();
    let v = r.json::<BannerResponse>().await.unwrap().banner.unwrap();
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
    assert_eq!(again.user.banner.as_deref(), Some(v.as_str()));
    assert!(again.user.banner_animated, "the animation flag survives too");
    let r = http.get(format!("{base}/api/users/{}/banner?v={v}", ana.user.id)).bearer_auth(&again.token).send().await.unwrap();
    assert_eq!(dims(&r.bytes().await.unwrap()), (240, 240));
    let r = http.get(format!("{base}/api/users/{}/banner?anim=1&v={v}", ana.user.id)).bearer_auth(&again.token).send().await.unwrap();
    assert_eq!(frames_of(&r.bytes().await.unwrap()), 2);
    server.shutdown().await;
}
