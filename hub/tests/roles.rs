//! Item 12 end-to-end: identity (personal passwords, device tokens, reset codes, brakes), the
//! owner bootstrap, the permission matrix of every moderation / admin action, channel CRUD
//! broadcasts, message deletion, the audit log, and compatibility with v0.4.1 clients.
//! Real server on an ephemeral port, no LiveKit (its calls fail fast and are best-effort; the
//! live test `livekit_live.rs` covers the SFU side).

use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use vox_proto::*;
use voxhub_server::{start, Options, Server};

const PASSWORD: &str = "hunter2";

struct Env {
    server: Server,
    base: String,
    http: reqwest::Client,
    dir: tempfile::TempDir,
}

fn write_config(dir: &std::path::Path, owner: &str) {
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "server_name = \"testsrv\"\nserver_password = \"{PASSWORD}\"\nowner = \"{owner}\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
}

async fn start_in(dir: &std::path::Path) -> (Server, String) {
    let server = start(Options { data_dir: dir.to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) })
        .await
        .unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    (server, base)
}

async fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), "Dono");
    let (server, base) = start_in(dir.path()).await;
    Env { server, base, http: reqwest::Client::new(), dir }
}

#[derive(Clone)]
struct Who {
    token: String,
    user: User,
}

impl Env {
    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.base)
    }

    async fn login_json(&self, body: Value) -> reqwest::Response {
        self.http.post(self.url("/api/login")).json(&body).send().await.unwrap()
    }

    async fn login_raw(&self, name: &str, password: Option<&str>, device: Option<&str>, remember: bool) -> reqwest::Response {
        let req = LoginRequest {
            username: name.into(),
            server_password: PASSWORD.into(),
            protocol_version: PROTOCOL_VERSION,
            password: password.map(Into::into),
            device_token: device.map(Into::into),
            remember_device: remember,
            ..Default::default()
        };
        self.http.post(self.url("/api/login")).json(&req).send().await.unwrap()
    }

    async fn login_full(&self, name: &str, password: Option<&str>) -> LoginResponse {
        let r = self.login_raw(name, password, None, false).await;
        let status = r.status();
        let text = r.text().await.unwrap();
        assert_eq!(status, StatusCode::OK, "login {name}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn who(&self, name: &str, password: Option<&str>) -> Who {
        let l = self.login_full(name, password).await;
        Who { token: l.token, user: l.user }
    }

    async fn post(&self, who: &Who, path: &str, body: Value) -> reqwest::Response {
        self.http.post(self.url(path)).bearer_auth(&who.token).json(&body).send().await.unwrap()
    }

    async fn status(&self, who: &Who, path: &str, body: Value) -> StatusCode {
        self.post(who, path, body).await.status()
    }

    async fn get(&self, who: &Who, path: &str) -> reqwest::Response {
        self.http.get(self.url(path)).bearer_auth(&who.token).send().await.unwrap()
    }

    async fn set_pw(&self, who: &Who, current: Option<&str>, new: Option<&str>, remember: bool) -> reqwest::Response {
        let body = json!({ "current": current, "new": new, "remember_device": remember });
        self.post(who, "/api/account/password", body).await
    }

    async fn set_pw_ok(&self, who: &Who, current: Option<&str>, new: &str) -> SetPasswordResponse {
        let r = self.set_pw(who, current, Some(new), false).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    async fn ws(&self, who: &Who) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", self.base, who.token)).await.unwrap();
        Ws(ws)
    }

    async fn channels(&self, who: &Who) -> Vec<Channel> {
        self.get(who, "/api/channels").await.json().await.unwrap()
    }

    async fn audit(&self, who: &Who) -> Vec<AuditEntry> {
        let r = self.get(who, "/api/admin/audit").await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }
}

async fn error_of(r: reqwest::Response) -> (StatusCode, String) {
    let s = r.status();
    (s, r.json::<ApiError>().await.map(|e| e.error).unwrap_or_default())
}

struct Ws(tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>);

