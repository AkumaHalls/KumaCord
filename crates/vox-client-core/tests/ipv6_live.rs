//! v1.0.2 against a real LiveKit: the client library a v1.0.1 app already has (hub client and
//! voice session, unchanged) talks to a hub over IPv6 only. The hub listens on `[::1]`, hands out
//! `ws://[::1]:<signal>` as the LiveKit URL (it follows the address the client used), and voice
//! connects and carries audio.
//!
//! Needs the dev LiveKit binary at `target-v02/devbin/livekit-server.exe` (or `VOX_LIVEKIT_BIN`)
//! and the dev ports (hub 7997, LiveKit 7970-7972). No audio devices: one member publishes a sine
//! tone as its microphone, the listener plays nothing.
//!
//! `cargo test -p vox-client-core --test ipv6_live -- --ignored --nocapture`

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use vox_client_core::hub::HubClient;
use vox_client_core::{VoiceConfig, VoiceSession};
use vox_proto::*;
use voxhub_server::{start, Options};

const HUB_PORT: u16 = 7997;
const LK_SIGNAL: u16 = 7970;

fn livekit_bin() -> PathBuf {
    std::env::var_os("VOX_LIVEKIT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target-v02/devbin/livekit-server.exe"))
}

async fn received_from(listener: &VoiceSession, identity: &str) -> u64 {
    let label = format!("voice:{identity}");
    listener.streams().await.iter().filter(|s| s.label == label).map(|s| s.counters.frames.load(Relaxed)).sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the dev LiveKit binary and the dev ports"]
async fn voice_works_with_a_hub_reached_over_ipv6() {
    let _ = vox_log::init("ipv6-live-test", Some(std::env::temp_dir().join("concord-ipv6-live").as_path()), true);
    let dir = tempfile::tempdir().unwrap();
    // public_host is an IPv4 nobody answers on: voice must use the address the client used.
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"v6\"\nserver_password = \"\"\nbind = \"[::1]:{HUB_PORT}\"\npublic_host = \"192.0.2.10\"\nowner = \"dono\"\n\
             [livekit]\napi_key = \"APIlivetest12\"\napi_secret = \"livetestlivetestlivetestlivetest12\"\nsignal_port = {LK_SIGNAL}\nrtc_tcp_port = 7971\nrtc_udp_port = 7972\n"
        ),
    )
    .unwrap();
    let bin = livekit_bin();
    assert!(bin.exists(), "no LiveKit binary at {}", bin.display());
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: false, livekit_bin: bin, bind_override: None }).await.unwrap();
    assert!(server.addr.is_ipv6(), "{}", server.addr);
    let t = Instant::now();
    while tokio::net::TcpStream::connect(("::1", LK_SIGNAL)).await.is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "LiveKit did not open [::1]:{LK_SIGNAL}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let base = format!("http://[::1]:{HUB_PORT}");
    let owner = HubClient::new(&base).unwrap();
    owner.login("dono", "").await.unwrap();
    owner.set_password(&SetPasswordRequest { new: Some("dono-senha".into()), ..Default::default() }).await.unwrap();
    let member = HubClient::new(&base).unwrap();
    let bia = member.login("bia", "").await.unwrap().user;
    let room_ch = owner.channels().await.unwrap().into_iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;

    let j = owner.voice_join(room_ch).await.unwrap();
    assert_eq!(j.livekit_url, format!("ws://[::1]:{LK_SIGNAL}"));
    let (listener, _lrx) = VoiceSession::connect(&j.livekit_url, &j.livekit_token, VoiceConfig { mic: false, playback: false, ..Default::default() }).await.unwrap();
    let j = member.voice_join(room_ch).await.unwrap();
    assert_eq!(j.livekit_url, format!("ws://[::1]:{LK_SIGNAL}"));
    let tone = VoiceConfig { mic: true, playback: false, mic_test_tone: Some(440.0), ..Default::default() };
    let (bia_session, _brx) = VoiceSession::connect(&j.livekit_url, &j.livekit_token, tone).await.unwrap();

    let ident = bia.id.to_string();
    let t = Instant::now();
    loop {
        let a = received_from(&listener, &ident).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let got = received_from(&listener, &ident).await.saturating_sub(a);
        println!("{got} audio chunks in 2 s");
        if got > 50 {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "no audio over the IPv6 hub ({got} chunks in 2 s)");
    }
    bia_session.leave().await;
    listener.leave().await;
    server.shutdown().await;
}
