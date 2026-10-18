//! Invite migration safety, against a database written by the released v0.7.0 hub
//! (`releases/v0.7.0/voxhub-server.exe`, or `VOX_V070_HUB`). Same method as
//! `attachments_migration.rs`: the old hub runs on a temp dir, people log in and chat, it is
//! killed like a crash, and the new hub opens a *copy*: old sessions and history are there,
//! invites work on it (and survive a restart), and the v0.7.0 hub still runs on the migrated copy
//! (downgrade: the invite tables are new tables it never reads). Skipped when no old binary is
//! found.

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
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    std::env::var_os("VOX_V070_HUB").map(PathBuf::from).into_iter().chain([root.join("releases/v0.7.0/voxhub-server.exe")]).find(|p| p.exists())
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

async fn login(http: &reqwest::Client, port: u16, body: Value) -> reqwest::Response {
    http.post(format!("http://127.0.0.1:{port}/api/login")).json(&body).send().await.unwrap()
}

async fn login_ok(http: &reqwest::Client, port: u16, body: Value) -> Value {
    let r = login(http, port, body).await;
    let (s, t) = (r.status(), r.text().await.unwrap());
    assert_eq!(s, StatusCode::OK, "{t}");
    serde_json::from_str(&t).unwrap()
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
async fn a_v070_database_gets_invites_and_stays_usable() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.7.0 hub binary (set VOX_V070_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v070"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- the v0.7.0 hub writes its database
    let port = free_port();
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let bruno = login_ok(&http, port, json!({"username": "Bruno", "server_password": PASSWORD, "protocol_version": 1})).await;
    let bt = bruno["token"].as_str().unwrap().to_string();
    let chs: Vec<Channel> = http.get(format!("http://127.0.0.1:{port}/api/channels")).bearer_auth(&bt).send().await.unwrap().json().await.unwrap();
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    // A v0.7.0 hub has no invites: the endpoints are missing and the field is ignored.
    assert_eq!(http.get(format!("http://127.0.0.1:{port}/api/invites")).bearer_auth(&bt).send().await.unwrap().status(), StatusCode::NOT_FOUND);
    let r = login(&http, port, json!({"username": "Zé", "server_password": "", "protocol_version": 1, "invite": "ab".repeat(16)})).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "an old hub wants the password");
    say(port, &bt, geral, "oi do v0.7.0").await;
    drop(old); // killed like a crash: the last writes may only be in the WAL
    copy_db(&old_dir, &new_dir);

    // ---- the new hub opens the copy, twice
    let port = free_port();
    config(&new_dir, port);
    let base = format!("http://127.0.0.1:{port}");
    let mut owner_token = String::new();
    let mut dani_device = String::new();
    for round in 0..2 {
        let server = start(Options { data_dir: new_dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
        // The v0.7.0 session is still valid and the history is there.
        let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&bt).send().await.unwrap().json().await.unwrap();
        assert_eq!(msgs.last().unwrap().content, "oi do v0.7.0", "round {round}");
        if round == 0 {
            let ana = login_ok(&http, port, json!({"username": "Ana", "server_password": PASSWORD, "protocol_version": 1})).await;
            let r = http.post(format!("{base}/api/account/password")).bearer_auth(ana["token"].as_str().unwrap()).json(&json!({"new": "ana-senha-1"})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let ana = login_ok(&http, port, json!({"username": "Ana", "server_password": PASSWORD, "password": "ana-senha-1", "protocol_version": 1})).await;
            owner_token = ana["token"].as_str().unwrap().to_string();
            let inv: InviteCreated = http.post(format!("{base}/api/invites")).bearer_auth(&owner_token).json(&json!({"max_uses": 3})).send().await.unwrap().json().await.unwrap();
            let dani = login_ok(&http, port, json!({"username": "Dani", "server_password": "", "protocol_version": 1, "invite": inv.token, "remember_device": true})).await;
            dani_device = dani["device_token"].as_str().unwrap().to_string();
        } else {
            // Second start: the invite, its use and Dani's way back in are still there.
            let list: Vec<Invite> = http.get(format!("{base}/api/invites")).bearer_auth(&owner_token).send().await.unwrap().json().await.unwrap();
            assert_eq!((list.len(), list[0].uses), (1, 1));
            login_ok(&http, port, json!({"username": "Dani", "server_password": "", "protocol_version": 1, "device_token": dani_device})).await;
        }
        server.shutdown().await;
    }
    let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
    db.call_now(|c| {
        assert_eq!(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?, voxhub_server::db::SCHEMA_VERSION);
        for t in ["invites", "invite_members"] {
            assert_eq!(c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?", [t], |r| r.get::<_, i64>(0))?, 1, "{t}");
        }
        Ok(())
    })
    .unwrap();
    drop(db);

    // ---- downgrade: the v0.7.0 hub still runs on the migrated copy (it ignores the invite tables;
    // Dani, who only had the invite, needs the server password there, like before invites)
    let port = free_port();
    config(&new_dir, port);
    let old = run_old(&bin, &new_dir, port).await;
    let r = login(&http, port, json!({"username": "Dani", "server_password": "", "protocol_version": 1, "device_token": dani_device})).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let dani = login_ok(&http, port, json!({"username": "Dani", "server_password": PASSWORD, "protocol_version": 1})).await;
    say(port, dani["token"].as_str().unwrap(), geral, "de volta ao v0.7.0").await;
    drop(old);
}