impl Ws {
    /// Next server event (control frames skipped); None on close or after `wait`.
    async fn next_within(&mut self, wait: Duration) -> Option<ServerEvent> {
        loop {
            match tokio::time::timeout(wait, self.0.next()).await.ok()?? {
                Ok(WsMsg::Text(t)) => return Some(serde_json::from_str(&t).unwrap()),
                Ok(WsMsg::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    async fn ready(&mut self) -> (User, Option<Account>, Vec<VoiceMember>) {
        match self.next_within(Duration::from_secs(5)).await {
            Some(ServerEvent::Ready { user, account, voice, .. }) => (user, account, voice),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// Skip events until one matches (5 s at most).
    async fn wait_for(&mut self, what: &str, mut pred: impl FnMut(&ServerEvent) -> bool) -> ServerEvent {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            match self.next_within(Duration::from_secs(5)).await {
                Some(ev) if pred(&ev) => return ev,
                Some(_) => {}
                None => break,
            }
        }
        panic!("no {what} event");
    }

    /// Is the socket closed by the server within 3 s (events before the close are skipped)?
    async fn closed(&mut self) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) {
            match tokio::time::timeout(Duration::from_secs(3), self.0.next()).await {
                Ok(None) | Ok(Some(Ok(WsMsg::Close(_)))) | Ok(Some(Err(_))) => return true,
                Ok(Some(Ok(_))) => {}
                Err(_) => return false,
            }
        }
        false
    }

    async fn send(&mut self, ev: &ClientEvent) {
        self.0.send(WsMsg::Text(serde_json::to_string(ev).unwrap().into())).await.unwrap();
    }

    async fn voice(&mut self, channel: Option<i64>, streaming: bool) {
        self.send(&ClientEvent::VoiceState { channel_id: channel, muted: false, deafened: false, streaming, camera: false, audio_only: false }).await;
    }
}

fn voice_ids(chs: &[Channel]) -> (i64, i64) {
    let v: Vec<i64> = chs.iter().filter(|c| c.kind == ChannelKind::Voice).map(|c| c.id).collect();
    (v[0], v[1])
}

fn text_id(chs: &[Channel]) -> i64 {
    chs.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id
}

/// The owner (password set), an admin, a moderator (both with passwords, promoted) and two
/// plain members (no password).
struct Staff {
    owner: Who,
    admin: Who,
    moder: Who,
    mem: Who,
    mem2: Who,
}

async fn staff(env: &Env) -> Staff {
    let owner = env.who("Dono", None).await;
    env.set_pw_ok(&owner, None, "dono-senha").await;
    let admin = env.who("adm", None).await;
    env.set_pw_ok(&admin, None, "adm-senha").await;
    let moder = env.who("modi", None).await;
    env.set_pw_ok(&moder, None, "mod-senha").await;
    let mem = env.who("mem", None).await;
    let mem2 = env.who("mem2", None).await;
    assert_eq!(env.status(&owner, &format!("/api/admin/users/{}/role", admin.user.id), json!({"role": "admin"})).await, StatusCode::OK);
    assert_eq!(env.status(&admin, &format!("/api/admin/users/{}/role", moder.user.id), json!({"role": "moderator"})).await, StatusCode::OK);
    Staff { owner, admin, moder, mem, mem2 }
}

// ---------------------------------------------------------------- identity

#[tokio::test]
async fn personal_password_and_device_tokens() {
    let env = setup().await;
    let ana = env.who("ana", None).await;
    assert_eq!(ana.user.role, Role::Member);
    let ana_other = env.who("Ana", None).await; // another device, before the password exists
    let mut ws_other = env.ws(&ana_other).await;
    let (_, account, _) = ws_other.ready().await;
    assert_eq!(account, Some(Account { has_password: false, must_set_password: false, owner_pinned: true, ..Default::default() }));

    // Too short / too long.
    assert_eq!(env.set_pw(&ana, None, Some("12345"), false).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.set_pw(&ana, None, Some(&"x".repeat(129)), false).await.status(), StatusCode::BAD_REQUEST);
    // Set it: no current password needed the first time; this device is remembered.
    let r: SetPasswordResponse = env.set_pw(&ana, None, Some("senha-ana"), true).await.json().await.unwrap();
    assert_eq!(r.account, Account { has_password: true, must_set_password: false, owner_pinned: true, ..Default::default() });
    let device1 = r.device_token.expect("device token");
    // Every other session of the name is over (HTTP and WebSocket); this one goes on.
    assert!(ws_other.closed().await, "the other device's socket must be closed");
    assert_eq!(env.get(&ana_other, "/api/channels").await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(env.get(&ana, "/api/channels").await.status(), StatusCode::OK);

    // Now the name needs the password (any case), a device token, or nothing works.
    for name in ["ana", "ANA"] {
        assert_eq!(error_of(env.login_raw(name, None, None, false).await).await, (StatusCode::UNAUTHORIZED, ERR_PERSONAL_PASSWORD_REQUIRED.into()));
    }
    let t = Instant::now();
    assert_eq!(error_of(env.login_raw("ana", Some("senha-anx"), None, false).await).await, (StatusCode::UNAUTHORIZED, ERR_WRONG_PERSONAL_PASSWORD.into()));
    assert!(t.elapsed() >= Duration::from_millis(300), "a wrong password is answered slowly");
    let ok = env.login_full("ana", Some("senha-ana")).await;
    assert_eq!(ok.user.id, ana.user.id);
    assert!(ok.device_token.is_none(), "no device token unless asked");
    let r: LoginResponse = env.login_raw("ana", Some("senha-ana"), None, true).await.json().await.unwrap();
    let device2 = r.device_token.expect("remember_device gives a token");
    assert_eq!(device2.len(), 64);
    for d in [&device1, &device2] {
        let r = env.login_raw("ana", None, Some(d), false).await;
        assert_eq!(r.status(), StatusCode::OK, "device token login");
    }
    // A device token only works for its own name, and a bogus one asks for the password.
    let bob = env.who("bob", None).await;
    env.set_pw_ok(&bob, None, "senha-bob").await;
    assert_eq!(error_of(env.login_raw("bob", None, Some(&device1), false).await).await.1, ERR_PERSONAL_PASSWORD_REQUIRED);
    assert_eq!(error_of(env.login_raw("ana", None, Some("deadbeef"), false).await).await.1, ERR_PERSONAL_PASSWORD_REQUIRED);

    // Changing it needs the current one; it revokes every remembered device.
    assert_eq!(error_of(env.set_pw(&ana, Some("errada"), Some("nova-senha"), false).await).await, (StatusCode::UNAUTHORIZED, ERR_WRONG_PERSONAL_PASSWORD.into()));
    assert_eq!(env.set_pw(&ana, None, Some("nova-senha"), false).await.status(), StatusCode::UNAUTHORIZED);
    env.set_pw_ok(&ana, Some("senha-ana"), "nova-senha").await;
    assert_eq!(error_of(env.login_raw("ana", None, Some(&device2), false).await).await.1, ERR_PERSONAL_PASSWORD_REQUIRED);
    assert_eq!(env.login_raw("ana", Some("senha-ana"), None, false).await.status(), StatusCode::UNAUTHORIZED);
    let ana = env.who("ana", Some("nova-senha")).await;

    // A member may remove it again: the name is open, as before item 12.
    let r = env.set_pw(&ana, Some("nova-senha"), None, false).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<SetPasswordResponse>().await.unwrap().account, Account { owner_pinned: true, ..Default::default() });
    assert_eq!(env.login_raw("ana", None, None, false).await.status(), StatusCode::OK);
    env.server.shutdown().await;
}

#[tokio::test]
async fn wrong_passwords_are_slowed_down() {
    let env = setup().await;
    let ana = env.who("ana", None).await;
    env.set_pw_ok(&ana, None, "senha-ana").await;
    for _ in 0..3 {
        assert_eq!(env.login_raw("ana", Some("chute"), None, false).await.status(), StatusCode::UNAUTHORIZED);
    }
    // Now even the right password waits: the name (and this address) are being slowed down.
    let (status, msg) = error_of(env.login_raw("ana", Some("senha-ana"), None, false).await).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(msg.starts_with(ERR_TOO_MANY_ATTEMPTS), "{msg}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(env.login_raw("ana", Some("senha-ana"), None, false).await.status(), StatusCode::OK);
    // A success starts over.
    assert_eq!(env.login_raw("ana", Some("chute"), None, false).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(env.login_raw("ana", Some("senha-ana"), None, false).await.status(), StatusCode::OK);

    // The address brake also covers the server password.
    for _ in 0..3 {
        let r = env.login_json(json!({"username": "zé", "server_password": "nope", "protocol_version": PROTOCOL_VERSION})).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(env.login_raw("zé", None, None, false).await.status(), StatusCode::TOO_MANY_REQUESTS);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(env.login_raw("zé", None, None, false).await.status(), StatusCode::OK);
    env.server.shutdown().await;
}

#[tokio::test]
async fn owner_bootstrap_and_recovery() {
    let env = setup().await;
    // Someone takes the owner's name before the owner logs in after the update.
    let squatter = env.who("dono", None).await;
    let owner = env.who("Dono", None).await;
    assert_eq!(owner.user.role, Role::Owner, "the config's owner, case-insensitively");
    let mut ws = env.ws(&owner).await;
    let (me, account, _) = ws.ready().await;
    assert_eq!(me.role, Role::Owner);
    assert_eq!(account, Some(Account { has_password: false, must_set_password: true, owner_pinned: true, ..Default::default() }));
    // Until the password is set, the owner's powers are off.
    let mem = env.who("mem", None).await;
    env.set_pw_ok(&mem, None, "mem-senha").await;
    assert_eq!(env.status(&owner, "/api/admin/channels", json!({"name": "x", "kind": "text"})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.status(&owner, &format!("/api/admin/users/{}/role", mem.user.id), json!({"role": "admin"})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.get(&owner, "/api/admin/users").await.status(), StatusCode::FORBIDDEN);
    // The owner cannot go without one.
    assert_eq!(env.set_pw(&owner, None, None, false).await.status(), StatusCode::BAD_REQUEST);
    let r = env.set_pw_ok(&owner, None, "dono-senha").await;
    assert_eq!(r.account, Account { has_password: true, must_set_password: false, owner_pinned: true, ..Default::default() });
    assert_eq!(env.get(&squatter, "/api/channels").await.status(), StatusCode::UNAUTHORIZED, "the squatter is out");
    assert_eq!(env.status(&owner, "/api/admin/channels", json!({"name": "x", "kind": "text"})).await, StatusCode::OK);
    assert_eq!(error_of(env.login_raw("DONO", None, None, false).await).await.1, ERR_PERSONAL_PASSWORD_REQUIRED);

    // The host lost it: `voxhub-server --reset-password dono` prints a one-time code.
    let out = voxhub_server::admin::reset_password_cli(env.dir.path(), "dono").unwrap();
    let code = out.lines().find_map(|l| l.strip_prefix("Código: ")).unwrap().trim().to_string();
    assert!(ws.closed().await || env.get(&owner, "/api/channels").await.status() == StatusCode::UNAUTHORIZED);
    assert_eq!(env.get(&owner, "/api/channels").await.status(), StatusCode::UNAUTHORIZED, "sessions end with the reset");
    assert_eq!(error_of(env.login_raw("dono", Some("dono-senha"), None, false).await).await.1, ERR_WRONG_PERSONAL_PASSWORD);
    assert_eq!(error_of(env.login_raw("dono", None, None, false).await).await.1, ERR_PERSONAL_PASSWORD_REQUIRED);
    let owner = env.who("dono", Some(&code.to_lowercase())).await;
    let mut ws = env.ws(&owner).await;
    assert_eq!(ws.ready().await.1, Some(Account { has_password: false, must_set_password: true, owner_pinned: true, ..Default::default() }));
    assert_eq!(env.get(&owner, "/api/admin/users").await.status(), StatusCode::FORBIDDEN);
    env.set_pw_ok(&owner, None, "dono-nova").await;
    assert_eq!(env.get(&owner, "/api/admin/users").await.status(), StatusCode::OK);
    assert_eq!(env.login_raw("dono", Some(&code), None, false).await.status(), StatusCode::UNAUTHORIZED, "the code is used up");
    let log = env.audit(&owner).await;
    assert!(log.iter().any(|e| e.action == AUDIT_RESET_PASSWORD && e.actor.is_none() && e.target.as_ref().is_some_and(|t| t.id == owner.user.id)));
    assert!(voxhub_server::admin::reset_password_cli(env.dir.path(), "ninguém").is_err());
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- roles

#[tokio::test]
async fn role_assignment_rules() {
    let env = setup().await;
    let s = staff(&env).await;
    let role = |w: &Who| format!("/api/admin/users/{}/role", w.user.id);
    let mut ws = env.ws(&s.mem2).await;
    ws.ready().await;

    // Promotion needs a personal password.
    let (st, msg) = error_of(env.post(&s.owner, &role(&s.mem), json!({"role": "moderator"})).await).await;
    assert_eq!((st, msg.contains("personal password")), (StatusCode::CONFLICT, true));
    env.set_pw_ok(&s.mem, None, "mem-senha").await;
    let s_mem = env.who("mem", Some("mem-senha")).await;

    // Admin: members <-> moderators only.
    assert_eq!(env.status(&s.admin, &role(&s_mem), json!({"role": "moderator"})).await, StatusCode::OK);
    let ev = ws.wait_for("UserUpdated", |e| matches!(e, ServerEvent::UserUpdated { .. })).await;
    assert!(matches!(ev, ServerEvent::UserUpdated { user } if user.id == s_mem.user.id && user.role == Role::Moderator));
    assert_eq!(env.status(&s.admin, &role(&s_mem), json!({"role": "member"})).await, StatusCode::OK);
    assert_eq!(env.status(&s.admin, &role(&s_mem), json!({"role": "admin"})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.status(&s.admin, &role(&s.owner), json!({"role": "member"})).await, StatusCode::FORBIDDEN);
    let admin2 = env.who("adm2", None).await;
    env.set_pw_ok(&admin2, None, "adm2-senha").await;
    assert_eq!(env.status(&s.owner, &role(&admin2), json!({"role": "admin"})).await, StatusCode::OK);
    assert_eq!(env.status(&s.admin, &role(&admin2), json!({"role": "member"})).await, StatusCode::FORBIDDEN, "admins do not demote admins");
    // Moderators and members: nothing.
    assert_eq!(env.status(&s.moder, &role(&s_mem), json!({"role": "moderator"})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.status(&s.mem2, &role(&s_mem), json!({"role": "moderator"})).await, StatusCode::FORBIDDEN);
    // Owner: anything but the owner role.
    assert_eq!(env.status(&s.owner, &role(&admin2), json!({"role": "member"})).await, StatusCode::OK);
    assert_eq!(env.status(&s.owner, &role(&s_mem), json!({"role": "owner"})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.status(&s.owner, &role(&s.owner), json!({"role": "admin"})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.status(&s.owner, "/api/admin/users/9999/role", json!({"role": "member"})).await, StatusCode::NOT_FOUND);

    // A role above member cannot drop its password.
    assert_eq!(env.set_pw(&s.moder, Some("mod-senha"), None, false).await.status(), StatusCode::BAD_REQUEST);

    // The member list (admins only) and roles everywhere.
    assert_eq!(env.get(&s.moder, "/api/admin/users").await.status(), StatusCode::FORBIDDEN);
    let users: Vec<MemberInfo> = env.get(&s.admin, "/api/admin/users").await.json().await.unwrap();
    let find = |name: &str| users.iter().find(|u| u.user.username == name).unwrap().clone();
    assert_eq!((find("Dono").user.role, find("adm").user.role, find("modi").user.role), (Role::Owner, Role::Admin, Role::Moderator));
    assert!(find("adm").has_password && !find("mem2").has_password);
    // Reset passwords: the owner only.
    assert_eq!(env.status(&s.admin, &format!("/api/admin/users/{}/reset-password", s.moder.user.id), json!({})).await, StatusCode::FORBIDDEN);
    let r: ResetPasswordResponse = env.post(&s.owner, &format!("/api/admin/users/{}/reset-password", s.moder.user.id), json!({})).await.json().await.unwrap();
    assert_eq!(env.get(&s.moder, "/api/channels").await.status(), StatusCode::UNAUTHORIZED);
    let moder = env.who("modi", Some(&r.code)).await;
    assert_eq!(moder.user.role, Role::Moderator);
    // Logged in with a code: powers off until a new password is set.
    assert_eq!(env.status(&moder, &format!("/api/mod/users/{}/mute", s.mem2.user.id), json!({"muted": true})).await, StatusCode::FORBIDDEN);
    env.set_pw_ok(&moder, None, "mod-nova").await;
    assert_eq!(env.status(&moder, &format!("/api/mod/users/{}/mute", s.mem2.user.id), json!({"muted": true})).await, StatusCode::OK);

    let log = env.audit(&s.owner).await;
    let roles: Vec<&str> = log.iter().filter(|e| e.action == AUDIT_SET_ROLE).map(|e| e.detail.as_str()).collect();
    assert!(roles.contains(&"member -> moderator") && roles.contains(&"admin -> member"), "{roles:?}");
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- voice moderation

#[tokio::test]
async fn moderation_permission_matrix() {
    let env = setup().await;
    let s = staff(&env).await;
    let (sala1, sala2) = voice_ids(&env.channels(&s.owner).await);
    let all = [("owner", &s.owner), ("admin", &s.admin), ("moder", &s.moder), ("mem", &s.mem), ("mem2", &s.mem2)];
    let mut sockets = Vec::new();
    for (_, w) in &all {
        let mut ws = env.ws(w).await;
        ws.ready().await;
        ws.voice(Some(sala1), false).await;
        sockets.push(ws);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rank = |name: &str| match name {
        "owner" => Role::Owner,
        "admin" => Role::Admin,
        "moder" => Role::Moderator,
        _ => Role::Member,
    };
    // Server-mute, for every actor x target pair.
    for (an, actor) in &all {
        for (tn, target) in &all {
            let path = format!("/api/mod/users/{}/mute", target.user.id);
            let got = env.status(actor, &path, json!({"muted": true})).await;
            let want = if !rank(an).can_moderate() {
                StatusCode::FORBIDDEN
            } else if an == tn {
                StatusCode::BAD_REQUEST
            } else if rank(an).outranks(rank(tn)) {
                StatusCode::OK
            } else {
                StatusCode::FORBIDDEN
            };
            assert_eq!(got, want, "{an} mutes {tn}");
            if got == StatusCode::OK {
                assert_eq!(env.status(actor, &path, json!({"muted": false})).await, StatusCode::OK);
            }
        }
    }
    // Disconnect / move / stop-stream follow the same rule.
    for (path, body) in [("disconnect", json!({})), ("move", json!({"channel_id": sala2})), ("stop-stream", json!({}))] {
        assert_eq!(env.status(&s.mem, &format!("/api/mod/users/{}/{path}", s.mem2.user.id), body.clone()).await, StatusCode::FORBIDDEN, "member {path}");
        assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/{path}", s.admin.user.id), body.clone()).await, StatusCode::FORBIDDEN, "moderator {path} admin");
        assert_eq!(env.status(&s.admin, &format!("/api/mod/users/{}/{path}", s.owner.user.id), body.clone()).await, StatusCode::FORBIDDEN, "admin {path} owner");
        assert_eq!(env.status(&s.moder, &format!("/api/mod/users/9999/{path}"), body.clone()).await, StatusCode::NOT_FOUND);
    }
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/stop-stream", s.mem.user.id), json!({})).await, StatusCode::CONFLICT, "not streaming");
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/move", s.mem.user.id), json!({"channel_id": sala1})).await, StatusCode::BAD_REQUEST, "same channel");
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/move", s.mem.user.id), json!({"channel_id": 9999})).await, StatusCode::NOT_FOUND);
    // Old hubs' clients cannot send these; a bad body is a 4xx, not a crash.
    let r = env.http.post(env.url(&format!("/api/mod/users/{}/mute", s.mem.user.id))).bearer_auth(&s.moder.token).body("x").header("content-type", "application/json").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    env.server.shutdown().await;
}

#[tokio::test]
async fn moderation_effects_and_audit() {
    let env = setup().await;
    let s = staff(&env).await;
    let (sala1, sala2) = voice_ids(&env.channels(&s.owner).await);
    let mut w_mod = env.ws(&s.moder).await;
    w_mod.ready().await;
    let mut w_mem = env.ws(&s.mem).await;
    w_mem.ready().await;
    let mut w_mem2 = env.ws(&s.mem2).await;
    w_mem2.ready().await;
    w_mem.voice(Some(sala1), false).await;
    // (mem2's live is an audio-only one: a moderator ending it must clear the flag too.)
    w_mem2.send(&ClientEvent::VoiceState { channel_id: Some(sala1), muted: false, deafened: false, streaming: true, camera: false, audio_only: true }).await;
    w_mod.wait_for("mem2 in voice", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == s.mem2.user.id)).await;

    // Server mute: everybody sees the flag, only the target is told who did it.
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/mute", s.mem.user.id), json!({"muted": true})).await, StatusCode::OK);
    let ev = w_mod.wait_for("server_muted", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == s.mem.user.id)).await;
    assert!(matches!(ev, ServerEvent::VoiceStateChanged { member } if member.server_muted && member.channel_id == Some(sala1)));
    let ev = w_mem.wait_for("Moderated", |e| matches!(e, ServerEvent::Moderated { .. })).await;
    assert!(matches!(ev, ServerEvent::Moderated { action: ModAction::ServerMute, by } if by.id == s.moder.user.id));
    // It sticks: across the target's own voice-state updates, in snapshots, and in join tokens.
    w_mem.send(&ClientEvent::VoiceState { channel_id: Some(sala1), muted: false, deafened: true, streaming: false, camera: false, audio_only: false }).await;
    let ev = w_mod.wait_for("deafen", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == s.mem.user.id)).await;
    assert!(matches!(ev, ServerEvent::VoiceStateChanged { member } if member.server_muted && member.deafened));
    let mut fresh = env.ws(&s.admin).await;
    let (_, _, voice) = fresh.ready().await;
    assert!(voice.iter().any(|m| m.user.id == s.mem.user.id && m.server_muted));
    let grant = |token: &str| {
        let claims = token.split('.').nth(1).unwrap();
        let v: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
        v["video"].clone()
    };
    let join: VoiceJoinResponse = env.post(&s.mem, &format!("/api/voice/{sala1}/join"), json!({})).await.json().await.unwrap();
    assert_eq!(grant(&join.livekit_token)["canPublishSources"], json!(["camera", "screen_share", "screen_share_audio"]));
    // Unmute.
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/mute", s.mem.user.id), json!({"muted": false})).await, StatusCode::OK);
    let ev = w_mem.wait_for("Moderated", |e| matches!(e, ServerEvent::Moderated { .. })).await;
    assert!(matches!(ev, ServerEvent::Moderated { action: ModAction::ServerUnmute, .. }));
    let join: VoiceJoinResponse = env.post(&s.mem, &format!("/api/voice/{sala1}/join"), json!({})).await.json().await.unwrap();
    assert_eq!(grant(&join.livekit_token)["canPublishSources"], json!(["camera", "microphone", "screen_share", "screen_share_audio"]));

    // Stop stream: mem2 is told, and stops being live for everybody.
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/stop-stream", s.mem2.user.id), json!({})).await, StatusCode::OK);
    let ev = w_mem2.wait_for("Moderated", |e| matches!(e, ServerEvent::Moderated { .. })).await;
    assert!(matches!(ev, ServerEvent::Moderated { action: ModAction::StopStream, .. }));
    let ev = w_mod.wait_for("not live", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == s.mem2.user.id && !member.streaming && !member.audio_only)).await;
    assert!(matches!(ev, ServerEvent::VoiceStateChanged { member } if member.channel_id == Some(sala1)));

    // Move: a current client is told where to go and goes; it is not removed afterwards.
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/move", s.mem.user.id), json!({"channel_id": sala2})).await, StatusCode::OK);
    let ev = w_mem.wait_for("Moderated", |e| matches!(e, ServerEvent::Moderated { .. })).await;
    assert!(matches!(ev, ServerEvent::Moderated { action: ModAction::Move { channel_id }, .. } if channel_id == sala2));
    w_mem.voice(Some(sala2), false).await;
    // An older client ignores the event and stays: after the grace period it is taken out.
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/move", s.mem2.user.id), json!({"channel_id": sala2})).await, StatusCode::OK);
    let t = Instant::now();
    let ev = w_mod.wait_for("mem2 removed", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == s.mem2.user.id && member.channel_id.is_none())).await;
    assert!(matches!(ev, ServerEvent::VoiceStateChanged { .. }));
    assert!(t.elapsed() >= Duration::from_millis(1500), "removed only after the grace period");
    let mut snap = env.ws(&s.admin).await;
    let (_, _, voice) = snap.ready().await;
    assert!(voice.iter().any(|m| m.user.id == s.mem.user.id && m.channel_id == Some(sala2)), "the mover stays in the new channel");
    assert!(!voice.iter().any(|m| m.user.id == s.mem2.user.id));

    // Disconnect.
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/disconnect", s.mem.user.id), json!({})).await, StatusCode::OK);
    let ev = w_mem.wait_for("Moderated", |e| matches!(e, ServerEvent::Moderated { .. })).await;
    assert!(matches!(ev, ServerEvent::Moderated { action: ModAction::Disconnect, .. }));
    w_mod.wait_for("mem left", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == s.mem.user.id && member.channel_id.is_none())).await;
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/disconnect", s.mem.user.id), json!({})).await, StatusCode::CONFLICT, "not in voice any more");

    // The moderation log, newest first; moderators cannot read it.
    assert_eq!(env.get(&s.moder, "/api/admin/audit").await.status(), StatusCode::FORBIDDEN);
    let log = env.audit(&s.admin).await;
    let actions: Vec<&str> = log.iter().map(|e| e.action.as_str()).collect();
    let tail: Vec<&str> = actions.iter().take(7).copied().collect();
    assert_eq!(tail, [AUDIT_DISCONNECT, AUDIT_MOVE, AUDIT_MOVE, AUDIT_STOP_STREAM, AUDIT_SERVER_UNMUTE, AUDIT_SERVER_MUTE, AUDIT_SET_ROLE]);
    assert!(log[1].detail.contains("Sala 1") && log[1].detail.contains("Sala 2"), "{}", log[1].detail);
    assert!(log.iter().all(|e| e.actor.is_some()));
    assert_eq!(log[0].actor.as_ref().unwrap().role, Role::Moderator);
    assert_eq!(log[0].target.as_ref().unwrap().id, s.mem.user.id);
    assert!(log.windows(2).all(|w| w[0].id > w[1].id));
    let older: Vec<AuditEntry> = env.get(&s.admin, &format!("/api/admin/audit?before={}&limit=2", log[1].id)).await.json().await.unwrap();
    assert_eq!(older.iter().map(|e| e.id).collect::<Vec<_>>(), [log[2].id, log[3].id]);
    env.server.shutdown().await;
}

#[tokio::test]
async fn server_mute_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), "Dono");
    let (server, base) = start_in(dir.path()).await;
    let env = Env { server, base, http: reqwest::Client::new(), dir };
    let s = staff(&env).await;
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/mute", s.mem.user.id), json!({"muted": true})).await, StatusCode::OK);
    let Env { server, http, dir, .. } = env;
    server.shutdown().await;
    let (server, base) = start_in(dir.path()).await;
    let env = Env { server, base, http, dir };
    let mem = env.who("mem", None).await;
    let mut ws = env.ws(&mem).await;
    ws.ready().await;
    let (sala1, _) = voice_ids(&env.channels(&mem).await);
    ws.voice(Some(sala1), false).await;
    let ev = ws.wait_for("own state", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.user.id == mem.user.id)).await;
    assert!(matches!(ev, ServerEvent::VoiceStateChanged { member } if member.server_muted));
    let users: Vec<MemberInfo> = env.get(&env.who("Dono", Some("dono-senha")).await, "/api/admin/users").await.json().await.unwrap();
    assert!(users.iter().any(|u| u.user.id == mem.user.id && u.server_muted));
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- chat and channels

#[tokio::test]
async fn message_deletion_rules() {
    let env = setup().await;
    let s = staff(&env).await;
    let geral = text_id(&env.channels(&s.mem).await);
    let mut w_mem = env.ws(&s.mem).await;
    w_mem.ready().await;
    let mut w_mem2 = env.ws(&s.mem2).await;
    w_mem2.ready().await;
    let mut ids = Vec::new();
    for (first, text) in [(true, "da mem"), (false, "da mem2"), (false, "outra da mem2")] {
        let w = if first { &mut w_mem } else { &mut w_mem2 };
        w.send(&ClientEvent::SendMessage { channel_id: geral, content: text.into(), mentions: vec![], reply_to: None, reply_mention: false }).await;
        let ev = w.wait_for("MessageCreated", |e| matches!(e, ServerEvent::MessageCreated { message } if message.content == text)).await;
        let ServerEvent::MessageCreated { message } = ev else { unreachable!() };
        ids.push(message.id);
    }
    let del = |id: i64| format!("/api/messages/{id}/delete");
    assert_eq!(env.status(&s.mem, &del(ids[1]), json!({})).await, StatusCode::FORBIDDEN, "a member deletes only their own");
    assert_eq!(env.status(&s.mem, &del(ids[0]), json!({})).await, StatusCode::OK);
    let ev = w_mem2.wait_for("MessageDeleted", |e| matches!(e, ServerEvent::MessageDeleted { .. })).await;
    assert!(matches!(ev, ServerEvent::MessageDeleted { channel_id, message_id } if channel_id == geral && message_id == ids[0]));
    assert_eq!(env.status(&s.moder, &del(ids[1]), json!({})).await, StatusCode::OK, "moderators delete any message");
    assert_eq!(env.status(&s.moder, &del(ids[1]), json!({})).await, StatusCode::NOT_FOUND);
    assert_eq!(env.status(&s.moder, &del(999_999), json!({})).await, StatusCode::NOT_FOUND);
    let history: Vec<Message> = env.get(&s.mem, &format!("/api/channels/{geral}/messages")).await.json().await.unwrap();
    assert_eq!(history.iter().map(|m| m.id).collect::<Vec<_>>(), [ids[2]]);
    // Authors carry their role in history; members serialise without one.
    let raw: Value = env.get(&s.mem, &format!("/api/channels/{geral}/messages")).await.json().await.unwrap();
    assert!(raw[0]["author"].get("role").is_none());
    let log = env.audit(&s.owner).await;
    let dels: Vec<&AuditEntry> = log.iter().filter(|e| e.action == AUDIT_DELETE_MESSAGE).collect();
    assert_eq!(dels.len(), 1, "only someone else's message is logged");
    assert_eq!(dels[0].detail, "#geral: da mem2");

    // v0.8.2: the owner and an admin delete their own messages like anybody (no audit entry), a
    // reply to a deleted message quotes it as deleted, and the next message gets a fresh id.
    let mut w_owner = env.ws(&s.owner).await;
    w_owner.ready().await;
    let mut own = Vec::new();
    for (w, text) in [(&mut w_owner, "do dono"), (&mut w_mem, "da mem, respondendo")] {
        let reply_to = own.first().copied();
        w.send(&ClientEvent::SendMessage { channel_id: geral, content: text.into(), mentions: vec![], reply_to, reply_mention: false }).await;
        let ev = w.wait_for("MessageCreated", |e| matches!(e, ServerEvent::MessageCreated { message } if message.content == text)).await;
        let ServerEvent::MessageCreated { message } = ev else { unreachable!() };
        own.push(message.id);
    }
    let r = env.post(&s.admin, &format!("/api/channels/{geral}/messages"), json!({"content": "do admin"})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let admin_msg: Message = r.json().await.unwrap();
    assert_eq!(env.status(&s.owner, &del(own[0]), json!({})).await, StatusCode::OK, "the owner deletes their own message");
    let ev = w_mem2.wait_for("MessageDeleted", |e| matches!(e, ServerEvent::MessageDeleted { message_id, .. } if *message_id == own[0])).await;
    assert!(matches!(ev, ServerEvent::MessageDeleted { channel_id, .. } if channel_id == geral), "everybody sees it go");
    assert_eq!(env.status(&s.admin, &del(admin_msg.id), json!({})).await, StatusCode::OK, "an admin deletes their own message");
    let history: Vec<Message> = env.get(&s.mem, &format!("/api/channels/{geral}/messages")).await.json().await.unwrap();
    let reply = history.iter().find(|m| m.id == own[1]).unwrap();
    assert_eq!(reply.reply, Some(ReplyRef::gone(own[0])), "the reply quotes a deleted message");
    let r = env.post(&s.owner, &format!("/api/channels/{geral}/messages"), json!({"content": "depois"})).await;
    let after: Message = r.json().await.unwrap();
    assert!(after.id > admin_msg.id, "a deleted message's id is never handed out again");
    let log = env.audit(&s.owner).await;
    assert_eq!(log.iter().filter(|e| e.action == AUDIT_DELETE_MESSAGE).count(), 1, "own deletes are never logged");
    env.server.shutdown().await;
}

#[tokio::test]
async fn channel_management() {
    let env = setup().await;
    let s = staff(&env).await;
    let mut w = env.ws(&s.mem).await;
    w.ready().await;
    for who in [&s.moder, &s.mem] {
        assert_eq!(env.status(who, "/api/admin/channels", json!({"name": "x", "kind": "text"})).await, StatusCode::FORBIDDEN);
    }
    for bad in ["", "   ", &"x".repeat(33), "a\u{7}b"] {
        assert_eq!(env.status(&s.admin, "/api/admin/channels", json!({"name": bad, "kind": "text"})).await, StatusCode::BAD_REQUEST, "{bad:?}");
    }
    let r = env.post(&s.admin, "/api/admin/channels", json!({"name": " avisos ", "kind": "text"})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let avisos: Channel = r.json().await.unwrap();
    assert_eq!((avisos.name.as_str(), avisos.kind, avisos.position), ("avisos", ChannelKind::Text, 3));
    let list = wait_list(&mut w).await;
    assert_eq!(list.len(), 4);
    let r: Channel = env.post(&s.owner, "/api/admin/channels", json!({"name": "Sala 3", "kind": "voice"})).await.json().await.unwrap();
    let sala3 = r.id;
    wait_list(&mut w).await;

    assert_eq!(env.status(&s.moder, &format!("/api/admin/channels/{}/rename", avisos.id), json!({"name": "y"})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.status(&s.admin, &format!("/api/admin/channels/{}/rename", avisos.id), json!({"name": "anúncios"})).await, StatusCode::OK);
    let list = wait_list(&mut w).await;
    assert!(list.iter().any(|c| c.id == avisos.id && c.name == "anúncios"));
    assert_eq!(env.status(&s.admin, "/api/admin/channels/9999/rename", json!({"name": "y"})).await, StatusCode::NOT_FOUND);

    // Reorder: a permutation of every id.
    let mut ids: Vec<i64> = list.iter().map(|c| c.id).collect();
    ids.reverse();
    assert_eq!(env.status(&s.moder, "/api/admin/channels/reorder", json!({"ids": ids})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.status(&s.admin, "/api/admin/channels/reorder", json!({"ids": &ids[1..]})).await, StatusCode::BAD_REQUEST);
    assert_eq!(env.status(&s.admin, "/api/admin/channels/reorder", json!({"ids": [ids[0], ids[0], ids[1], ids[2], ids[3]]})).await, StatusCode::BAD_REQUEST);
    assert_eq!(env.status(&s.admin, "/api/admin/channels/reorder", json!({"ids": ids})).await, StatusCode::OK);
    let list = wait_list(&mut w).await;
    assert_eq!(list.iter().map(|c| c.id).collect::<Vec<_>>(), ids);
    assert_eq!(env.channels(&s.mem).await, list, "HTTP and the event agree");

    // Deleting a voice channel takes its people out of it.
    let mut w2 = env.ws(&s.mem2).await;
    w2.ready().await;
    w2.voice(Some(sala3), false).await;
    w.wait_for("mem2 in sala 3", |e| matches!(e, ServerEvent::VoiceStateChanged { member } if member.channel_id == Some(sala3))).await;
    assert_eq!(env.status(&s.moder, &format!("/api/admin/channels/{sala3}/delete"), json!({})).await, StatusCode::FORBIDDEN);
    assert_eq!(env.status(&s.admin, &format!("/api/admin/channels/{sala3}/delete"), json!({})).await, StatusCode::OK);
    let mut saw_list = false;
    let mut saw_leave = false;
    while !(saw_list && saw_leave) {
        match w.next_within(Duration::from_secs(3)).await.expect("events") {
            ServerEvent::ChannelsChanged { channels } => saw_list = !channels.iter().any(|c| c.id == sala3),
            ServerEvent::VoiceStateChanged { member } if member.user.id == s.mem2.user.id => saw_leave = member.channel_id.is_none(),
            _ => {}
        }
    }
    assert_eq!(env.post(&s.mem2, &format!("/api/voice/{sala3}/join"), json!({})).await.status(), StatusCode::NOT_FOUND);
    // Its messages go with a text channel; the last text channel stays.
    let mut w3 = env.ws(&s.mem2).await;
    w3.ready().await;
    w3.send(&ClientEvent::SendMessage { channel_id: avisos.id, content: "oi".into(), mentions: vec![], reply_to: None, reply_mention: false }).await;
    w3.wait_for("message", |e| matches!(e, ServerEvent::MessageCreated { .. })).await;
    assert_eq!(env.status(&s.admin, &format!("/api/admin/channels/{}/delete", avisos.id), json!({})).await, StatusCode::OK);
    assert_eq!(env.get(&s.mem, &format!("/api/channels/{}/messages", avisos.id)).await.status(), StatusCode::NOT_FOUND);
    let geral = text_id(&env.channels(&s.mem).await);
    let (st, msg) = error_of(env.post(&s.admin, &format!("/api/admin/channels/{geral}/delete"), json!({})).await).await;
    assert_eq!((st, msg.contains("last text channel")), (StatusCode::CONFLICT, true));
    let log = env.audit(&s.admin).await;
    let chans: Vec<(&str, &str)> = log.iter().filter(|e| e.action.starts_with("channel_")).map(|e| (e.action.as_str(), e.detail.as_str())).collect();
    assert_eq!(
        chans,
        [
            (AUDIT_CHANNEL_DELETE, "anúncios (texto)"),
            (AUDIT_CHANNEL_DELETE, "Sala 3 (voz)"),
            (AUDIT_CHANNEL_REORDER, ""),
            (AUDIT_CHANNEL_RENAME, "avisos -> anúncios"),
            (AUDIT_CHANNEL_CREATE, "Sala 3 (voz)"),
            (AUDIT_CHANNEL_CREATE, "avisos (texto)"),
        ]
    );
    env.server.shutdown().await;
}

// ---------------------------------------------------------------- compatibility

/// A v0.4.1 client speaks raw JSON with none of the item-12 fields: it logs in as before (open
/// names), sees members without roles, ignores events it does not know, and is still
/// server-muted (the hub and the token enforce it, not the client).
#[tokio::test]
async fn v041_clients_keep_working() {
    let env = setup().await;
    let s = staff(&env).await;
    let r = env.login_json(json!({"username": "velho", "server_password": PASSWORD, "protocol_version": 1})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let raw: Value = r.json().await.unwrap();
    // v1.1: `flags` is the one additive field (badges); it shows up for a badged member
    // and a v0.4.1 client simply ignores it (serde skips unknown fields). Nothing else
    // may ever appear uninvited: role/avatar/display_name stay absent when unset, and a
    // member with no badges serialises byte-identical to before (vox-proto tests that).
    let keys = raw["user"].as_object().unwrap().keys().collect::<Vec<_>>();
    assert!(keys.iter().all(|k| ["id", "username", "flags"].contains(&k.as_str())), "a member's JSON is the old one, plus at most the additive badges field");
    assert!(raw.get("device_token").is_none());
    let old = Who { token: raw["token"].as_str().unwrap().into(), user: serde_json::from_value(raw["user"].clone()).unwrap() };
    // A protected name refuses it (the old app shows "Sessão recusada").
    let r = env.login_json(json!({"username": "adm", "server_password": PASSWORD, "protocol_version": 1})).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws?token={}", env.base, old.token)).await.unwrap();
    let first = loop {
        if let Some(Ok(WsMsg::Text(t))) = ws.next().await {
            break serde_json::from_str::<Value>(&t).unwrap();
        }
    };
    assert_eq!(first["type"], "ready");
    let (sala1, _) = voice_ids(&serde_json::from_value::<Vec<Channel>>(first["channels"].clone()).unwrap());
    let raw_voice = format!(r#"{{"type":"voice_state","channel_id":{sala1},"muted":false,"deafened":false,"streaming":false}}"#);
    ws.send(WsMsg::Text(raw_voice.into())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.status(&s.moder, &format!("/api/mod/users/{}/mute", old.user.id), json!({"muted": true})).await, StatusCode::OK);
    // It gets a `moderated` event it cannot parse (it logs and skips it) and keeps working.
    let mut saw_moderated = false;
    let mut saw_flag = false;
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) && !(saw_moderated && saw_flag) {
        if let Ok(Some(Ok(WsMsg::Text(t)))) = tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            let v: Value = serde_json::from_str(&t).unwrap();
            saw_moderated |= v["type"] == "moderated";
            saw_flag |= v["type"] == "voice_state_changed" && v["member"]["user"]["id"] == old.user.id && v["member"]["server_muted"] == true;
        }
    }
    assert!(saw_moderated && saw_flag);
    ws.send(WsMsg::Text(r#"{"type":"ping","nonce":7}"#.into())).await.unwrap();
    let pong = loop {
        if let Some(Ok(WsMsg::Text(t))) = ws.next().await {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["type"] == "pong" {
                break v;
            }
        }
    };
    assert_eq!(pong["nonce"], 7);
    // Its next join token leaves the microphone out: LiveKit refuses its mic.
    let join: Value = env.post(&old, &format!("/api/voice/{sala1}/join"), json!({})).await.json().await.unwrap();
    let claims = join["livekit_token"].as_str().unwrap().split('.').nth(1).unwrap().to_string();
    let v: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap()).unwrap();
    assert!(!v["video"]["canPublishSources"].as_array().unwrap().iter().any(|s| s == "microphone"));
    env.server.shutdown().await;
}

async fn wait_list(w: &mut Ws) -> Vec<Channel> {
    match w.wait_for("ChannelsChanged", |e| matches!(e, ServerEvent::ChannelsChanged { .. })).await {
        ServerEvent::ChannelsChanged { channels } => channels,
        _ => unreachable!(),
    }
}
