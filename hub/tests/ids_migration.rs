//! v0.8.2: ids are never handed out twice, on a database written by the real v0.8.1 hub. Same
//! method as `chat_migration.rs`: `releases/v0.8.1/voxhub-server.exe` (or `VOX_V081_HUB`, or a
//! v0.8.1 hub built from the `v0.8.1` tag into `target-v02/v081-hub/voxhub-server.exe`) runs on a
//! temp dir, people chat, send an image, create a channel and DM; it is killed like a crash, and
//! the new hub opens a *copy* (twice). There the newest message, image, channel and DM line are
//! deleted and new ones created: every new id is above every id ever used (SQLite alone would hand
//! the deleted ones out again), and the new image is served with its own file. Then the v0.8.1 hub
//! runs on the migrated copy (downgrade): it reads and writes (it may reuse an id, as it always
//! did, but it must not fail), and back on the new hub the ids are still fresh. Skipped when no
//! old binary is found.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use image::{Rgb, RgbImage};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options};

const PASSWORD: &str = "pw";
const OWNER_PW: &str = "dono-senha-1";

fn old_hub() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    std::env::var_os("VOX_V081_HUB")
        .map(PathBuf::from)
        .into_iter()
        .chain([root.join("releases/v0.8.1/voxhub-server.exe"), root.join("target-v02/v081-hub/voxhub-server.exe")])
        .find(|p| p.exists())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn config(dir: &Path, port: u16) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "server_name = \"mig\"\nserver_password = \"{PASSWORD}\"\nbind = \"127.0.0.1:{port}\"\nowner = \"Dono\"\n\
             [livekit]\napi_key = \"APImig000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
}

/// Runs the old hub; killed on drop (only this exact child).
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
        assert!(t.elapsed() < Duration::from_secs(15), "the old hub did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    hub
}

fn copy_db(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for name in ["voxhub.db", "voxhub.db-wal", "voxhub.db-shm"] {
        if from.join(name).exists() {
            std::fs::copy(from.join(name), to.join(name)).unwrap();
        }
    }
    // The image files go with the database.
    let (src, dst) = (from.join("attachments"), to.join("attachments"));
    if src.exists() {
        for sub in std::fs::read_dir(&src).unwrap().flatten() {
            std::fs::create_dir_all(dst.join(sub.file_name())).unwrap();
            for f in std::fs::read_dir(sub.path()).unwrap().flatten() {
                std::fs::copy(f.path(), dst.join(sub.file_name()).join(f.file_name())).unwrap();
            }
        }
    }
}

fn new_hub(dir: &Path) -> Options {
    Options { data_dir: dir.to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }
}

