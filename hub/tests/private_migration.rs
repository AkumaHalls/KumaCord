//! Private channels (v0.9) on a database written by the real v0.8.2 hub, and the downgrade guard.
//! Same method as `ids_migration.rs`: `releases/v0.8.2/voxhub-server.exe` (or `VOX_V082_HUB`, or a
//! v0.8.2 hub built from the `v0.8.2` tag into `target-v02/v082-hub/voxhub-server.exe`) runs on a
//! temp dir, people chat and a channel is created; it is killed like a crash, and the new hub opens
//! a *copy* (twice): everything is there, a channel is made private and stays private across a
//! restart. Then the v0.8.2 hub is run on that file: it must **refuse to start** (it would show the
//! private channel to everybody). Made public again on the new hub, the v0.8.2 hub runs on the file
//! as before and reads everything. Skipped when no old binary is found.

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
const OWNER_PW: &str = "dono-senha-1";

fn old_hub() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    std::env::var_os("VOX_V082_HUB")
        .map(PathBuf::from)
        .into_iter()
        .chain([root.join("releases/v0.8.2/voxhub-server.exe"), root.join("target-v02/v082-hub/voxhub-server.exe")])
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

fn spawn_old(bin: &Path, dir: &Path, stderr: Stdio) -> OldHub {
    let child = Command::new(bin)
        .arg("--data-dir")
        .arg(dir)
        .arg("--no-livekit")
        .arg("--log-dir")
        .arg(dir.join("logs"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .unwrap();
    OldHub(child)
}

async fn run_old(bin: &Path, dir: &Path, port: u16) -> OldHub {
    let hub = spawn_old(bin, dir, Stdio::null());
    let t = Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        assert!(t.elapsed() < Duration::from_secs(15), "the old hub did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    hub
}

/// The old hub must exit with an error and never listen.
async fn old_refuses(bin: &Path, dir: &Path, port: u16) {
    let mut hub = spawn_old(bin, dir, Stdio::piped());
    let t = Instant::now();
    loop {
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err(), "the v0.8.2 hub started on a database with a private channel");
        if let Some(status) = hub.0.try_wait().unwrap() {
            assert!(!status.success(), "it must fail, not end normally");
            // ...and because of the guard row (`access.rs`), not anything else.
            let mut err = String::new();
            std::io::Read::read_to_string(&mut hub.0.stderr.take().unwrap(), &mut err).unwrap();
            assert!(err.contains("Invalid column type") && err.contains("position"), "{err}");
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "the v0.8.2 hub neither started nor exited");
        tokio::time::sleep(Duration::from_millis(100)).await;
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

fn new_hub(dir: &Path) -> Options {
    Options { data_dir: dir.to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }
}

struct Hub {
    http: reqwest::Client,
    base: String,
}

impl Hub {
    fn at(http: &reqwest::Client, port: u16) -> Hub {
        Hub { http: http.clone(), base: format!("http://127.0.0.1:{port}") }
    }

    async fn login(&self, name: &str, personal: Option<&str>) -> String {
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
        v["token"].as_str().unwrap().to_string()
    }

    async fn ok<T: serde::de::DeserializeOwned>(&self, token: &str, path: &str, body: Value) -> T {
        let r = self.http.post(format!("{}{path}", self.base)).bearer_auth(token).json(&body).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{path}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn get_status(&self, token: &str, path: &str) -> StatusCode {
        self.http.get(format!("{}{path}", self.base)).bearer_auth(token).send().await.unwrap().status()
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

    async fn channel_ids(&self, token: &str) -> Vec<i64> {
        let list: Vec<Value> = self.get(token, "/api/channels").await;
        list.iter().map(|c| c["id"].as_i64().unwrap()).collect()
    }
}

#[tokio::test]
async fn private_channels_on_a_v082_database_and_the_downgrade_guard() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.8.2 hub binary (set VOX_V082_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v082"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- the v0.8.2 hub writes its database: a channel that will become private, with history
    let port = free_port();
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let hub = Hub::at(&http, port);
    let dt = hub.login("Dono", None).await;
    let _: Value = hub.ok(&dt, "/api/account/password", json!({"new": OWNER_PW})).await;
    let bt = hub.login("Bruno", None).await;
    let ct = hub.login("Carla", None).await;
    let geral = hub.channel_ids(&bt).await[0];
    hub.say(&bt, geral, "oi do v0.8.2").await;
    let segredos: Value = hub.ok(&dt, "/api/admin/channels", json!({"name": "segredos", "kind": "text"})).await;
    let segredos = segredos["id"].as_i64().unwrap();
    let old_msg = hub.say(&ct, segredos, "conversa antiga").await;
    drop(old); // killed like a crash
    copy_db(&old_dir, &new_dir);

    // ---- the new hub on the copy, twice: nothing private at first; then private, and it stays so
    let port = free_port();
    config(&new_dir, port);
    let hub = Hub::at(&http, port);
    for round in 0..2 {
        let server = start(new_hub(&new_dir)).await.unwrap();
        let chans: Vec<Channel> = hub.get(&dt, "/api/channels").await;
        assert!(chans.iter().any(|c| c.id == geral), "round {round}");
        let s = chans.iter().find(|c| c.id == segredos).unwrap_or_else(|| panic!("round {round}: the owner always sees it"));
        if round == 0 {
            assert!(s.private.is_none(), "a v0.8.2 channel reads as public");
            assert!(hub.channel_ids(&bt).await.contains(&segredos));
            // Private for Carla (by name), staff always.
            let carla = hub.get::<Vec<MemberInfo>>(&dt, "/api/admin/users").await.into_iter().find(|m| m.user.username == "Carla").unwrap().user.id;
            let _: OkResponse = hub.ok(&dt, &format!("/api/admin/channels/{segredos}/access"), json!({"private": {"users": [carla]}})).await;
        } else {
            assert!(s.private.as_ref().is_some_and(|p| p.users.len() == 1), "round {round}: still private after a restart: {s:?}");
        }
        // Bruno (the old session from the v0.8.2 hub still works) sees nothing of it, Carla does.
        assert!(!hub.channel_ids(&bt).await.contains(&segredos), "round {round}");
        assert_eq!(hub.get_status(&bt, &format!("/api/channels/{segredos}/messages")).await, StatusCode::NOT_FOUND);
        let msgs: Vec<Message> = hub.get(&ct, &format!("/api/channels/{segredos}/messages")).await;
        assert!(msgs.iter().any(|m| m.id == old_msg), "round {round}: Carla reads the old history");
        assert!(hub.channel_ids(&bt).await.contains(&geral));
        server.shutdown().await;
    }
    let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
    db.call_now(|c| {
        assert_eq!(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?, voxhub_server::db::SCHEMA_VERSION, "no version bump");
        assert_eq!(voxhub_server::access::min_hub_version(c)?.as_deref(), Some(voxhub_server::access::PRIVATE_MIN_HUB_VERSION));
        Ok(())
    })
    .unwrap();
    drop(db);

    // ---- downgrade with a private channel: the v0.8.2 hub must not start (it would show it to all)
    let port = free_port();
    config(&new_dir, port);
    old_refuses(&bin, &new_dir, port).await;

    // ---- public again on the new hub: the guard goes
    let port = free_port();
    config(&new_dir, port);
    let hub = Hub::at(&http, port);
    let server = start(new_hub(&new_dir)).await.unwrap();
    let _: OkResponse = hub.ok(&dt, &format!("/api/admin/channels/{segredos}/access"), json!({})).await;
    assert!(hub.channel_ids(&bt).await.contains(&segredos));
    server.shutdown().await;

    // ---- now the v0.8.2 hub runs on the file again and reads everything
    let port = free_port();
    config(&new_dir, port);
    let old = run_old(&bin, &new_dir, port).await;
    let hub = Hub::at(&http, port);
    let ids = hub.channel_ids(&bt).await;
    assert!(ids.contains(&segredos) && ids.contains(&geral) && ids.iter().all(|id| *id > 0), "{ids:?}");
    let msgs: Vec<Message> = hub.get(&bt, &format!("/api/channels/{segredos}/messages")).await;
    assert!(msgs.iter().any(|m| m.id == old_msg));
    hub.say(&bt, geral, "de volta ao v0.8.2").await;
    drop(old);

    // ---- and the new hub once more
    let port = free_port();
    config(&new_dir, port);
    let hub = Hub::at(&http, port);
    let server = start(new_hub(&new_dir)).await.unwrap();
    let msgs: Vec<Message> = hub.get(&bt, &format!("/api/channels/{geral}/messages")).await;
    assert!(msgs.iter().any(|m| m.content == "de volta ao v0.8.2"));
    server.shutdown().await;
}
