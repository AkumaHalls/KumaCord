//! DM migration safety, against a database written by the real v0.5.2 hub (the release the
//! friends run). Same method as `migration.rs`: `releases/v0.5.2/voxhub-server.exe` (or
//! `VOX_V052_HUB`) runs on a temp dir, two people log in and chat, it is killed like a crash, and
//! the new hub opens a *copy*: the old sessions and messages are there, DMs work on it (and survive
//! a restart), and the v0.5.2 hub still runs on the migrated copy (downgrade: the DM tables are new
//! tables it never reads). Skipped when the old binary is not there.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options};

const PASSWORD: &str = "pw";

fn old_hub() -> Option<PathBuf> {
    let p = std::env::var_os("VOX_V052_HUB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../releases/v0.5.2/voxhub-server.exe"));
    p.exists().then_some(p)
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

#[tokio::test]
async fn a_v052_database_gets_dms_and_stays_usable() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.5.2 hub binary (set VOX_V052_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v052"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- the v0.5.2 hub writes its database
    let port = free_port();
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let bruno = login(&http, port, "Bruno").await;
    let carla = login(&http, port, "Carla").await;
    let chs: Vec<Channel> = http
        .get(format!("http://127.0.0.1:{port}/api/channels"))
        .bearer_auth(bruno["token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    // A v0.5.2 hub has no DMs: the 404 is how a new app knows to hide them.
    let r = http.get(format!("http://127.0.0.1:{port}/api/dms")).bearer_auth(bruno["token"].as_str().unwrap()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    say(port, bruno["token"].as_str().unwrap(), geral, "oi do v0.5.2").await;
    drop(old); // killed like a crash: the last writes may only be in the WAL
    copy_db(&old_dir, &new_dir);

    // ---- the new hub opens the copy, twice
    let port = free_port();
    config(&new_dir, port);
    let base = format!("http://127.0.0.1:{port}");
    let (bt, ct) = (bruno["token"].as_str().unwrap().to_string(), carla["token"].as_str().unwrap().to_string());
    let carla_id = carla["user"]["id"].as_i64().unwrap();
    let mut dm_id = 0;
    for round in 0..2 {
        let server = start(Options { data_dir: new_dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
        // The v0.5.2 sessions are still valid and the channel history is there.
        let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&bt).send().await.unwrap().json().await.unwrap();
        assert_eq!(msgs.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["oi do v0.5.2"], "round {round}");
        if round == 0 {
            let r = http.get(format!("{base}/api/dms")).bearer_auth(&bt).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            assert!(r.json::<Vec<DmConversation>>().await.unwrap().is_empty(), "a migrated file starts with no DMs");
            let dm: DmConversation = http.post(format!("{base}/api/dms/{carla_id}")).bearer_auth(&bt).json(&json!({})).send().await.unwrap().json().await.unwrap();
            dm_id = dm.id;
            let r = http.post(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&bt).json(&json!({"content": "dm no banco migrado"})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        } else {
            // Second start: the DM is still there, unread for Carla.
            let list: Vec<DmConversation> = http.get(format!("{base}/api/dms")).bearer_auth(&ct).send().await.unwrap().json().await.unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!((list[0].id, list[0].unread), (dm_id, 1));
            let lines: Vec<DmMessage> = http.get(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&ct).send().await.unwrap().json().await.unwrap();
            assert_eq!(lines[0].content, "dm no banco migrado");
        }
        server.shutdown().await;
    }
    let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
    db.call_now(|c| {
        for t in ["dm_conversations", "dm_messages", "dm_blocks"] {
            assert_eq!(c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?", [t], |r| r.get::<_, i64>(0))?, 1, "{t}");
        }
        assert_eq!(c.query_row("SELECT format FROM dm_messages", [], |r| r.get::<_, String>(0))?, "plain");
        Ok(())
    })
    .unwrap();
    drop(db);

    // ---- downgrade: the v0.5.2 hub still runs on the migrated copy (it ignores the DM tables)
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
    assert_eq!(msgs.len(), 1);
    say(port, dani["token"].as_str().unwrap(), geral, "de volta ao v0.5.2").await;
    drop(old);
}
