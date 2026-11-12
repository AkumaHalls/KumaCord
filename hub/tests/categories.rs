//! v1.1 regression: the `Ready` snapshot must carry voice channels and the category they are in.
//! Guards the "canais de voz sumiram" bug (the sidebar groups by `category_id`; if the hub stops
//! sending it, voice channels land in a group the app cannot see).
//! Real server on an ephemeral port, no LiveKit.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
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

    /// Skip events until one matches (5 s at most).
    async fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&ServerEvent) -> bool) -> ServerEvent {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            match self.next().await {
                Some(ev) if pred(&ev) => return ev,
                Some(_) => {}
                None => break,
            }
        }
        panic!("no {what} event");
    }

    /// Nothing matching `pred` arrives within `wait`.
    async fn quiet_for(&mut self, wait: Duration, mut pred: impl FnMut(&ServerEvent) -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < wait {
            let left = wait.saturating_sub(t.elapsed());
            match tokio::time::timeout(left, self.0.next()).await.ok().flatten() {
                Some(Ok(WsMsg::Text(text))) => {
                    let ev: ServerEvent = serde_json::from_str(&text).unwrap();
                    if pred(&ev) {
                        return false;
                    }
                }
                _ => break,
            }
        }
        true
    }
}

/// A POST with a bearer token, returning only the status (the permission checks).
async fn post_status(env: &Env, token: &str, path: &str, body: serde_json::Value) -> StatusCode {
    env.http.post(env.url(path)).bearer_auth(token).json(&body).send().await.unwrap().status()
}