/// A photo whose pixels depend on `seed` (so every one is a different file).
fn photo(seed: u8) -> Vec<u8> {
    let img = RgbImage::from_fn(640, 480, |x, y| Rgb([(x % 256) as u8 ^ seed, (y % 256) as u8, seed]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img).unwrap();
    out
}

/// One hub, as the tests talk to it.
struct Hub {
    http: reqwest::Client,
    base: String,
}

impl Hub {
    fn at(http: &reqwest::Client, port: u16) -> Hub {
        Hub { http: http.clone(), base: format!("http://127.0.0.1:{port}") }
    }

    async fn login(&self, name: &str, personal: Option<&str>) -> (String, i64) {
        let r = self
            .http
            .post(format!("{}/api/login", self.base))
            .json(&json!({"username": name, "server_password": PASSWORD, "protocol_version": 1, "password": personal}))
            .send()
            .await
            .unwrap();
        let status = r.status();
        let v: Value = r.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "login {name}: {v}");
        (v["token"].as_str().unwrap().to_string(), v["user"]["id"].as_i64().unwrap())
    }

    async fn post(&self, token: &str, path: &str, body: Value) -> reqwest::Response {
        self.http.post(format!("{}{path}", self.base)).bearer_auth(token).json(&body).send().await.unwrap()
    }

    async fn ok<T: serde::de::DeserializeOwned>(&self, token: &str, path: &str, body: Value) -> T {
        let r = self.post(token, path, body).await;
        let status = r.status();
        let text = r.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{path}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, token: &str, path: &str) -> T {
        let r = self.http.get(format!("{}{path}", self.base)).bearer_auth(token).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        r.json().await.unwrap()
    }

    /// Over the WebSocket (the old apps' way), waiting for it to come back.
    async fn say(&self, token: &str, channel: i64, text: &str) -> i64 {
        let ws_url = format!("{}/ws?token={token}", self.base.replace("http://", "ws://"));
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        ws.send(WsMsg::Text(json!({"type": "send_message", "channel_id": channel, "content": text}).to_string().into())).await.unwrap();
        loop {
            let Some(Ok(WsMsg::Text(t))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap() else { continue };
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "message_created" && v["message"]["content"] == text {
                return v["message"]["id"].as_i64().unwrap();
            }
        }
    }

    /// Upload a photo and send it in `channel`: (message, attachment).
    async fn send_photo(&self, token: &str, channel: i64, seed: u8, caption: &str) -> (Message, Attachment) {
        let r = self.http.post(format!("{}/api/attachments", self.base)).bearer_auth(token).body(photo(seed)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let a: Attachment = r.json().await.unwrap();
        let m: Message = self.ok(token, &format!("/api/channels/{channel}/messages"), json!({"content": caption, "attachments": [a.id]})).await;
        assert_eq!(m.attachments.len(), 1);
        let a = m.attachments[0].clone();
        (m, a)
    }

    async fn thumb_status(&self, token: &str, id: AttachmentId) -> (StatusCode, Option<String>) {
        let r = self.http.get(format!("{}/api/attachments/{id}/thumb", self.base)).bearer_auth(token).send().await.unwrap();
        (r.status(), r.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_string))
    }

    async fn create_channel(&self, token: &str, name: &str) -> Channel {
        self.ok(token, "/api/admin/channels", json!(ChannelCreateRequest { name: name.into(), kind: ChannelKind::Text, private: None })).await
    }

    async fn dm_line(&self, token: &str, dm: i64, text: &str) -> i64 {
        let line: Value = self.ok(token, &format!("/api/dms/{dm}/messages"), json!({"content": text})).await;
        line["id"].as_i64().unwrap()
    }
}

/// Everything the old hub created, and the newest ids seen of each kind.
#[derive(Default, Debug)]
struct Seen {
    message: i64,
    attachment: i64,
    channel: i64,
    dm_line: i64,
}

impl Seen {
    fn note(&mut self, message: i64, attachment: i64, channel: i64, dm_line: i64) {
        self.message = self.message.max(message);
        self.attachment = self.attachment.max(attachment);
        self.channel = self.channel.max(channel);
        self.dm_line = self.dm_line.max(dm_line);
    }
}

#[tokio::test]
async fn ids_are_never_reused_on_a_v081_database() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.8.1 hub binary (set VOX_V081_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v081"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();
    let mut seen = Seen::default();

    // ---- the v0.8.1 hub writes its database
    let port = free_port();
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let hub = Hub::at(&http, port);
    let (dt, _dono) = hub.login("Dono", None).await;
    let _: Value = hub.ok(&dt, "/api/account/password", json!({"new": OWNER_PW})).await;
    let (bt, bruno) = hub.login("Bruno", None).await;
    let chs: Vec<Channel> = hub.get(&bt, "/api/channels").await;
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let first = hub.say(&bt, geral, "primeira do v0.8.1").await;
    let (old_photo_msg, old_photo) = hub.send_photo(&bt, geral, 1, "foto do v0.8.1").await;
    let extra = hub.create_channel(&dt, "extra").await;
    let dm: Value = hub.ok(&dt, &format!("/api/dms/{bruno}"), json!({})).await;
    let dm_id = dm["id"].as_i64().unwrap();
    let old_line = hub.dm_line(&dt, dm_id, "dm do v0.8.1").await;
    seen.note(old_photo_msg.id, old_photo.id, extra.id, old_line);
    drop(old); // killed like a crash: the last writes may only be in the WAL
    copy_db(&old_dir, &new_dir);

    // ---- the new hub opens the copy, twice: delete the newest of everything, make new ones
    let port = free_port();
    config(&new_dir, port);
    let hub = Hub::at(&http, port);
    for round in 0..2 {
        let server = start(new_hub(&new_dir)).await.unwrap();
        // The old sessions still work and the old rows are there.
        let msgs: Vec<Message> = hub.get(&bt, &format!("/api/channels/{geral}/messages")).await;
        assert!(msgs.iter().any(|m| m.id == first), "round {round}");
        let newest = msgs.iter().map(|m| m.id).max().unwrap();
        assert_eq!(newest, seen.message, "round {round}: the newest message is the one made last");
        let (newest_photo_msg, newest_photo) = msgs.iter().filter(|m| !m.attachments.is_empty()).map(|m| (m.id, m.attachments[0].clone())).max_by_key(|(id, _)| *id).unwrap();
        assert_eq!(newest_photo_msg, newest, "round {round}: the newest message has the newest image");
        let (code, old_etag) = hub.thumb_status(&bt, newest_photo.id).await;
        assert_eq!(code, StatusCode::OK);

        // Delete the newest message (with its image), channel and DM line...
        let _: OkResponse = hub.ok(&bt, &format!("/api/messages/{newest}/delete"), json!({})).await;
        assert_eq!(hub.thumb_status(&bt, newest_photo.id).await.0, StatusCode::NOT_FOUND, "the image went with its message");
        let newest_channel = hub.get::<Vec<Channel>>(&dt, "/api/channels").await.iter().map(|c| c.id).max().unwrap();
        assert_eq!(newest_channel, seen.channel, "round {round}");
        let _: OkResponse = hub.ok(&dt, &format!("/api/admin/channels/{newest_channel}/delete"), json!({})).await;
        let lines: Vec<DmMessage> = hub.get(&dt, &format!("/api/dms/{dm_id}/messages")).await;
        let newest_line = lines.iter().map(|l| l.id).max().unwrap();
        assert_eq!(newest_line, seen.dm_line, "round {round}");
        let _: OkResponse = hub.ok(&dt, &format!("/api/dms/{dm_id}/messages/{newest_line}/delete"), json!({})).await;

        // ...and make new ones: every id is fresh (SQLite alone would hand the deleted ones out).
        let text = hub.say(&bt, geral, &format!("texto {round}")).await;
        assert!(text > seen.message, "round {round}: message {text} reused (newest seen {})", seen.message);
        let (photo_msg, photo) = hub.send_photo(&bt, geral, 10 + round as u8, "foto nova").await;
        assert!(photo_msg.id > text);
        assert!(photo.id > seen.attachment, "round {round}: image {} reused (newest seen {})", photo.id, seen.attachment);
        assert_ne!(photo.hash, newest_photo.hash, "a different file");
        let (code, etag) = hub.thumb_status(&bt, photo.id).await;
        assert_eq!(code, StatusCode::OK);
        assert_ne!(etag, old_etag, "round {round}: the new image is served with its own file");
        let ch = hub.create_channel(&dt, &format!("nova {round}")).await;
        assert!(ch.id > seen.channel, "round {round}: channel {} reused (newest seen {})", ch.id, seen.channel);
        let empty: Vec<Message> = hub.get(&bt, &format!("/api/channels/{}/messages", ch.id)).await;
        assert!(empty.is_empty(), "a new channel starts empty");
        let line = hub.dm_line(&dt, dm_id, "dm novo").await;
        assert!(line > seen.dm_line, "round {round}: DM line {line} reused (newest seen {})", seen.dm_line);
        seen.note(photo_msg.id, photo.id, ch.id, line);
        server.shutdown().await;
    }
    let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
    db.call_now(|c| {
        assert_eq!(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?, voxhub_server::db::SCHEMA_VERSION, "no version bump");
        for (seq, at_least) in [(voxhub_server::ids::Seq::Messages, seen.message), (voxhub_server::ids::Seq::Channels, seen.channel), (voxhub_server::ids::Seq::DmMessages, seen.dm_line)] {
            assert!(voxhub_server::ids::high_water(c, seq)? >= at_least, "{seq:?}");
        }
        Ok(())
    })
    .unwrap();
    drop(db);

    // ---- downgrade: the v0.8.1 hub runs on the migrated copy (it never reads `id_high_water`)
    let port = free_port();
    config(&new_dir, port);
    let old = run_old(&bin, &new_dir, port).await;
    let hub = Hub::at(&http, port);
    let msgs: Vec<Message> = hub.get(&bt, &format!("/api/channels/{geral}/messages")).await;
    assert!(msgs.iter().any(|m| m.id == seen.message), "it reads what the new hub wrote");
    // Its own deletes and inserts work (it may reuse an id here, as it always did).
    let _: OkResponse = hub.ok(&bt, &format!("/api/messages/{}/delete", seen.message), json!({})).await;
    hub.say(&bt, geral, "de volta ao v0.8.1").await;
    let (old_msg, old_img) = hub.send_photo(&bt, geral, 99, "foto do v0.8.1 de novo").await;
    let old_ch = hub.create_channel(&dt, "do v0.8.1").await;
    let old_line = hub.dm_line(&dt, dm_id, "dm do v0.8.1 de novo").await;
    seen.note(old_msg.id, old_img.id, old_ch.id, old_line);
    drop(old);

    // ---- the new hub again: above everything either hub ever handed out
    let port = free_port();
    config(&new_dir, port);
    let hub = Hub::at(&http, port);
    let server = start(new_hub(&new_dir)).await.unwrap();
    let (photo_msg, photo) = hub.send_photo(&bt, geral, 200, "foto final").await;
    assert!(photo_msg.id > seen.message && photo.id > seen.attachment, "{photo_msg:?} vs {seen:?}");
    let ch = hub.create_channel(&dt, "final").await;
    assert!(ch.id > seen.channel);
    let line = hub.dm_line(&dt, dm_id, "dm final").await;
    assert!(line > seen.dm_line);
    let msgs: Vec<Message> = hub.get(&bt, &format!("/api/channels/{geral}/messages")).await;
    assert!(msgs.iter().any(|m| m.content == "de volta ao v0.8.1"));
    server.shutdown().await;
}
