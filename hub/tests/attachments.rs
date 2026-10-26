//! Chat images end to end: a real hub on an ephemeral port. Upload checks (junk, bombs, size,
//! metadata stripped, thumbnails), channel and DM messages with images, what old apps see,
//! caching headers, dedup on disk, the deletion cascade (own delete, moderator delete, channel
//! delete, DM delete), DM privacy (a third member, an Admin and the owner get `404`), pending
//! uploads (only the uploader), the per-person daily allowance and the total storage cap.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use image::{Rgb, RgbImage, Rgba, RgbaImage};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "pw-img";

struct Env {
    _server: Server,
    base: String,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

async fn setup_with(extra: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"img\"\nserver_password = \"{PASSWORD}\"\nowner = \"Dono\"\n{extra}\n\
             [livekit]\napi_key = \"APIimg00000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) })
        .await
        .unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { _server: server, base, http: reqwest::Client::new(), dir }
}

async fn setup() -> Env {
    setup_with("").await
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

    /// Somebody with a role (personal password set first).
    async fn staff(&self, owner: &Who, name: &str, role: &str) -> Who {
        let w = self.who(name).await;
        assert_eq!(self.post(&w, "/api/account/password", json!({"new": format!("{name}-senha")})).await.status(), StatusCode::OK);
        let r = self.post(owner, &format!("/api/admin/users/{}/role", w.user.id), json!({ "role": role })).await;
        assert_eq!(r.status(), StatusCode::OK, "make {name} {role}");
        w
    }

    async fn owner(&self) -> Who {
        let o = self.who("Dono").await;
        assert_eq!(self.post(&o, "/api/account/password", json!({"new": "dono-senha"})).await.status(), StatusCode::OK);
        o
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn get(&self, who: &Who, path: &str) -> reqwest::Response {
        self.http.get(self.url(path)).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn upload_raw(&self, who: &Who, image: &[u8], thumb: Option<&[u8]>) -> reqwest::Response {
        let mut body = image.to_vec();
        let n = thumb.map_or(0, |t| t.len());
        if let Some(t) = thumb {
            body.extend(t);
        }
        self.http.post(self.url(&format!("/api/attachments?thumb_len={n}"))).bearer_auth(&who.token).body(body).send().await.unwrap()
    }

    async fn upload(&self, who: &Who, image: &[u8]) -> Attachment {
        let r = self.upload_raw(who, image, None).await;
        assert_eq!(r.status(), StatusCode::OK, "upload");
        r.json().await.unwrap()
    }

    async fn geral(&self, who: &Who) -> ChannelId {
        let chs: Vec<Channel> = self.get(who, "/api/channels").await.json().await.unwrap();
        chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id
    }

    async fn say(&self, who: &Who, channel: ChannelId, caption: &str, ids: &[AttachmentId]) -> reqwest::Response {
        self.post(who, &format!("/api/channels/{channel}/messages"), json!({ "content": caption, "attachments": ids })).await
    }

    fn stored_files(&self) -> Vec<PathBuf> {
        fn walk(p: &Path, out: &mut Vec<PathBuf>) {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        walk(&e.path(), out);
                    } else {
                        out.push(e.path());
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.dir.path().join("attachments"), &mut out);
        out
    }
}

async fn error_of(r: reqwest::Response) -> (StatusCode, String) {
    let s = r.status();
    (s, r.json::<ApiError>().await.map(|e| e.error).unwrap_or_default())
}

// ---------------------------------------------------------------- generated images

fn jpeg_of(img: &RgbImage, q: u8) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q).encode_image(img).unwrap();
    out
}

fn photo(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let img = RgbImage::from_fn(w, h, |x, y| Rgb([((x * 255) / w) as u8, ((y * 255) / h) as u8, ((x ^ y ^ seed) % 256) as u8]));
    jpeg_of(&img, 85)
}

fn noise_jpeg(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut s = seed | 1;
    let img = RgbImage::from_fn(w, h, |_, _| {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        Rgb([(s >> 24) as u8, (s >> 16) as u8, (s >> 8) as u8])
    });
    jpeg_of(&img, 92)
}

fn with_exif(jpeg: &[u8]) -> Vec<u8> {
    let payload = b"Exif\0\0SECRET-GPS-COORDINATES";
    let mut out = jpeg[..2].to_vec();
    out.extend([0xff, 0xe1]);
    out.extend(((payload.len() + 2) as u16).to_be_bytes());
    out.extend(payload);
    out.extend(&jpeg[2..]);
    out
}

fn png_claiming(w: u32, h: u32) -> Vec<u8> {
    // Header only (IHDR + a tiny junk IDAT): what a decompression bomb looks like to us.
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
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", [w.to_be_bytes().as_slice(), h.to_be_bytes().as_slice(), &[1, 0, 0, 0, 0]].concat()), (b"IDAT", vec![0x78, 0x9c, 1, 0, 0, 0xff, 0xff]), (b"IEND", vec![])] {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(&data);
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
    }
    out
}