/// One category, created through the admin API (the app's "Nova categoria" form).
async fn make_category(env: &Env, token: &str, name: &str, position: i64) -> ChannelId {
    let made: serde_json::Value = env
        .http
        .post(env.url("/api/admin/categories"))
        .bearer_auth(token)
        .json(&json!({ "name": name, "color": "#5865F2", "position": position }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    ChannelId(made["id"].as_i64().expect("categoria criada"))
}

/// A channel created through the admin API.
async fn make_channel(env: &Env, token: &str, name: &str, kind: &str) -> Channel {
    env.http
        .post(env.url("/api/admin/channels"))
        .bearer_auth(token)
        .json(&json!({ "name": name, "kind": kind }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Put a channel in a category (or back out with `null`).
async fn set_category(env: &Env, token: &str, channel: ChannelId, category: Option<ChannelId>) -> StatusCode {
    env.http
        .post(env.url(&format!("/api/admin/channels/{channel}/category")))
        .bearer_auth(token)
        .json(&json!({ "category_id": category }))
        .send()
        .await
        .unwrap()
        .status()
}

/// The sidebar as a fresh connection sees it: every channel and every category, in order.
async fn structure_of(env: &Env, token: &str) -> (Vec<Channel>, Vec<ChannelCategory>) {
    let mut ws = env.ws(token).await;
    match ws.expect().await {
        ServerEvent::Ready { channels, categories, .. } => (channels, categories.expect("o hub tem categorias")),
        other => panic!("Ready esperado: {other:?}"),
    }
}

/// The sidebar's single top-level order: categories and standalone channels merged by position
/// (this is what the app draws, and what the old two-list reorder used to scramble).
fn top_level(chs: &[Channel], cats: &[ChannelCategory]) -> Vec<String> {
    let mut items: Vec<(i32, String)> = cats.iter().map(|c| (c.position, format!("categoria {}", c.name))).collect();
    items.extend(chs.iter().filter(|c| c.category_id.is_none()).map(|c| (c.position, format!("canal {}", c.name))));
    items.sort_by_key(|(p, _)| *p);
    items.into_iter().map(|(_, name)| name).collect()
}

/// The channels of one category, in the order the hub keeps them.
fn inside(chs: &[Channel], cat: ChannelId) -> Vec<String> {
    let mut mine: Vec<&Channel> = chs.iter().filter(|c| c.category_id == Some(cat)).collect();
    mine.sort_by_key(|c| c.position);
    mine.into_iter().map(|c| c.name.clone()).collect()
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

/// v1.2: the server half of the sidebar's drag & drop of categories. The order sent is the
/// order drawn — every category id exactly once — and it comes back to everybody at once in
/// `CategoriesChanged`.
#[tokio::test]
async fn reordering_categories_swaps_their_order_and_tells_everybody() {
    let env = setup().await;
    let dono = env.login("dono").await;
    let pw = env.http.post(env.url("/api/account/password")).bearer_auth(&dono.token).json(&json!({ "current": null, "new": "dono-senha", "remember_device": false })).send().await.unwrap();
    assert_eq!(pw.status(), StatusCode::OK);

    let mut ws = env.ws(&dono.token).await;
    assert!(matches!(ws.expect().await, ServerEvent::Ready { .. }), " Ready vem primeiro");

    let first = make_category(&env, &dono.token, "Jogos", 0).await;
    let second = make_category(&env, &dono.token, "Música", 1).await;
    let third = make_category(&env, &dono.token, "Estudos", 2).await;
    for _ in 0..3 {
        let ServerEvent::CategoriesChanged { categories } = ws.expect().await else {
            panic!("cada categoria criada avisa a todos");
        };
        assert!(!categories.is_empty(), "a lista de categorias não volta vazia");
    }

    // The new order, reversed: the drag & drop sends the whole list every time.
    let ids = vec![third, second, first];
    // A member may not reorder anything.
    let bia = env.login("bia").await;
    assert_eq!(post_status(&env, &bia.token, "/api/admin/categories/reorder", json!({"ids": ids})).await, StatusCode::FORBIDDEN);
    // ...and the list has to be a permutation of every id, exactly once.
    assert_eq!(post_status(&env, &dono.token, "/api/admin/categories/reorder", json!({"ids": &ids[1..]})).await, StatusCode::BAD_REQUEST);
    assert_eq!(post_status(&env, &dono.token, "/api/admin/categories/reorder", json!({"ids": [ids[0], ids[0], ids[1], ids[2]]})).await, StatusCode::BAD_REQUEST);
    assert_eq!(post_status(&env, &dono.token, "/api/admin/categories/reorder", json!({"ids": []})).await, StatusCode::BAD_REQUEST);

    assert_eq!(post_status(&env, &dono.token, "/api/admin/categories/reorder", json!({"ids": ids})).await, StatusCode::OK);
    let ServerEvent::CategoriesChanged { categories } = ws.expect().await else {
        panic!("a ordem nova vai para todos de uma vez");
    };
    assert_eq!(categories.iter().map(|c| c.id).collect::<Vec<_>>(), ids, "a ordem que o drag mandou é a que aparece");
    assert_eq!((categories[0].position, categories[1].position, categories[2].position), (0, 1, 2));

    // And the member sees it too (the hub is the only writer, everybody reads the same list).
    let mut ws2 = env.ws(&bia.token).await;
    let ServerEvent::Ready { categories: ready_cats, .. } = ws2.expect().await else {
        panic!("Ready esperado");
    };
    let ready_cats = ready_cats.expect("o hub tem categorias");
    assert_eq!(ready_cats.iter().map(|c| c.id).collect::<Vec<_>>(), ids, "um membro que conecta depois ve a ordem nova");
    env.server.shutdown().await;
}

/// 2026-11-11: `POST /api/admin/structure` — the whole sidebar order in ONE atomic batch, the way
/// the Discord client sends its single `PATCH /guilds/{id}/channels`. The old pair
/// (`/channels/reorder` + `/categories/reorder`) rewrites each list's positions on its own and
/// therefore destroys the interleaving between categories and loose channels; here the client sends
/// the complete state, so moving a loose channel no longer makes every category jump.
#[tokio::test]
async fn reordering_the_whole_structure_keeps_categories_and_loose_channels_interleaved() {
    let env = setup().await;
    let dono = env.login("dono").await;
    let pw = env.http.post(env.url("/api/account/password")).bearer_auth(&dono.token).json(&json!({ "current": null, "new": "dono-senha", "remember_device": false })).send().await.unwrap();
    assert_eq!(pw.status(), StatusCode::OK);

    // Two categories with channels inside them, and one loose channel ("geral" comes with the hub).
    let jogos = make_category(&env, &dono.token, "Jogos", 0).await;
    let musica = make_category(&env, &dono.token, "Música", 1).await;
    let sala3 = make_channel(&env, &dono.token, "Sala 3", "voice").await.id;
    assert_eq!(set_category(&env, &dono.token, sala3, Some(musica)).await, StatusCode::OK);
    let (chs, _) = structure_of(&env, &dono.token).await;
    let geral = chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let (sala1, sala2) = {
        let v: Vec<ChannelId> = chs.iter().filter(|c| c.kind == ChannelKind::Voice && c.category_id.is_none()).map(|c| c.id).collect();
        (v[0], v[1])
    };
    assert_eq!(set_category(&env, &dono.token, sala1, Some(jogos)).await, StatusCode::OK);
    assert_eq!(set_category(&env, &dono.token, sala2, Some(musica)).await, StatusCode::OK);

    // A member watches every broadcast from here on.
    let bia = env.login("bia").await;
    let mut watcher = env.ws(&bia.token).await;
    assert!(matches!(watcher.expect().await, ServerEvent::Ready { .. }));

    // The whole state at once: a category, the loose channel, the other category; the channels of
    // "Música" reversed and "Sala 3" (created last) first. The positions sent are deliberately
    // nonsense: the hub rewrites them densely and only the order matters.
    let body = json!({
        "top": [
            { "id": musica, "position": 99 },
            { "id": geral, "position": 7 },
            { "id": jogos, "position": 3 },
        ],
        "groups": [
            { "category_id": musica, "ids": [sala3, sala2] },
            { "category_id": jogos, "ids": [sala1] },
        ],
    });
    assert_eq!(post_status(&env, &bia.token, "/api/admin/structure", body.clone()).await, StatusCode::FORBIDDEN, "um membro não reordena nada");
    assert_eq!(post_status(&env, &dono.token, "/api/admin/structure", body).await, StatusCode::OK);

    // Both lists go out at once, to everybody.
    let ServerEvent::ChannelsChanged { channels: seen } = watcher.wait_for("ChannelsChanged", |e| matches!(e, ServerEvent::ChannelsChanged { .. })).await else { unreachable!() };
    assert!(!seen.is_empty(), "a lista de canais não volta vazia");
    let ServerEvent::CategoriesChanged { categories: seen_cats } = watcher.wait_for("CategoriesChanged", |e| matches!(e, ServerEvent::CategoriesChanged { .. })).await else { unreachable!() };
    assert_eq!(seen_cats.iter().map(|c| c.id).collect::<Vec<_>>(), [musica, jogos], "a ordem que o drag mandou é a que aparece");
    // The top level is ONE position sequence: the category, the loose channel, the other category.
    assert_eq!((seen_cats[0].position, seen_cats[1].position), (0, 2));

    // A fresh connection sees the interleaving: category, loose channel, category.
    let (chs, cats) = structure_of(&env, &dono.token).await;
    assert_eq!(top_level(&chs, &cats), ["categoria Música", "canal geral", "categoria Jogos"], "{chs:?} {cats:?}");
    // ...and inside "Música" the order sent, not the one the ids were created in.
    assert_eq!(inside(&chs, musica), ["Sala 3", "Sala 2"]);
    assert_eq!(inside(&chs, jogos), ["Sala 1"]);
    // The positions are dense: 0, 1, 2 at the top level and 0, 1 inside the category.
    let mut top: Vec<i32> = cats.iter().map(|c| c.position).chain(chs.iter().filter(|c| c.category_id.is_none()).map(|c| c.position)).collect();
    top.sort_unstable();
    assert_eq!(top, [0, 1, 2], "{chs:?} {cats:?}");
    let in_musica: Vec<i32> = chs.iter().filter(|c| c.category_id == Some(musica)).map(|c| c.position).collect();
    assert_eq!(in_musica, [0, 1]);

    // A category with nothing in it is a normal row: the app sends every category, empty ones
    // included, so `groups` carries it with an empty `ids` and that is not "missing a channel".
    let vazia = make_category(&env, &dono.token, "Vazia", 3).await;
    let body = json!({
        "top": [
            { "id": vazia, "position": 0 },
            { "id": musica, "position": 1 },
            { "id": geral, "position": 2 },
            { "id": jogos, "position": 3 },
        ],
        "groups": [
            { "category_id": vazia, "ids": [] },
            { "category_id": musica, "ids": [sala3, sala2] },
            { "category_id": jogos, "ids": [sala1] },
        ],
    });
    assert_eq!(post_status(&env, &dono.token, "/api/admin/structure", body).await, StatusCode::OK, "uma categoria vazia não é um pedido incompleto");
    // The two broadcasts this one earns (the watcher must be quiet again before the 400s below).
    watcher.wait_for("ChannelsChanged", |e| matches!(e, ServerEvent::ChannelsChanged { .. })).await;
    watcher.wait_for("CategoriesChanged", |e| matches!(e, ServerEvent::CategoriesChanged { .. })).await;
    let (chs, cats) = structure_of(&env, &dono.token).await;
    assert_eq!(top_level(&chs, &cats), ["categoria Vazia", "categoria Música", "canal geral", "categoria Jogos"], "{chs:?} {cats:?}");
    assert!(inside(&chs, vazia).is_empty());
    let mut top: Vec<i32> = cats.iter().map(|c| c.position).chain(chs.iter().filter(|c| c.category_id.is_none()).map(|c| c.position)).collect();
    top.sort_unstable();
    assert_eq!(top, [0, 1, 2, 3], "{chs:?} {cats:?}");
    // The channels inside the other categories kept the order they had.
    assert_eq!(inside(&chs, musica), ["Sala 3", "Sala 2"]);
    assert_eq!(inside(&chs, jogos), ["Sala 1"]);

    // A request that does not cover EVERYTHING is a 400 and changes nothing (not one row is written).
    let before = structure_of(&env, &dono.token).await;
    // The complete top level, so each of these fails for its own reason and not for a missing row.
    let all_top = json!([
        { "id": vazia, "position": 0 },
        { "id": musica, "position": 1 },
        { "id": geral, "position": 2 },
        { "id": jogos, "position": 3 },
    ]);
    let all_groups = json!([
        { "category_id": vazia, "ids": [] },
        { "category_id": musica, "ids": [sala3, sala2] },
        { "category_id": jogos, "ids": [sala1] },
    ]);
    let bad: Vec<(&str, serde_json::Value)> = vec![
        ("missing a loose channel", json!({"top": [{"id": vazia, "position": 0}, {"id": musica, "position": 1}, {"id": jogos, "position": 2}], "groups": all_groups})),
        ("missing a category", json!({"top": [{"id": geral, "position": 0}, {"id": musica, "position": 1}, {"id": jogos, "position": 2}], "groups": all_groups})),
        ("a repeated id", json!({"top": [{"id": vazia, "position": 0}, {"id": musica, "position": 1}, {"id": geral, "position": 2}, {"id": jogos, "position": 3}, {"id": geral, "position": 4}], "groups": all_groups})),
        ("a group missing a channel", json!({"top": all_top, "groups": [{"category_id": vazia, "ids": []}, {"category_id": musica, "ids": [sala3]}, {"category_id": jogos, "ids": [sala1]}]})),
        ("a group with a stranger", json!({"top": all_top, "groups": [{"category_id": vazia, "ids": []}, {"category_id": musica, "ids": [sala3, sala2]}, {"category_id": jogos, "ids": [sala1, sala3]}]})),
        ("a group that is not a category", json!({"top": all_top, "groups": [{"category_id": geral, "ids": []}]})),
    ];
    for (what, body) in bad {
        assert_eq!(post_status(&env, &dono.token, "/api/admin/structure", body).await, StatusCode::BAD_REQUEST, "{what}");
    }
    assert_eq!(structure_of(&env, &dono.token).await, before, "um pedido recusado não muda nada");
    assert!(watcher.quiet_for(Duration::from_millis(400), |e| matches!(e, ServerEvent::ChannelsChanged { .. } | ServerEvent::CategoriesChanged { .. })).await, "e não avisa ninguém");

    // The old pair keeps working exactly as before (older apps still use it).
    let ids: Vec<ChannelId> = chs.iter().map(|c| c.id).collect();
    assert_eq!(post_status(&env, &dono.token, "/api/admin/channels/reorder", json!({"ids": ids})).await, StatusCode::OK);
    watcher.wait_for("ChannelsChanged", |e| matches!(e, ServerEvent::ChannelsChanged { .. })).await;
    assert_eq!(post_status(&env, &dono.token, "/api/admin/categories/reorder", json!({"ids": [jogos, musica, vazia]})).await, StatusCode::OK);
    watcher.wait_for("CategoriesChanged", |e| matches!(e, ServerEvent::CategoriesChanged { .. })).await;
    env.server.shutdown().await;
}
