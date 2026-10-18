//! Mentions / replies / reactions migration safety, against a database written by the real v0.7.0
//! hub. Same method as `attachments_migration.rs`: `releases/v0.7.0/voxhub-server.exe` (or
//! `VOX_V070_HUB`, or a v0.7.0 hub built from the `v0.7.0` tag into
//! `target-v02/v070-hub/voxhub-server.exe`) runs on a temp dir, people log in, chat and DM, it is
//! killed like a crash, and the new hub opens a *copy* (twice): the old sessions, messages and DMs
//! are there, and mentions, replies and reactions work and survive a restart. Then the v0.7.0 hub
//! runs on the migrated copy (downgrade): it reads everything (a reply is a plain message), writes,
//! and deletes a message that has reactions; back on the new hub, those reactions are gone (the
//! triggers ran under the old hub) and the reply's quote says "deleted". Skipped when no old
//! binary is found.

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
    std::env::var_os("VOX_V070_HUB")
        .map(PathBuf::from)
        .into_iter()
        .chain([root.join("releases/v0.7.0/voxhub-server.exe"), root.join("target-v02/v070-hub/voxhub-server.exe")])
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

async fn login(http: &reqwest::Client, port: u16, name: &str) -> (String, i64) {
    let r = http
        .post(format!("http://127.0.0.1:{port}/api/login"))
        .json(&json!({"username": name, "server_password": PASSWORD, "protocol_version": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v: Value = r.json().await.unwrap();
    (v["token"].as_str().unwrap().to_string(), v["user"]["id"].as_i64().unwrap())
}

/// Send over the WebSocket (the v0.7.0 app's way, old shape) and wait for it to come back.
async fn say(port: u16, token: &str, channel: i64, text: &str) -> i64 {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws?token={token}")).await.unwrap();
    ws.send(WsMsg::Text(json!({"type": "send_message", "channel_id": channel, "content": text}).to_string().into())).await.unwrap();
    loop {
        let Some(Ok(WsMsg::Text(t))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap() else { continue };
        let v: Value = serde_json::from_str(&t).unwrap();
        if v["type"] == "message_created" && v["message"]["content"] == text {
            return v["message"]["id"].as_i64().unwrap();
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

fn new_hub(dir: &Path) -> Options {
    Options { data_dir: dir.to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }
}

#[tokio::test]
async fn a_v070_database_gets_mentions_replies_and_reactions() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.7.0 hub binary (set VOX_V070_HUB)");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let (old_dir, new_dir) = (tmp.path().join("v070"), tmp.path().join("copy"));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::new();

    // ---- the v0.7.0 hub writes its database (channel messages and a DM)
    let port = free_port();
    config(&old_dir, port);
    let old = run_old(&bin, &old_dir, port).await;
    let (bt, bruno) = login(&http, port, "Bruno").await;
    let (ct, carla) = login(&http, port, "Carla").await;
    let chs: Vec<Channel> = http.get(format!("http://127.0.0.1:{port}/api/channels")).bearer_auth(&bt).send().await.unwrap().json().await.unwrap();
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    // A v0.7.0 hub has no reactions: the route does not exist.
    let r = http.post(format!("http://127.0.0.1:{port}/api/messages/1/reactions")).bearer_auth(&bt).json(&json!({"emoji": "👍", "add": true})).send().await.unwrap();
    assert!(matches!(r.status(), StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED), "{}", r.status());
    let first = say(port, &bt, geral, "oi do v0.7.0 @Carla").await;
    let dm: Value = http.post(format!("http://127.0.0.1:{port}/api/dms/{carla}")).bearer_auth(&bt).json(&json!({})).send().await.unwrap().json().await.unwrap();
    let dm_id = dm["id"].as_i64().unwrap();
    let r = http.post(format!("http://127.0.0.1:{port}/api/dms/{dm_id}/messages")).bearer_auth(&bt).json(&json!({"content": "dm do v0.7.0"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let old_line = r.json::<Value>().await.unwrap()["id"].as_i64().unwrap();
    drop(old); // killed like a crash: the last writes may only be in the WAL
    copy_db(&old_dir, &new_dir);

    // ---- the new hub opens the copy, twice
    let port = free_port();
    config(&new_dir, port);
    let base = format!("http://127.0.0.1:{port}");
    let mut reply_id = 0;
    for round in 0..2 {
        let server = start(new_hub(&new_dir)).await.unwrap();
        let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&ct).send().await.unwrap().json().await.unwrap();
        let lines: Vec<DmMessage> = http.get(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&ct).send().await.unwrap().json().await.unwrap();
        if round == 0 {
            // The old rows read with nothing new on them.
            assert_eq!(msgs.len(), 1);
            let m = &msgs[0];
            assert_eq!((m.id, m.content.as_str()), (first, "oi do v0.7.0 @Carla"));
            assert!(m.mentions.is_empty() && !m.mention_everyone && m.reply.is_none() && m.reactions.is_empty());
            assert!(lines[0].reply.is_none() && lines[0].reactions.is_empty());
            // Everything new works on it.
            let r = http
                .post(format!("{base}/api/channels/{geral}/messages"))
                .bearer_auth(&ct)
                .json(&json!({"content": "respondendo @Bruno", "mentions": [bruno], "reply_to": first, "reply_mention": true}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK);
            let reply: Message = r.json().await.unwrap();
            assert_eq!(reply.mentions, vec![bruno]);
            assert_eq!(reply.reply.as_ref().unwrap().author.as_ref().unwrap().id, bruno);
            reply_id = reply.id;
            for (who, emoji) in [(&ct, "👍"), (&bt, "🎉")] {
                let r = http.post(format!("{base}/api/messages/{first}/reactions")).bearer_auth(who).json(&json!({"emoji": emoji, "add": true})).send().await.unwrap();
                assert_eq!(r.status(), StatusCode::OK);
            }
            let r = http.post(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&ct).json(&json!({"content": "resposta no dm", "reply_to": old_line})).send().await.unwrap();
            assert_eq!(r.json::<DmMessage>().await.unwrap().reply.unwrap().id, old_line);
            let r = http.post(format!("{base}/api/dms/{dm_id}/messages/{old_line}/reactions")).bearer_auth(&ct).json(&json!({"emoji": "❤", "add": true})).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::OK);
        } else {
            // Second start: all of it is still there (the old sessions too).
            assert_eq!(msgs.len(), 2);
            let reply = msgs.iter().find(|m| m.id == reply_id).unwrap();
            assert_eq!((reply.mentions.clone(), reply.reply.as_ref().unwrap().id), (vec![bruno], first));
            let orig = msgs.iter().find(|m| m.id == first).unwrap();
            assert_eq!(orig.reactions.iter().map(|r| r.emoji.as_str()).collect::<Vec<_>>(), ["👍", "🎉"]);
            assert_eq!(lines.iter().find(|l| l.id == old_line).unwrap().reactions.len(), 1);
            assert_eq!(lines.first().unwrap().reply.as_ref().unwrap().id, old_line);
        }
        server.shutdown().await;
    }

    // ---- downgrade: the v0.7.0 hub runs on the migrated copy
    let port = free_port();
    config(&new_dir, port);
    let old = run_old(&bin, &new_dir, port).await;
    let msgs: Vec<Value> = http.get(format!("http://127.0.0.1:{port}/api/channels/{geral}/messages")).bearer_auth(&bt).send().await.unwrap().json().await.unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["content"], "respondendo @Bruno", "the reply reads as a plain message");
    assert!(msgs[0].get("reply").is_none() && msgs[1].get("reactions").is_none());
    say(port, &bt, geral, "de volta ao v0.7.0").await;
    // It deletes the reacted message (its own SQL: no idea reactions exist).
    let r = http.post(format!("http://127.0.0.1:{port}/api/messages/{first}/delete")).bearer_auth(&bt).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = http.post(format!("http://127.0.0.1:{port}/api/dms/{dm_id}/messages/{old_line}/delete")).bearer_auth(&bt).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    drop(old);

    // ---- and the new hub again: the triggers cleaned up after the old one
    let port = free_port();
    config(&new_dir, port);
    let server = start(new_hub(&new_dir)).await.unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let msgs: Vec<Message> = http.get(format!("{base}/api/channels/{geral}/messages")).bearer_auth(&ct).send().await.unwrap().json().await.unwrap();
    assert_eq!(msgs.iter().find(|m| m.id == reply_id).unwrap().reply, Some(ReplyRef::gone(first)));
    assert!(msgs.iter().any(|m| m.content == "de volta ao v0.7.0" && m.reply.is_none() && m.mentions.is_empty()));
    let lines: Vec<DmMessage> = http.get(format!("{base}/api/dms/{dm_id}/messages")).bearer_auth(&ct).send().await.unwrap().json().await.unwrap();
    assert_eq!(lines.first().unwrap().reply, Some(ReplyRef::gone(old_line)));
    server.shutdown().await;
    let db = voxhub_server::db::Db::open(&new_dir.join("voxhub.db")).unwrap();
    db.call_now(|c| {
        assert_eq!(c.query_row("SELECT COUNT(*) FROM reactions", [], |r| r.get::<_, i64>(0))?, 0, "the deleted message's reactions went with it");
        assert_eq!(c.query_row("SELECT COUNT(*) FROM dm_reactions", [], |r| r.get::<_, i64>(0))?, 0);
        Ok(())
    })
    .unwrap();
}
