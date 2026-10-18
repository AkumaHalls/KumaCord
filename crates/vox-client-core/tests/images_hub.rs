//! The app's image calls against a real hub (no LiveKit): compress a generated picture with
//! `vox_client_core::images`, upload it with its thumbnail, send it in a channel and in a DM,
//! download it back; the daily allowance comes back as a `429` the app can recognise; and an
//! older hub (the real v0.6.0, when its binary is around) answers "no images", so the app hides
//! the 📎 button.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use image::{Rgb, RgbImage};
use vox_client_core::hub::{AttachmentsReply, HttpStatusError, HubClient};
use vox_client_core::images::{compress, Quality};
use vox_proto::*;
use voxhub_server::{start, Options};

fn config(dir: &std::path::Path, bind: &str, extra: &str) {
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "server_name = \"img\"\nserver_password = \"\"\nbind = \"{bind}\"\n{extra}\n\
             [livekit]\napi_key = \"APIimgcli001\"\napi_secret = \"imgcliimgcliimgcliimgcli0123456789\"\nsignal_port = 1\n"
        ),
    )
    .unwrap();
}

fn picture(w: u32, h: u32, seed: u32) -> Vec<u8> {
    let mut s = seed | 1;
    let img = RgbImage::from_fn(w, h, |x, y| {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let n = (s >> 28) as u8;
        Rgb([(x * 255 / w) as u8 / 2 + n, (y * 255 / h) as u8 / 2 + n, 90 + n])
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 92).encode_image(&img).unwrap();
    out
}

async fn client(base: &str, name: &str) -> HubClient {
    let hub = HubClient::new(base).unwrap();
    hub.login(name, "").await.unwrap();
    hub
}

#[tokio::test]
async fn the_app_sends_and_fetches_images() {
    let dir = tempfile::tempdir().unwrap();
    config(dir.path(), "127.0.0.1:0", "attachment_daily_mb = 1");
    let server = start(Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: None }).await.unwrap();
    let base = format!("http://127.0.0.1:{}", server.addr.port());
    let (ana, bia) = (client(&base, "Ana").await, client(&base, "Bia").await);

    let AttachmentsReply::Ready(limits) = ana.attachment_limits().await else { panic!("a new hub knows images") };
    assert_eq!(limits.max_per_message, ATTACHMENT_MAX_PER_MESSAGE as u32);

    let c = compress(&picture(1600, 1000, 1), Quality::Normal, &|_| {}).unwrap();
    let a = ana.upload_attachment(&c.bytes, c.thumb.as_deref()).await.unwrap();
    assert_eq!((a.width, a.height, a.thumb_w, a.thumb_h), (c.width, c.height, c.thumb_w, c.thumb_h));
    let geral = ana.channels().await.unwrap().into_iter().find(|ch| ch.kind == ChannelKind::Text).unwrap().id;
    let m = ana.send_channel_message(geral, "olha", &[a.id]).await.unwrap();
    assert_eq!((m.caption(), m.attachments.len()), ("olha", 1));
    let file = bia.attachment_file(a.id, false).await.unwrap();
    assert_eq!(file.len() as u64, a.size);
    let thumb = bia.attachment_file(a.id, true).await.unwrap();
    assert_eq!(image::load_from_memory(&thumb).unwrap().width(), c.thumb_w);

    // A DM with an image.
    let people = ana.dm_people().await.unwrap();
    let bia_user = people.iter().find(|u| u.username == "Bia").unwrap();
    let dm = ana.open_dm(bia_user.id).await.unwrap();
    let c2 = compress(&picture(500, 300, 2), Quality::Normal, &|_| {}).unwrap();
    let a2 = ana.upload_attachment(&c2.bytes, c2.thumb.as_deref()).await.unwrap();
    let line = ana.send_dm_with(dm.id, "", &[a2.id]).await.unwrap();
    assert_eq!((line.caption(), line.attachments.len()), ("", 1));
    assert!(bia.attachment_file(a2.id, false).await.is_ok());

    // The daily allowance (1 MB here): a clear 429 the app turns into a sentence.
    let big = compress(&picture(1900, 1900, 3), Quality::Original, &|_| {}).unwrap();
    let mut refused = None;
    for _ in 0..4 {
        if let Err(e) = ana.upload_attachment(&big.bytes, big.thumb.as_deref()).await {
            refused = Some(e);
            break;
        }
    }
    let e = refused.expect("the allowance ran out");
    let status = e.downcast_ref::<HttpStatusError>().unwrap();
    assert_eq!((status.status, status.message.as_str()), (429, ERR_ATTACHMENT_DAILY));
    server.shutdown().await;
}

fn old_hub() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::var_os("VOX_V060_HUB")
        .map(PathBuf::from)
        .into_iter()
        .chain([root.join("releases/v0.6.0/voxhub-server.exe"), root.join("target-v02/v060-hub/voxhub-server.exe")])
        .find(|p| p.exists())
}

#[tokio::test]
async fn an_older_hub_means_no_images() {
    let Some(bin) = old_hub() else {
        eprintln!("skipped: no v0.6.0 hub binary (set VOX_V060_HUB)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    config(dir.path(), &format!("127.0.0.1:{port}"), "");
    let mut child = Command::new(bin)
        .arg("--data-dir")
        .arg(dir.path())
        .arg("--no-livekit")
        .arg("--log-dir")
        .arg(dir.path().join("logs"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_err() {
        assert!(t.elapsed() < Duration::from_secs(15), "the old hub did not start");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let hub = client(&format!("http://127.0.0.1:{port}"), "Ana").await;
    let reply = hub.attachment_limits().await;
    // Only this exact child is stopped.
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(reply, AttachmentsReply::Unsupported);
}
