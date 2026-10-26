//! Chat-image migration safety, against a database written by the real v0.6.0 hub (the release
//! the friends run). Same method as `dm_migration.rs`: `releases/v0.6.0/voxhub-server.exe` (or
//! `VOX_V060_HUB`, or a v0.6.0 hub built from the `v0.6.0` tag into
//! `target-v02/v060-hub/voxhub-server.exe`) runs on a temp dir, people log in, chat and DM, it is
//! killed like a crash, and the new hub opens a *copy*: the old sessions, messages and DMs are
//! there, images work on it (and survive a restart, files included), and the v0.6.0 hub still
//! runs on the migrated copy (downgrade: the image tables are new tables it never reads, and a
//! message with images reads as its caption plus "[imagem]"). Skipped when no old binary is found.

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

fn old_hub() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    std::env::var_os("VOX_V060_HUB")
        .map(PathBuf::from)
        .into_iter()
        .chain([root.join("releases/v0.6.0/voxhub-server.exe"), root.join("target-v02/v060-hub/voxhub-server.exe")])
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
            "server_name = \"mig\"\nserver_password = \"{PASSWORD}\"\nbind = \"127.0.0.1:{port}\"\nowner = \"Ana\"\n\
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

async fn login(http: &reqwest::Client, port: u16, name: &str) -> Value {
    let r = http
        .post(format!("http://127.0.0.1:{port}/api/login"))
        .json(&json!({"username": name, "server_password": PASSWORD, "protocol_version": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    r.json().await.unwrap()
}

async fn say(port: u16, token: &str, channel: i64, text: &str) {
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

fn copy_db(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for name in ["voxhub.db", "voxhub.db-wal", "voxhub.db-shm"] {
        if from.join(name).exists() {
            std::fs::copy(from.join(name), to.join(name)).unwrap();
        }
    }
}

fn photo() -> Vec<u8> {
    let img = RgbImage::from_fn(800, 600, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 77]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img).unwrap();
    out
}

#[tokio::test]
async fn a_v060_database_gets_images_and_stays_usable() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.6.0 hub binary (set VOX_V060_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v060"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- the v0.6.0 hub writes its database (a channel message and a DM)
    let port = free_port();
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let bruno = login(&http, port, "Bruno").await;
    let carla = login(&http, port, "Carla").await;
    let (bt, ct) = (bruno["token"].as_str().unwrap().to_string(), carla["token"].as_str().unwrap().to_string());
    let carla_id = carla["user"]["id"].as_i64().unwrap();
    let chs: Vec<Channel> = http.get(format!("http://127.0.0.1:{port}/api/channels")).bearer_auth(&bt).send().await.unwrap().json().await.unwrap();
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    // A v0.6.0 hub has no images: the 404 is how a new app knows to hide the clip button.
    let r = http.get(format!("http://127.0.0.1:{port}/api/attachments/limits")).bearer_auth(&bt).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    say(port, &bt, geral, "oi do v0.6.0").await;
    let dm: Value = http.post(format!("http://127.0.0.1:{port}/api/dms/{carla_id}")).bearer_auth(&bt).json(&json!({})).send().await.unwrap().json().await.unwrap();
    let dm_id = dm["id"].as_i64().unwrap();
    let r = http.post(format!("http://127.0.0.1:{port}/api/dms/{dm_id}/messages")).bearer_auth(&bt).json(&json!({"content": "dm do v0.6.0"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    drop(old); // killed like a crash: the last writes may only be in the WAL
    copy_db(&old_dir, &new_dir);

    // ---- the new hub opens the copy, twice
    let port = free_port();
    config(&new_dir, port);
    let base = format!("http://127.0.0.1:{port}");
    let mut image_id = 0;
    for round in 0..2 {
        let server = start(Options { data_dir: new_dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
        // The v0.6.0 sessions are still valid, the history and the DM are there.
        let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&bt).send().await.unwrap().json().await.unwrap();
        let lines: Vec<DmMessage> = http.get(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&ct).send().await.unwrap().json().await.unwrap();
        assert_eq!(lines.last().unwrap().content, "dm do v0.6.0", "round {round}");
        if round == 0 {
            assert_eq!(msgs.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["oi do v0.6.0"]);
            assert!(msgs[0].attachments.is_empty());
            let r = http.get(format!("{base}/api/attachments/limits")).bearer_auth(&bt).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let a: Attachment = http.post(format!("{base}/api/attachments")).bearer_auth(&bt).body(photo()).send().await.unwrap().json().await.unwrap();
            image_id = a.id;
            let r = http.post(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&bt).json(&json!({"content": "foto nova", "attachments": [a.id]})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        } else {
            // Second start: the message, its image and the file are still there.
            assert_eq!(msgs[0].caption(), "foto nova");
            assert_eq!(msgs[0].attachments[0].id, image_id);
            let r = http.get(format!("{base}/api/attachments/{image_id}")).bearer_auth(&ct).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            assert_eq!(r.bytes().await.unwrap().len() as u64, msgs[0].attachments[0].size);
        }
        server.shutdown().await;
    }
    let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
    db.call_now(|c| {
        assert_eq!(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?, voxhub_server::db::SCHEMA_VERSION);
        for t in ["attachment_blobs", "attachments", "attachment_uploads"] {
            assert_eq!(c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?", [t], |r| r.get::<_, i64>(0))?, 1, "{t}");
        }
        Ok(())
    })
    .unwrap();
    drop(db);

    // ---- downgrade: the v0.6.0 hub still runs on the migrated copy (it ignores the image tables)
    let port = free_port();
    config(&new_dir, port);
    let old = run_old(&bin, &new_dir, port).await;
    let dani = login(&http, port, "Dani").await;
    let msgs: Vec<Value> = http
        .get(format!("http://127.0.0.1:{port}/api/channels/{geral}/messages"))
        .bearer_auth(dani["token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["content"], "foto nova\n[imagem]", "an old hub shows the caption and the marker");
    assert!(msgs[0].get("attachments").is_none());
    say(port, dani["token"].as_str().unwrap(), geral, "de volta ao v0.6.0").await;
    drop(old);
}
