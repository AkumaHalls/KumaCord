//! v1.0.2: the hub over IPv6. A client reaches the hub at `[::1]`, logs in, and gets LiveKit at
//! the same IPv6 address (`ws://[::1]:<signal port>`), in the form a v1.0.1 app connects to. Real
//! server on an ephemeral loopback port, no LiveKit.

use reqwest::StatusCode;
use vox_proto::*;
use voxhub_server::{start, Options};

const PASSWORD: &str = "hunter2";

#[tokio::test]
async fn a_client_on_ipv6_logs_in_and_gets_an_ipv6_livekit_url() {
    let dir = tempfile::tempdir().unwrap();
    // public_host is an IPv4 on purpose: the LiveKit address must follow the address the client
    // used, not the configured one.
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"v6\"\nserver_password = \"{PASSWORD}\"\npublic_host = \"192.0.2.10\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 7970\n"
        ),
    )
    .unwrap();
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("[::1]:0".into()) })
        .await
        .unwrap();
    assert!(server.addr.is_ipv6() && server.addr.ip().is_loopback(), "{}", server.addr);
    assert_eq!(server.addrs, [server.addr]);
    let base = format!("http://[::1]:{}", server.addr.port());
    let http = reqwest::Client::new();

    let health: HealthResponse = http.get(format!("{base}/api/health")).send().await.unwrap().json().await.unwrap();
    assert!(health.ok);
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));

    // The first login from the server PC over IPv6 loopback: the host's own app, so the owner.
    let req = LoginRequest { username: "anfitriao".into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
    let r = http.post(format!("{base}/api/login")).json(&req).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let me: LoginResponse = r.json().await.unwrap();
    assert_eq!(me.user.role, Role::Owner, "[::1] is the server PC too");

    let chs: Vec<Channel> = http.get(format!("{base}/api/channels")).bearer_auth(&me.token).send().await.unwrap().json().await.unwrap();
    let voice = chs.iter().find(|c| c.kind == ChannelKind::Voice).expect("a voice channel").id;
    let r = http.post(format!("{base}/api/voice/{voice}/join")).bearer_auth(&me.token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let join: VoiceJoinResponse = r.json().await.unwrap();
    assert_eq!(join.livekit_url, "ws://[::1]:7970");
    // How the app's LiveKit SDK reads it.
    let u = url::Url::parse(&join.livekit_url).unwrap();
    assert_eq!((u.host_str(), u.port()), (Some("[::1]"), Some(7970)));

    // A second person, not on the server PC, over the same IPv6 socket.
    let req = LoginRequest { username: "bia".into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
    let r = http.post(format!("{base}/api/login")).header("x-forwarded-for", "203.0.113.9").json(&req).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let bia: LoginResponse = r.json().await.unwrap();
    assert_eq!(bia.user.role, Role::Member);
    server.shutdown().await;
}
