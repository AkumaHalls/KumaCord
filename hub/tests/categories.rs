//! v1.1 regression: the `Ready` snapshot must carry voice channels and the category they are in.
//! Guards the "canais de voz sumiram" bug (the sidebar groups by `category_id`; if the hub stops
//! sending it, voice channels land in a group the app cannot see).
//! Real server on an ephemeral port, no LiveKit.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "hunter2";

struct Env {
    server: Server,
    base: String,
    http: reqwest::Client,
    _dir: tempfile::TempDir,
}

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"cats\"\nserver_password = \"{PASSWORD}\"\nowner = \"dono\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let server = start(Options {
        data_dir: dir.path().to_path_buf(),
        no_livekit: true,
        livekit_bin: "unused".into(),
        bind_override: Some("127.0.0.1:0".into()),
    })
    .await
    .unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    Env { server, base, http: reqwest::Client::new(), _dir: dir }
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn login(&self, user: &str) -> LoginResponse {
        let req = LoginRequest { username: user.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        let r = self.http.post(self.url("/api/login")).json(&req).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn ws(&self, token: &str) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={token}", self.base)).await.unwrap();
        Ws(ws)
    }
}

struct Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>);

impl Ws {
    async fn next(&mut self) -> Option<ServerEvent> {
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.0.next()).await.ok()?? {
                Ok(WsMsg::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
                Ok(WsMsg::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn expect(&mut self) -> ServerEvent {
        self.next().await.expect("expected an event")
    }
}

#[tokio::test]
async fn ready_snapshot_has_voice_channels_with_their_categories() {
    let env = setup().await;
    let ana = env.login("dono").await;
    // The owner's powers stay off until the personal password is set (item 12).
    let pw = env.http.post(env.url("/api/account/password")).bearer_auth(&ana.token).json(&json!({ "current": null, "new": "dono-senha", "remember_device": false })).send().await.unwrap();
    assert_eq!(pw.status(), StatusCode::OK);

    // A voice room, then a category, straight through the admin API.
    let made: serde_json::Value =
        env.http.post(env.url("/api/admin/channels")).bearer_auth(&ana.token).json(&json!({ "name": "Sala Teste", "kind": "voice" })).send().await.unwrap().json().await.unwrap();
    assert!(made["id"].is_i64(), "canal de voz criado: {made}");
    let cat: serde_json::Value = env
        .http
        .post(env.url("/api/admin/categories"))
        .bearer_auth(&ana.token)
        .json(&json!({ "name": "Jogos", "color": "#5865F2", "position": 0 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(cat["id"].is_i64(), "categoria criada: {cat}");

    // The snapshot the app would receive on connect.
    let mut ws = env.ws(&ana.token).await;
    let ServerEvent::Ready { channels, categories, .. } = ws.expect().await else {
        panic!("o primeiro evento tem de ser Ready");
    };

    let voices: Vec<_> = channels.iter().filter(|c| c.kind == ChannelKind::Voice).collect();
    assert!(!voices.is_empty(), "o Ready tem de trazer os canais de voz: {channels:?}");
    let known: Vec<ChannelId> = categories.as_ref().map(|cs| cs.iter().map(|c| c.id).collect()).unwrap_or_default();
    for v in &voices {
        if let Some(id) = v.category_id {
            assert!(known.contains(&id), "canal de voz {v:?} numa categoria que o Ready não manda: {categories:?}");
        }
    }

    // Assign the voice room to the category and re-login: it comes back grouped.
    let cat_id = cat["id"].as_i64().unwrap();
    let voice_id = voices[0].id;
    let st = env
        .http
        .post(env.url(&format!("/api/admin/channels/{voice_id}/category")))
        .bearer_auth(&ana.token)
        .json(&json!({ "category_id": cat_id }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(st, StatusCode::OK, "atribuir canal à categoria deve funcionar");

    let bia = env.login("bia").await;
    let mut ws2 = env.ws(&bia.token).await;
    let ServerEvent::Ready { channels, categories, .. } = ws2.expect().await else {
        panic!("Ready esperado");
    };
    let voice = channels.iter().find(|c| c.id == voice_id).expect("a sala de voz volta no Ready");
    assert_eq!(voice.category_id, Some(ChannelId(cat_id)), "a sala de voz vem na categoria atribuída");
    let cats = categories.expect("o hub tem categorias");
    assert!(cats.iter().any(|c| c.id == ChannelId(cat_id)), "a categoria vai no Ready: {cats:?}");
    env.server.shutdown().await;
}
