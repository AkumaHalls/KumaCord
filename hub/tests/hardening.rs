//! The hub's own defences against a peer that is not playing along: the per-name login brake
//! under simultaneous guesses, and the listener's limits (header / idle timeout, connection caps,
//! request timeout, WebSockets per person). Real hub on an ephemeral port, no LiveKit.

use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use vox_proto::*;
use voxhub_server::serve::Limits;
use voxhub_server::{start_tuned, Options, Server, Tuning};

const PASSWORD: &str = "hunter2";

async fn hub(limits: Limits) -> (Server, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"testsrv\"\nserver_password = \"{PASSWORD}\"\n\
             [livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
    let opts = Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) };
    let server = start_tuned(opts, Default::default(), Tuning { limits, ..Tuning::default() }).await.unwrap();
    let base = format!("127.0.0.1:{}", server.addr.port());
    (server, base, dir)
}

fn login_req(name: &str, password: Option<&str>) -> LoginRequest {
    LoginRequest {
        username: name.into(),
        server_password: PASSWORD.into(),
        protocol_version: PROTOCOL_VERSION,
        password: password.map(Into::into),
        ..Default::default()
    }
}

/// Is the socket closed by the server within `wait` (EOF or reset)?
async fn closed_within(s: &mut TcpStream, wait: Duration) -> bool {
    let mut buf = [0u8; 1024];
    let until = Instant::now() + wait;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(left, s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => {}
            Err(_) => return false,
        }
    }
    false
}

/// Twenty wrong guesses for a protected name sent at the same moment: only as many are checked
/// as the brake allows in a row (three), the rest are told to wait; the owner of the name still
/// gets in afterwards.
#[tokio::test]
async fn simultaneous_guesses_do_not_get_extra_checks() {
    let (_server, base, _dir) = hub(Limits::default()).await;
    let http = reqwest::Client::new();
    let url = format!("http://{base}/api/login");
    let ana: LoginResponse = http.post(&url).json(&login_req("ana", None)).send().await.unwrap().json().await.unwrap();
    let r = http.post(format!("http://{base}/api/account/password")).bearer_auth(&ana.token).json(&json!({ "new": "senha-da-ana" })).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let burst = (0..20).map(|i| {
        let (http, url) = (http.clone(), url.clone());
        async move {
            let r = http.post(&url).json(&login_req("ana", Some(&format!("chute{i}")))).send().await.unwrap();
            let status = r.status();
            (status, r.json::<ApiError>().await.map(|e| e.error).unwrap_or_default())
        }
    });
    let answers = futures_util::future::join_all(burst).await;
    let checked = answers.iter().filter(|(s, e)| *s == StatusCode::UNAUTHORIZED && e == ERR_WRONG_PERSONAL_PASSWORD).count();
    let waited = answers.iter().filter(|(s, _)| *s == StatusCode::TOO_MANY_REQUESTS).count();
    assert!(checked <= 3, "at most the free failures are checked: {answers:?}");
    assert_eq!(checked + waited, 20, "{answers:?}");

    // The real person, after the wait the hub asks for.
    let mut ok = false;
    for _ in 0..40 {
        let r = http.post(&url).json(&login_req("ana", Some("senha-da-ana"))).send().await.unwrap();
        if r.status() == StatusCode::OK {
            ok = true;
            break;
        }
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(ok, "the right password works once the brake allows");
}

/// Half a request head, idle connections, too many connections from one address, a body that
/// never comes: none of them hold the hub.
#[tokio::test]
async fn the_listener_does_not_wait_forever() {
    let limits = Limits {
        max_conns_per_ip: 3,
        header_read_timeout: Duration::from_millis(800),
        login_timeout: Duration::from_millis(800),
        ..Limits::default()
    };
    let (server, base, _dir) = hub(limits).await;

    // A request head that never ends is dropped.
    let mut slow = TcpStream::connect(&base).await.unwrap();
    slow.write_all(b"GET /api/health HTTP/1.1\r\nHost: x\r\nX-A: ").await.unwrap();
    assert!(closed_within(&mut slow, Duration::from_secs(4)).await, "partial head held open");
    for _ in 0..40 {
        if server.open_connections() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Three idle connections fill this address's share; a fourth is closed at once.
    let mut idle = Vec::new();
    for _ in 0..3 {
        idle.push(TcpStream::connect(&base).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut fourth = TcpStream::connect(&base).await.unwrap();
    assert!(closed_within(&mut fourth, Duration::from_millis(500)).await, "over the per-address cap");
    // The idle ones time out like a half-sent head (hyper's header timer runs while idle too).
    for s in &mut idle {
        assert!(closed_within(s, Duration::from_secs(4)).await, "idle connection held open");
    }
    for _ in 0..40 {
        if server.open_connections() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(server.open_connections(), 0, "slots are given back");

    // A login whose body never arrives gets a 408 instead of holding the request forever.
    let mut stuck = TcpStream::connect(&base).await.unwrap();
    stuck
        .write_all(b"POST /api/login HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 200\r\n\r\n{\"username\":")
        .await
        .unwrap();
    let mut got = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(4), stuck.read_to_end(&mut got)).await;
    assert!(String::from_utf8_lossy(&got).starts_with("HTTP/1.1 408"), "{}", String::from_utf8_lossy(&got));

    // Normal requests are fine.
    let r = reqwest::get(format!("http://{base}/api/health")).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

/// One person cannot open WebSockets without end.
#[tokio::test]
async fn websockets_per_person_are_capped() {
    let (_server, base, _dir) = hub(Limits::default()).await;
    let http = reqwest::Client::new();
    let l: LoginResponse = http.post(format!("http://{base}/api/login")).json(&login_req("ana", None)).send().await.unwrap().json().await.unwrap();
    let url = format!("ws://{base}/ws?token={}", l.token);
    let mut open = Vec::new();
    for _ in 0..voxhub_server::state::MAX_WS_PER_USER {
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        use futures_util::StreamExt;
        let _ready = ws.next().await; // registered once Ready is out
        open.push(ws);
    }
    match tokio_tungstenite::connect_async(&url).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(r)) => assert_eq!(r.status(), 429),
        other => panic!("expected 429, got {:?}", other.map(|_| ())),
    }
    drop(open);
}