fn gif(frames: u32, w: u32, h: u32) -> Vec<u8> {
    use image::codecs::gif::{GifEncoder, Repeat};
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut out);
        enc.set_repeat(Repeat::Infinite).unwrap();
        for i in 0..frames {
            let c = Rgba([(i * 40 % 256) as u8, 100, 200, 255]);
            enc.encode_frame(image::Frame::new(RgbaImage::from_pixel(w, h, c))).unwrap();
        }
    }
    out
}

fn has(b: &[u8], needle: &[u8]) -> bool {
    b.windows(needle.len()).any(|w| w == needle)
}

async fn next_event(ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, want: &str) -> ServerEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(Ok(WsMsg::Text(t)))) = tokio::time::timeout(left, ws.next()).await else { panic!("no {want} event") };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == want {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn uploads_are_checked_cleaned_and_thumbnailed() {
    let env = setup().await;
    let ana = env.who("Ana").await;
    let r = env.get(&ana, "/api/attachments/limits").await;
    assert_eq!(r.status(), StatusCode::OK);
    let lim: AttachmentLimits = r.json().await.unwrap();
    assert_eq!((lim.max_per_message, lim.max_side, lim.daily_bytes, lim.storage_full), (4, 4096, 200 * 1024 * 1024, false));

    // Junk, bombs, oversize, broken.
    assert_eq!(error_of(env.upload_raw(&ana, b"<html>hello</html>", None).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(env.upload_raw(&ana, &png_claiming(50_000, 50_000), None).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(env.upload_raw(&ana, &photo(5000, 100, 1), None).await).await.0, StatusCode::BAD_REQUEST, "over 4096 px");
    let big = noise_jpeg(1700, 1700, 7);
    assert!(big.len() > ATTACHMENT_MAX_BYTES, "{}", big.len());
    assert_eq!(error_of(env.upload_raw(&ana, &big, None).await).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    let mut cut = photo(600, 400, 2);
    cut.truncate(200);
    assert_eq!(error_of(env.upload_raw(&ana, &cut, None).await).await.0, StatusCode::BAD_REQUEST);
    // A body that is only "thumbnail".
    let r = env.http.post(env.url("/api/attachments?thumb_len=10")).bearer_auth(&ana.token).body(vec![0u8; 10]).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // A photo with EXIF: stored without it, a hub-made thumbnail.
    let src = with_exif(&photo(1600, 1200, 3));
    let a = env.upload(&ana, &src).await;
    assert_eq!((a.width, a.height, a.mime.as_str(), a.animated), (1600, 1200, "image/jpeg", false));
    assert!(a.thumb_w <= ATTACHMENT_THUMB_BOX && a.thumb_h <= ATTACHMENT_THUMB_BOX && a.thumb_w > 0, "{}x{}", a.thumb_w, a.thumb_h);
    let r = env.get(&ana, &format!("/api/attachments/{}", a.id)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let file = r.bytes().await.unwrap();
    assert!(!has(&file, b"SECRET") && !has(&file, b"Exif"), "metadata stripped");
    assert_eq!(file.len() as u64, a.size);
    let r = env.get(&ana, &format!("/api/attachments/{}/thumb", a.id)).await;
    assert_eq!(r.status(), StatusCode::OK);
    let t = image::load_from_memory(&r.bytes().await.unwrap()).unwrap();
    assert_eq!((t.width(), t.height()), (a.thumb_w, a.thumb_h));

    // The app's own thumbnail: checked against the image.
    let main = photo(800, 600, 4);
    let good = jpeg_of(&RgbImage::from_pixel(400, 300, Rgb([1, 2, 3])), 80);
    let wrong = jpeg_of(&RgbImage::from_pixel(300, 400, Rgb([1, 2, 3])), 80);
    assert_eq!(env.upload_raw(&ana, &main, Some(&good)).await.status(), StatusCode::OK);
    assert_eq!(error_of(env.upload_raw(&ana, &main, Some(&wrong)).await).await.0, StatusCode::BAD_REQUEST);

    // A small image is its own thumbnail.
    let small = env.upload(&ana, &photo(200, 100, 5)).await;
    assert_eq!((small.thumb_w, small.thumb_h), (200, 100));
    assert_eq!(env.get(&ana, &format!("/api/attachments/{}/thumb", small.id)).await.status(), StatusCode::OK);

    // An animated GIF stays animated (and gets a still thumbnail).
    let g = env.upload(&ana, &gif(5, 120, 90)).await;
    assert!(g.animated && g.mime == "image/gif");
    let r = env.get(&ana, &format!("/api/attachments/{}/thumb", g.id)).await;
    assert_ne!(r.headers()["content-type"], "image/gif");
}

#[tokio::test]
async fn channel_messages_carry_images_for_everybody_and_old_apps_see_a_marker() {
    let env = setup().await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let geral = env.geral(&ana).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", env.base, bia.token)).await.unwrap();
    next_event(&mut ws, "ready").await;

    let a1 = env.upload(&ana, &photo(1280, 720, 1)).await;
    let a2 = env.upload(&ana, &photo(640, 640, 2)).await;
    // Pending: only the uploader sees it, and nobody else can use it.
    assert_eq!(env.get(&bia, &format!("/api/attachments/{}", a1.id)).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(error_of(env.say(&bia, geral, "roubo", &[a1.id]).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(env.say(&ana, geral, "x", &[a1.id, a1.id]).await).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(env.say(&ana, geral, "", &[]).await).await.0, StatusCode::BAD_REQUEST);

    let r = env.say(&ana, geral, "  olha isso  ", &[a1.id, a2.id]).await;
    assert_eq!(r.status(), StatusCode::OK);
    let m: Message = r.json().await.unwrap();
    assert_eq!(m.content, "olha isso\n[2 imagens]", "what an old app shows");
    assert_eq!(m.caption(), "olha isso");
    assert_eq!(m.attachments.iter().map(|a| a.id).collect::<Vec<_>>(), [a1.id, a2.id]);

    let ServerEvent::MessageCreated { message } = next_event(&mut ws, "message_created").await else { panic!() };
    assert_eq!(message, m);
    // An old app's view of the same event: no `attachments`, the marker in the text.
    let raw = serde_json::to_value(&ServerEvent::MessageCreated { message: m.clone() }).unwrap();
    assert_eq!(raw["message"]["content"], "olha isso\n[2 imagens]");

    // Images only (no caption) and history.
    let a3 = env.upload(&ana, &photo(300, 200, 3)).await;
    let only: Message = env.say(&ana, geral, "", &[a3.id]).await.json().await.unwrap();
    assert_eq!((only.content.as_str(), only.caption()), ("[imagem]", ""));
    let hist: Vec<Message> = env.get(&bia, &format!("/api/channels/{geral}/messages")).await.json().await.unwrap();
    assert_eq!(hist[0].attachments.len(), 1);
    assert_eq!(hist[1].attachments.len(), 2);
    // Used attachments cannot go on another message.
    assert_eq!(error_of(env.say(&ana, geral, "de novo", &[a1.id]).await).await.0, StatusCode::BAD_REQUEST);

    // Anybody logged in fetches a channel's image, with long cache headers and an ETag.
    let r = env.get(&bia, &format!("/api/attachments/{}", a1.id)).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["cache-control"], "private, max-age=31536000, immutable");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(etag, format!("\"{}\"", a1.hash));
    let r = env.http.get(env.url(&format!("/api/attachments/{}", a1.id))).bearer_auth(&bia.token).header("if-none-match", &etag).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_MODIFIED);
    // Not logged in: no.
    assert_eq!(env.http.get(env.url(&format!("/api/attachments/{}", a1.id))).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    // Unknown id.
    assert_eq!(env.get(&bia, "/api/attachments/999999").await.status(), StatusCode::NOT_FOUND);

    // The plain WebSocket path still works and has no attachments.
    // (Covered by the api tests; here just the limit of four.)
    let many: Vec<AttachmentId> = futures_util::future::join_all((0..5).map(|i| {
        let env = &env;
        let ana = ana.clone();
        async move { env.upload(&ana, &photo(100 + i, 100, 10 + i)).await.id }
    }))
    .await;
    assert_eq!(error_of(env.say(&ana, geral, "cinco", &many).await).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn identical_files_are_stored_once_and_deletes_cascade() {
    let env = setup().await;
    let owner = env.owner().await;
    let ana = env.who("Ana").await;
    let mo = env.staff(&owner, "Mo", "moderator").await;
    let geral = env.geral(&ana).await;
    let pic = photo(1000, 700, 9);

    let a = env.upload(&ana, &pic).await;
    let b = env.upload(&ana, &pic).await;
    assert_eq!(a.hash, b.hash, "same content, same file");
    assert_ne!(a.id, b.id);
    let files = env.stored_files();
    assert_eq!(files.len(), 2, "one image file and one thumbnail: {files:?}");
    let m1: Message = env.say(&ana, geral, "um", &[a.id]).await.json().await.unwrap();
    let m2: Message = env.say(&ana, geral, "dois", &[b.id]).await.json().await.unwrap();

    // The author deletes one message: the file stays (the other still uses it).
    assert_eq!(env.post(&ana, &format!("/api/messages/{}/delete", m1.id), json!({})).await.status(), StatusCode::OK);
    assert_eq!(env.get(&ana, &format!("/api/attachments/{}", a.id)).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.get(&ana, &format!("/api/attachments/{}", b.id)).await.status(), StatusCode::OK);
    assert_eq!(env.stored_files().len(), 2);
    // A moderator deletes the other: the images are gone, from disk too.
    assert_eq!(env.post(&mo, &format!("/api/messages/{}/delete", m2.id), json!({})).await.status(), StatusCode::OK);
    assert_eq!(env.get(&ana, &format!("/api/attachments/{}", b.id)).await.status(), StatusCode::NOT_FOUND);
    assert!(env.stored_files().is_empty(), "{:?}", env.stored_files());

    // Deleting a channel deletes its images.
    let ch: Channel = env.post(&owner, "/api/admin/channels", json!({"name": "fotos", "kind": "text"})).await.json().await.unwrap();
    let c = env.upload(&ana, &photo(500, 500, 11)).await;
    assert_eq!(env.say(&ana, ch.id, "", &[c.id]).await.status(), StatusCode::OK);
    assert!(!env.stored_files().is_empty());
    assert_eq!(env.post(&owner, &format!("/api/admin/channels/{}/delete", ch.id), json!({})).await.status(), StatusCode::OK);
    assert!(env.stored_files().is_empty());
}

#[tokio::test]
async fn dm_images_are_only_for_the_two_people() {
    let env = setup().await;
    let owner = env.owner().await;
    let admin = env.staff(&owner, "Adm", "admin").await;
    let (ana, bia, caio) = (env.who("Ana").await, env.who("Bia").await, env.who("Caio").await);
    let dm: DmConversation = env.post(&ana, &format!("/api/dms/{}", bia.user.id), json!({})).await.json().await.unwrap();
    let pic = env.upload(&ana, &photo(900, 600, 21)).await;
    // Somebody else's pending image cannot go in my DM.
    let other = env.upload(&caio, &photo(90, 60, 22)).await;
    let r = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({"content": "", "attachments": [other.id]})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    let r = env.post(&ana, &format!("/api/dms/{}/messages", dm.id), json!({"content": "só pra você", "attachments": [pic.id]})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let line: DmMessage = r.json().await.unwrap();
    assert_eq!((line.caption(), line.attachments.len()), ("só pra você", 1));
    assert_eq!(line.content, "só pra você\n[imagem]");

    for (who, ok) in [(&ana, true), (&bia, true), (&caio, false), (&admin, false), (&owner, false)] {
        for path in [format!("/api/attachments/{}", pic.id), format!("/api/attachments/{}/thumb", pic.id)] {
            let status = env.get(who, &path).await.status();
            assert_eq!(status, if ok { StatusCode::OK } else { StatusCode::NOT_FOUND }, "{} {path}", who.user.username);
        }
    }
    // The history and the list carry it to the two only.
    let hist: Vec<DmMessage> = env.get(&bia, &format!("/api/dms/{}/messages", dm.id)).await.json().await.unwrap();
    assert_eq!(hist[0].attachments[0].id, pic.id);
    let list: Vec<DmConversation> = env.get(&bia, "/api/dms").await.json().await.unwrap();
    assert_eq!(list[0].last_message.as_ref().unwrap().attachments.len(), 1);
    // Channel moderation cannot reach it (DM line ids are their own space).
    assert_eq!(env.post(&owner, &format!("/api/messages/{}/delete", line.id), json!({})).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(env.get(&bia, &format!("/api/attachments/{}", pic.id)).await.status(), StatusCode::OK);

    // Deleting the line deletes the image.
    assert_eq!(env.post(&ana, &format!("/api/dms/{}/messages/{}/delete", dm.id, line.id), json!({})).await.status(), StatusCode::OK);
    assert_eq!(env.get(&bia, &format!("/api/attachments/{}", pic.id)).await.status(), StatusCode::NOT_FOUND);
    assert!(env.stored_files().iter().all(|p| !p.to_string_lossy().contains(&pic.hash)));
}

#[tokio::test]
async fn the_daily_allowance_is_per_person() {
    let env = setup_with("attachment_daily_mb = 1").await;
    let (ana, bia) = (env.who("Ana").await, env.who("Bia").await);
    let pic = noise_jpeg(560, 560, 3);
    assert!(pic.len() > 400 * 1024 && pic.len() < 900 * 1024, "{}", pic.len());
    env.upload(&ana, &pic).await;
    let (status, msg) = error_of(env.upload_raw(&ana, &noise_jpeg(560, 560, 4), None).await).await;
    assert_eq!((status, msg.as_str()), (StatusCode::TOO_MANY_REQUESTS, ERR_ATTACHMENT_DAILY));
    let lim: AttachmentLimits = env.get(&ana, "/api/attachments/limits").await.json().await.unwrap();
    assert!(lim.daily_bytes_left < 1024 * 1024 - pic.len() as u64 + 1);
    // Bia has her own allowance.
    env.upload(&bia, &noise_jpeg(560, 560, 5)).await;
}

#[tokio::test]
async fn a_full_storage_refuses_new_images_politely() {
    // About 1 MB of storage.
    let env = setup_with("max_attachment_storage_gb = 0.001").await;
    let ana = env.who("Ana").await;
    env.upload(&ana, &noise_jpeg(560, 560, 1)).await;
    let mut refused = None;
    for i in 0..4 {
        let r = env.upload_raw(&ana, &noise_jpeg(560, 560, 10 + i), None).await;
        if r.status() != StatusCode::OK {
            refused = Some(error_of(r).await);
            break;
        }
    }
    assert_eq!(refused, Some((StatusCode::INSUFFICIENT_STORAGE, ERR_ATTACHMENT_STORAGE_FULL.to_string())));
    // An identical file takes no new space: still accepted.
    let again = env.upload_raw(&ana, &noise_jpeg(560, 560, 1), None).await;
    assert_eq!(again.status(), StatusCode::OK);
}
