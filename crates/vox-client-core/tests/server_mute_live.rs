//! Item 12 against a real LiveKit: server-mute must really stop a member's microphone at the
//! SFU, unmute must bring it back, and disconnect must remove them from the room.
//!
//! Needs the dev LiveKit binary at `target-v02/devbin/livekit-server.exe` (or `VOX_LIVEKIT_BIN`)
//! and the dev ports (hub 7997, LiveKit 7970-7972). No audio devices: the member publishes a sine
//! tone as its microphone, the listener plays nothing.
//!
//! `cargo test -p vox-client-core --test server_mute_live -- --ignored --nocapture`

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use vox_client_core::hub::HubClient;
use vox_client_core::{VoiceConfig, VoiceEvent, VoiceSession};
use vox_proto::*;
use voxhub_server::livekit_admin::LiveKitAdmin;
use voxhub_server::{start, Options};

const HUB_PORT: u16 = 7997;
const LK_SIGNAL: u16 = 7970;
const API_KEY: &str = "APIlivetest12";
const API_SECRET: &str = "livetestlivetestlivetestlivetest12";

fn livekit_bin() -> PathBuf {
    std::env::var_os("VOX_LIVEKIT_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target-v02/devbin/livekit-server.exe"))
}

async fn wait_port(port: u16) {
    let t = Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        assert!(t.elapsed() < Duration::from_secs(20), "LiveKit did not open port {port}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn listener_cfg() -> VoiceConfig {
    VoiceConfig { mic: false, playback: false, ..Default::default() }
}

fn tone_cfg() -> VoiceConfig {
    VoiceConfig { mic: true, playback: false, mic_test_tone: Some(440.0), ..Default::default() }
}

/// Audio chunks the listener has received from `identity` so far.
async fn received_from(listener: &VoiceSession, identity: &str) -> u64 {
    let label = format!("voice:{identity}");
    listener.streams().await.iter().filter(|s| s.label == label).map(|s| s.counters.frames.load(Relaxed)).sum()
}

/// Chunks received from `identity` during `secs`.
async fn flow(listener: &VoiceSession, identity: &str, secs: u64) -> u64 {
    let a = received_from(listener, identity).await;
    tokio::time::sleep(Duration::from_secs(secs)).await;
    received_from(listener, identity).await.saturating_sub(a)
}

async fn join(hub: &HubClient, channel: ChannelId, cfg: VoiceConfig) -> (VoiceSession, tokio::sync::mpsc::UnboundedReceiver<VoiceEvent>) {
    let j = hub.voice_join(channel).await.unwrap();
    // The hub resolves the LiveKit host from the address used to reach it: 127.0.0.1 here.
    VoiceSession::connect(&j.livekit_url, &j.livekit_token, cfg).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the dev LiveKit binary and the dev ports"]
async fn server_mute_and_disconnect_are_enforced_by_livekit() {
    let _ = vox_log::init("livekit-live-test", Some(std::env::temp_dir().join("concord-livekit-live").as_path()), true);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "server_name = \"live12\"\nserver_password = \"\"\nbind = \"127.0.0.1:{HUB_PORT}\"\npublic_host = \"127.0.0.1\"\nowner = \"dono\"\n\
             [livekit]\napi_key = \"{API_KEY}\"\napi_secret = \"{API_SECRET}\"\nsignal_port = {LK_SIGNAL}\nrtc_tcp_port = 7971\nrtc_udp_port = 7972\n"
        ),
    )
    .unwrap();
    let bin = livekit_bin();
    assert!(bin.exists(), "no LiveKit binary at {}", bin.display());
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: false, livekit_bin: bin, bind_override: None }).await.unwrap();
    wait_port(LK_SIGNAL).await;
    let base = format!("http://127.0.0.1:{HUB_PORT}");

    // The owner sets a personal password (powers are off until then).
    let owner = HubClient::new(&base).unwrap();
    owner.login("dono", "").await.unwrap();
    owner.set_password(&SetPasswordRequest { new: Some("dono-senha".into()), ..Default::default() }).await.unwrap();
    let member = HubClient::new(&base).unwrap();
    let bia = member.login("bia", "").await.unwrap().user;
    let room_ch = owner.channels().await.unwrap().into_iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    let room = format!("voice-{room_ch}");
    let ident = bia.id.to_string();

    // Both in the room; the hub learns Bia's voice state over her event socket.
    let (listener, _lrx) = join(&owner, room_ch, listener_cfg()).await;
    let (bia_session, mut bia_rx) = join(&member, room_ch, tone_cfg()).await;
    let (bia_tx, mut _bia_events) = member.connect_events().unwrap();
    bia_tx.send(ClientEvent::VoiceState { channel_id: Some(room_ch), muted: false, deafened: false, streaming: false, camera: false, audio_only: false }).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;

    let admin = LiveKitAdmin::new(&server.config, true).unwrap();
    let p = admin.participant(&room, &ident).await.unwrap();
    println!("before mute: {p:?}");
    assert!(p.can_publish_mic && p.tracks.iter().any(|t| t.source == "MICROPHONE" && !t.muted));
    let before = flow(&listener, &ident, 2).await;
    println!("before mute: {before} audio chunks in 2 s");
    assert!(before > 50, "the listener should hear the tone ({before} chunks)");

    // Server-mute: LiveKit takes the microphone away.
    owner.mod_mute(bia.id, true).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let p = admin.participant(&room, &ident).await.unwrap();
    println!("after mute: {p:?}");
    assert!(!p.can_publish_mic, "permission still allows the mic");
    assert!(!p.tracks.iter().any(|t| t.source == "MICROPHONE" && !t.muted), "an unmuted mic is still published: {p:?}");
    let during = flow(&listener, &ident, 2).await;
    println!("while server-muted: {during} audio chunks in 2 s");
    assert!(during < 5, "audio still flows while server-muted ({during} chunks)");
    // The member's own client cannot get it back by unmuting.
    bia_session.set_muted(true);
    bia_session.set_muted(false);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let during = flow(&listener, &ident, 2).await;
    assert!(during < 5, "unmuting locally brought the audio back ({during} chunks)");
    assert!(bia_session.server_muted(), "the member's session knows it is server-muted");

    // Unmute: the permission comes back and the current client republishes its mic by itself.
    owner.mod_mute(bia.id, false).await.unwrap();
    let t = Instant::now();
    loop {
        let p = admin.participant(&room, &ident).await.unwrap();
        if p.can_publish_mic && p.tracks.iter().any(|t| t.source == "MICROPHONE" && !t.muted) {
            println!("after unmute ({} ms): {p:?}", t.elapsed().as_millis());
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(15), "mic not back after unmute: {p:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = flow(&listener, &ident, 2).await;
    println!("after unmute: {after} audio chunks in 2 s");
    assert!(after > 50, "audio did not come back ({after} chunks)");
    assert!(!bia_session.server_muted());

    // A server-muted member who joins again gets a token without the mic: no mic is published,
    // and the join does not wait for one.
    owner.mod_mute(bia.id, true).await.unwrap();
    bia_session.leave().await;
    let t = Instant::now();
    let (bia_session, mut bia_rx2) = join(&member, room_ch, tone_cfg()).await;
    println!("muted rejoin took {} ms", t.elapsed().as_millis());
    assert!(t.elapsed() < Duration::from_secs(8));
    assert!(bia_session.server_muted());
    tokio::time::sleep(Duration::from_secs(2)).await;
    let p = admin.participant(&room, &ident).await.unwrap();
    assert!(!p.tracks.iter().any(|t| t.source == "MICROPHONE" && !t.muted), "{p:?}");
    assert!(flow(&listener, &ident, 2).await < 5);
    owner.mod_mute(bia.id, false).await.unwrap();
    let t = Instant::now();
    while flow(&listener, &ident, 1).await < 20 {
        assert!(t.elapsed() < Duration::from_secs(15), "mic not published after unmute on a muted join");
    }
    drop(bia_rx.try_recv());

    // Stop stream: her Go Live (a test pattern) is muted at the SFU, and she is told to stop.
    let opts = vox_client_core::ShareOptions {
        width: 640,
        height: 360,
        fps: 15,
        encoder: vox_client_core::golive::EncoderChoice::Software,
        ..Default::default()
    };
    bia_session.start_share(vox_client_core::ShareSource::TestPattern, opts).await.unwrap();
    bia_tx.send(ClientEvent::VoiceState { channel_id: Some(room_ch), muted: false, deafened: false, streaming: true, camera: false, audio_only: false }).unwrap();
    let t = Instant::now();
    loop {
        let p = admin.participant(&room, &ident).await.unwrap();
        if p.tracks.iter().any(|t| t.source == "SCREEN_SHARE" && !t.muted) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(15), "share never published: {p:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    owner.mod_stop_stream(bia.id).await.unwrap_or_else(|e| panic!("{e:#}"));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let p = admin.participant(&room, &ident).await.unwrap();
    println!("after stop-stream: {p:?}");
    assert!(p.tracks.iter().filter(|t| t.source.starts_with("SCREEN_SHARE")).all(|t| t.muted), "{p:?}");
    let mut told = false;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(500), _bia_events.recv()).await {
        if let vox_client_core::HubEvent::Server(ServerEvent::Moderated { action: ModAction::StopStream, by }) = ev {
            assert_eq!(by.username, "dono");
            told = true;
        }
    }
    assert!(told, "the streamer was not told to stop");
    bia_session.stop_share().await;

    // Disconnect: LiveKit removes her from the room; her session sees it.
    owner.mod_disconnect(bia.id).await.unwrap_or_else(|e| panic!("{e:#}"));
    let t = Instant::now();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), bia_rx2.recv()).await {
            Ok(Some(VoiceEvent::Disconnected { reason })) => {
                println!("disconnected after {} ms: {reason}", t.elapsed().as_millis());
                break;
            }
            Ok(Some(_)) => {}
            other => panic!("no Disconnected event: {other:?}"),
        }
    }
    assert!(matches!(admin.participant(&room, &ident).await, Err(voxhub_server::livekit_admin::LkError::NotFound)));

    bia_session.leave().await;
    listener.leave().await;
    server.shutdown().await;
}
