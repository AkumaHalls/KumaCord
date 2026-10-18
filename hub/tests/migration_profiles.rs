//! Profiles migration safety, against a database written by the real v0.5.2 hub.
//!
//! The released `releases/v0.5.2/voxhub-server.exe` (or `VOX_V052_HUB`) runs on a temp dir: two
//! people log in, one with a profile picture, and they chat. It is then killed (as in a crash: its
//! WAL is not checkpointed), the files are copied, and the new hub opens the *copy* twice: the
//! users, their open sessions, pictures and messages are all still there, a v0.5.2 client's login
//! reads the same as before, the profile features work and survive another start, the schema is
//! additive (a column on `users`, one new table), and the v0.5.2 hub still runs on the migrated
//! copy with profile data in it (downgrade). Skipped when the old binary is not there.

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
        assert!(t.elapsed() < Duration::from_secs(15), "the v0.5.2 hub did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    hub
}

/// Exactly what a v0.5.2 client sends and reads.
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

fn picture() -> Vec<u8> {
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(48, 48, |x, y| image::Rgb([(x * 5) as u8, (y * 5) as u8, 90])));
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

#[tokio::test]
async fn a_v052_database_gets_profiles_and_still_runs_the_old_hub() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.5.2 hub binary (set VOX_V052_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v052"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- the v0.5.2 hub writes its database: a picture, chat, open sessions
    let port = free_port();
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let ana = login(&http, port, "Ana").await;
    let bruno = login(&http, port, "Bruno").await;
    let (ana_token, bruno_token) = (ana["token"].as_str().unwrap().to_string(), bruno["token"].as_str().unwrap().to_string());
    let r = http.put(format!("http://127.0.0.1:{port}/api/me/avatar")).bearer_auth(&bruno_token).body(picture()).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let bruno_avatar = r.json::<AvatarResponse>().await.unwrap().avatar.unwrap();
    // A v0.5.2 hub has no profile routes: the 404 is how a new client knows to hide the feature.
    let r = http.get(format!("http://127.0.0.1:{port}/api/users/1/profile")).bearer_auth(&ana_token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let chs: Vec<Channel> = http.get(format!("http://127.0.0.1:{port}/api/channels")).bearer_auth(&ana_token).send().await.unwrap().json().await.unwrap();
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    say(port, &ana_token, geral, "oi do v0.5.2").await;
    say(port, &bruno_token, geral, "olá").await;
    drop(old); // killed like a crash: the last writes may only be in the WAL
    copy_db(&old_dir, &new_dir);

    // ---- the new hub opens the copy, twice
    let port = free_port();
    config(&new_dir, port);
    let base = format!("http://127.0.0.1:{port}");
    for round in 0..2 {
        let server = start(Options { data_dir: new_dir.clone(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
        // The v0.5.2 sessions are still valid, and everything is where it was.
        let r = http.get(format!("{base}/api/channels")).bearer_auth(&bruno_token).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "round {round}: an old session survives the migration");
        let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&bruno_token).send().await.unwrap().json().await.unwrap();
        let by: Vec<(&str, Option<&str>, Option<&str>)> =
            msgs.iter().map(|m| (m.author.username.as_str(), m.author.avatar.as_deref(), m.author.display_name.as_deref())).collect();
        let (ana_name, bruno_name) = if round == 0 { (None, None) } else { (Some("Ana Maria"), Some("Bruninho")) };
        assert_eq!(by, [("Bruno", Some(bruno_avatar.as_str()), bruno_name), ("Ana", None, ana_name)], "round {round}");
        // A v0.5.2 client's view of a user nobody gave a display name: exactly the old JSON.
        let r = http.post(format!("{base}/api/login")).json(&json!({"username": "Carla", "server_password": PASSWORD, "protocol_version": 1})).send().await.unwrap();
        let carla: Value = r.json().await.unwrap();
        assert_eq!(carla["user"].get("display_name"), None, "{carla}");
        assert_eq!(carla["user"].as_object().unwrap().len(), 2, "id and username only: {carla}");
        let carla_token = carla["token"].as_str().unwrap();

        if round == 0 {
            // The profile features work on the migrated file.
            let r = http.get(format!("{base}/api/users/{}/profile", ana["user"]["id"])).bearer_auth(carla_token).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let p: UserProfile = r.json().await.unwrap();
            assert_eq!((p.user.username.as_str(), p.user.role, p.bio.as_str(), p.status, p.online), ("Ana", Role::Owner, "", None, false));
            assert!(p.member_since_ms > 0, "the join date is the old user's created_at_ms");
            // Ana (the owner) must set a personal password before her staff powers count; as a plain
            // profile owner she can edit regardless, with the session from the old hub.
            for (token, body) in [
                (&ana_token, json!({"display_name": "Ana Maria", "bio": "dona da casa", "accent": "#336699"})),
                (&bruno_token, json!({"display_name": "Bruninho"})),
            ] {
                let r = http.put(format!("{base}/api/me/profile")).bearer_auth(token).json(&body).send().await.unwrap();
                assert_eq!(r.status(), StatusCode::OK);
            }
            let r = http.put(format!("{base}/api/me/status")).bearer_auth(&ana_token).json(&json!({"emoji": "🎮", "text": "jogando"})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let r = http.put(format!("{base}/api/me/presence")).bearer_auth(&bruno_token).json(&json!({"presence": "dnd"})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        } else {
            // Second start: the profile data of the first is all still there.
            let p: UserProfile = http.get(format!("{base}/api/users/{}/profile", ana["user"]["id"])).bearer_auth(carla_token).send().await.unwrap().json().await.unwrap();
            assert_eq!((p.user.display_name.as_deref(), p.bio.as_str(), p.accent.as_deref()), (Some("Ana Maria"), "dona da casa", Some("#336699")));
            assert_eq!(p.status.map(|s| s.text), Some("jogando".to_string()));
            let (mut bruno_ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws?token={bruno_token}")).await.unwrap();
            // Bruno chose "não perturbe": it comes back with him.
            let mut dnd = false;
            for _ in 0..4 {
                if let Ok(Some(Ok(WsMsg::Text(t)))) = tokio::time::timeout(Duration::from_secs(3), bruno_ws.next()).await {
                    dnd |= matches!(serde_json::from_str::<ServerEvent>(&t), Ok(ServerEvent::PresenceChanged { presence: Presence::Dnd, .. }));
                }
            }
            assert!(dnd, "the stored presence is applied on the next connection");
        }
        // The migration left the schema where it should be, any number of starts.
        server.shutdown().await;
        let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
        db.call_now(|c| {
            assert_eq!(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?, voxhub_server::db::SCHEMA_VERSION);
            let cols: Vec<String> = c.prepare("PRAGMA table_info(users)")?.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_, _>>()?;
            assert!(cols.contains(&"display_name".to_string()), "{cols:?}");
            // The original columns are untouched (a downgraded hub reads the same ones).
            for old in ["id", "username", "created_at_ms", "role", "password_hash", "avatar_version"] {
                assert!(cols.contains(&old.to_string()), "{old} in {cols:?}");
            }
            assert_eq!(c.query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get::<_, i64>(0))?, 2);
            Ok(())
        })
        .unwrap();
    }

    // ---- downgrade: the v0.5.2 hub still runs on the migrated copy, profile data and all
    let port = free_port();
    config(&new_dir, port);
    let old = run_old(&bin, &new_dir, port).await;
    let dora = login(&http, port, "Dora").await;
    let msgs: Vec<Value> = http
        .get(format!("http://127.0.0.1:{port}/api/channels/{geral}/messages"))
        .bearer_auth(dora["token"].as_str().unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(msgs.len(), 2);
    // The old hub knows nothing about display names: it still names people by username.
    assert!(msgs.iter().all(|m| m["author"].get("display_name").is_none()), "{msgs:?}");
    say(port, dora["token"].as_str().unwrap(), geral, "de volta ao v0.5.2").await;
    drop(old);
}
