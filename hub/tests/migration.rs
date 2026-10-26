//! Item 12 migration safety, against a database written by the real v0.4.1 hub.
//!
//! The released `releases/v0.4.1/voxhub-server.exe` (or `VOX_V041_HUB`) runs on a temp dir: two
//! people log in and chat. It is then killed (as in a crash: its WAL is not checkpointed), the
//! files are copied, and the new hub opens the *copy*: everything is still there (users, their
//! open sessions, channels, messages), the new features work on it, a second start changes
//! nothing, and the v0.4.1 hub still runs on the migrated copy (downgrade). Skipped when the
//! old binary is not there. The soundboard tables are created on the copy too: a sound can be
//! added, survives a restart and is served, and the old hub still runs with those tables present.

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
    let p = std::env::var_os("VOX_V041_HUB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../releases/v0.4.1/voxhub-server.exe"));
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

/// Runs the v0.4.1 hub; killed on drop (only this exact child).
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

async fn login(http: &reqwest::Client, port: u16, name: &str) -> Value {
    // Exactly what a v0.4.1 client sends.
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
    // Wait for our own message to come back (it is saved by then).
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
async fn a_v041_database_migrates_and_stays_usable() {
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
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let ana = login(&http, port, "Ana").await;
    let bruno = login(&http, port, "Bruno").await;
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
    // A v0.4.1 hub has no soundboard: the 404 is how a new client knows to hide the feature.
    let r = http.get(format!("http://127.0.0.1:{port}/api/sounds")).bearer_auth(ana["token"].as_str().unwrap()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    say(port, ana["token"].as_str().unwrap(), geral, "oi do v0.4.1").await;
    say(port, bruno["token"].as_str().unwrap(), geral, "olá").await;
    drop(old); // killed like a crash: the last writes may only be in the WAL
    copy_db(&old_dir, &new_dir);

    // ---- the new hub opens the copy
    let port = free_port();
    config(&new_dir, port);
    for round in 0..2 {
        let server = start(Options { data_dir: new_dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
        let base = format!("http://127.0.0.1:{port}");
        let bruno_token = bruno["token"].as_str().unwrap();
        // The v0.4.1 session is still valid, and everything is where it was.
        let r = http.get(format!("{base}/api/channels")).bearer_auth(bruno_token).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "round {round}: an old session survives the migration");
        assert_eq!(r.json::<Vec<Channel>>().await.unwrap(), chs);
        let msgs: Vec<Message> =
            http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(bruno_token).send().await.unwrap().json().await.unwrap();
        let texts: Vec<(&str, &str, Role)> = msgs.iter().map(|m| (m.author.username.as_str(), m.content.as_str(), m.author.role)).collect();
        assert_eq!(texts, [("Bruno", "olá", Role::Member), ("Ana", "oi do v0.4.1", Role::Owner)]);
        if round == 0 {
            // The new features work on the migrated file: the owner protects her name...
            let r = http
                .post(format!("{base}/api/login"))
                .json(&json!({"username": "ana", "server_password": PASSWORD, "protocol_version": 1}))
                .send()
                .await
                .unwrap();
            let owner: LoginResponse = r.json().await.unwrap();
            assert_eq!(owner.user.id, ana["user"]["id"].as_i64().unwrap(), "same user, same id");
            let r = http
                .post(format!("{base}/api/account/password"))
                .bearer_auth(&owner.token)
                .json(&json!({"new": "senha-da-ana"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            // ...which ends her v0.4.1 session, and makes Bruno a moderator.
            let r = http.get(format!("{base}/api/channels")).bearer_auth(ana["token"].as_str().unwrap()).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
            let r = http
                .post(format!("{base}/api/admin/users/{}/role", bruno["user"]["id"]))
                .bearer_auth(&owner.token)
                .json(&json!({"role": "moderator"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::CONFLICT, "Bruno has no personal password yet");
            // The soundboard starts empty on a migrated file, and takes an upload from a v0.4.1 session.
            let sounds: Vec<Sound> = http.get(format!("{base}/api/sounds")).bearer_auth(bruno_token).send().await.unwrap().json().await.unwrap();
            assert!(sounds.is_empty());
            let wav = canonical_wav(1, &vec![500i16; 9600]);
            let r = http
                .post(format!("{base}/api/sounds?name=migrado&emoji=%F0%9F%8E%BA"))
                .bearer_auth(bruno_token)
                .header("content-type", "audio/wav")
                .body(wav.clone())
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let sound: Sound = r.json().await.unwrap();
            assert_eq!((sound.name.as_str(), sound.uploader.username.as_str()), ("migrado", "Bruno"));
        } else {
            // Second start: nothing changed, the password from the first round still protects Ana.
            let r = http
                .post(format!("{base}/api/login"))
                .json(&json!({"username": "Ana", "server_password": PASSWORD, "protocol_version": 1}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
            // The sound added in the first round is still there, with its audio.
            let sounds: Vec<Sound> = http.get(format!("{base}/api/sounds")).bearer_auth(bruno_token).send().await.unwrap().json().await.unwrap();
            assert_eq!(sounds.len(), 1, "round {round}");
            let audio = http.get(format!("{base}/api/sounds/{}/audio", sounds[0].id)).bearer_auth(bruno_token).send().await.unwrap();
            assert_eq!(audio.bytes().await.unwrap().as_ref(), canonical_wav(1, &vec![500i16; 9600]).as_slice());
        }
        server.shutdown().await;
    }

    // ---- downgrade: the v0.4.1 hub still runs on the migrated copy
    let port = free_port();
    config(&new_dir, port);
    let old = run_old(&bin, &new_dir, port).await;
    let carla = login(&http, port, "Carla").await;
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
    say(port, carla["token"].as_str().unwrap(), geral, "de volta ao v0.4.1").await;
    drop(old);
}

// ---------------------------------------------------------------- "quem hospeda vira dono"

fn config_no_owner(dir: &Path, port: u16) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "server_name = \"mig\"\nserver_password = \"{PASSWORD}\"\nbind = \"127.0.0.1:{port}\"\n\
             [livekit]\napi_key = \"APImig000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
}

/// The first event of a WebSocket opened with `token`, optionally "through a proxy" (the header
/// vetoes the automatic claim; without it the connection is the server PC's own app).
async fn ready_event(port: u16, token: &str, proxied: bool) -> Value {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://127.0.0.1:{port}/ws?token={token}").into_client_request().unwrap();
    if proxied {
        req.headers_mut().insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    loop {
        let Some(Ok(WsMsg::Text(t))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap() else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "ready" {
            return v;
        }
    }
}

/// The live server's situation: a database written by v0.4.1 (users, chat, open sessions) and a
/// config with no `owner`. After the update the host claims the server from the server PC, even
/// with the session its app already had; everything is additive, repeatable, and the old hubs
/// still run on the file.
#[tokio::test]
async fn a_v041_database_with_no_owner_is_claimed_by_the_server_pc() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.4.1 hub binary (set VOX_V041_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v041"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- v0.4.1 writes its database: no roles, no owner anywhere
    let port = free_port();
    config_no_owner(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let dono = login(&http, port, "dono").await;
    let ana = login(&http, port, "Ana").await;
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
    say(port, dono["token"].as_str().unwrap(), geral, "oi do v0.4.1").await;
    drop(old);
    copy_db(&old_dir, &new_dir);

    // ---- the new hub, still with no `owner` in config.toml
    let port = free_port();
    config_no_owner(&new_dir, port);
    let base = format!("http://127.0.0.1:{port}");
    let (dono_token, ana_token) = (dono["token"].as_str().unwrap().to_string(), ana["token"].as_str().unwrap().to_string());
    for round in 0..2 {
        let server = start(Options { data_dir: new_dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
        if round == 0 {
            assert!(server.claim_code().is_some(), "no owner yet: the console shows a code");
            // Everything of v0.4.1 is there, nobody is the owner, and the old sessions are valid.
            let r = http.get(format!("{base}/api/channels")).bearer_auth(&ana_token).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&ana_token).send().await.unwrap().json().await.unwrap();
            assert_eq!((msgs.len(), msgs[0].author.username.as_str(), msgs[0].author.role), (1, "dono", Role::Member));
            // Ana's app (through a proxy: not the server PC) sees a server with no owner...
            let ready = ready_event(port, &ana_token, true).await;
            assert_eq!(ready["account"]["no_owner"], true, "{ready}");
            assert_eq!(ready["user"].get("role"), None);
            // ...and the host's app, reconnecting with the session it had before the update, claims it.
            let ready = ready_event(port, &dono_token, false).await;
            assert_eq!(ready["user"]["role"], "owner", "{ready}");
            assert_eq!(ready["account"]["must_set_password"], true, "{ready}");
            assert_eq!(ready["account"].get("no_owner"), None, "{ready}");
            assert_eq!(server.claim_code(), None);
            let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&ana_token).send().await.unwrap().json().await.unwrap();
            assert_eq!(msgs[0].author.role, Role::Owner, "chat history shows the new owner's role");
            // The owner protects the name; nothing else is needed.
            let r = http.post(format!("{base}/api/account/password")).bearer_auth(&dono_token).json(&json!({"new": "dono-senha"})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let r = http.get(format!("{base}/api/admin/audit")).bearer_auth(&dono_token).send().await.unwrap();
            let log: Vec<AuditEntry> = r.json().await.unwrap();
            assert!(log.iter().any(|e| e.action == AUDIT_CLAIM_OWNER && e.detail == "pc-do-servidor"));
        } else {
            // A second start changes nothing: no new code, the same owner, nobody else can take over.
            assert_eq!(server.claim_code(), None);
            let r = http
                .post(format!("{base}/api/login"))
                .json(&json!({"username": "dono", "server_password": PASSWORD, "protocol_version": 1}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "the owner's name is protected");
            let r = http
                .post(format!("{base}/api/login"))
                .json(&json!({"username": "dono", "server_password": PASSWORD, "protocol_version": 1, "password": "dono-senha"}))
                .send()
                .await
                .unwrap();
            let owner: LoginResponse = r.json().await.unwrap();
            assert_eq!(owner.user.role, Role::Owner);
            let r = http.post(format!("{base}/api/login")).json(&json!({"username": "Ana", "server_password": PASSWORD, "protocol_version": 1})).send().await.unwrap();
            let again: LoginResponse = r.json().await.unwrap();
            assert_eq!(again.user.role, Role::Member, "the server PC is not an owner factory");
        }
        // The migration left the schema where it should be, any number of starts.
        server.shutdown().await;
        let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
        db.call_now(|c| {
            assert_eq!(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?, voxhub_server::db::SCHEMA_VERSION);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM server_meta WHERE key = 'owner_user_id'", [], |r| r.get::<_, i64>(0))?, 1);
            Ok(())
        })
        .unwrap();
    }

    // ---- downgrade: the v0.4.1 hub (and the v0.5.0 one, if it is here) still run on the migrated file
    let mut old_bins = vec![bin];
    let v050 = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../releases/v0.5.0/voxhub-server.exe");
    if v050.exists() {
        old_bins.push(v050);
    }
    for bin in old_bins {
        let port = free_port();
        config_no_owner(&new_dir, port);
        let old = run_old(&bin, &new_dir, port).await;
        let carla = login(&http, port, "Carla").await;
        let msgs: Vec<Value> = http
            .get(format!("http://127.0.0.1:{port}/api/channels/{geral}/messages"))
            .bearer_auth(carla["token"].as_str().unwrap())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(!msgs.is_empty(), "{}", bin.display());
        drop(old);
    }
}
