//! Live check against the real internet (ignored by default; needs network):
//! `cargo test -p voxhub-server --test link_preview_live -- --ignored --nocapture`
//! Prints titles only, never page contents.

use vox_proto::*;
use voxhub_server::linkpreview::PreviewConfig;
use voxhub_server::{start_with, Options};

#[tokio::test]
#[ignore = "needs internet"]
async fn real_sites() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "server_password = \"x\"\npublic_host = \"198.51.100.7\"\n[livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n",
    )
    .unwrap();
    let server = start_with(
        Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) },
        PreviewConfig::default(),
    )
    .await
    .unwrap();
    let base = format!("http://127.0.0.1:{}", server.addr.port());
    let http = reqwest::Client::new();
    let req = LoginRequest { username: "live".into(), server_password: "x".into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
    let token = http.post(format!("{base}/api/login")).json(&req).send().await.unwrap().json::<LoginResponse>().await.unwrap().token;
    let urls = std::env::var("LIVE_URLS").unwrap_or_else(|_| {
        [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://en.wikipedia.org/wiki/Rust_(programming_language)",
            "https://www.bbc.com/news",
            "https://www.instagram.com/reel/DeRslJageXF/",
            "https://github.com/rust-lang/rust",
        ]
        .join(" ")
    });
    for u in urls.split_whitespace() {
        let full = reqwest::Url::parse_with_params(&format!("{base}/api/link-preview"), [("url", u)]).unwrap();
        let t = std::time::Instant::now();
        let p: LinkPreview = http.get(full).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
        println!(
            "LIVE {u}\n  -> site={:?} title={:?} desc_len={} image={}x{} ({} ms)",
            p.site_name,
            p.title,
            p.description.chars().count(),
            p.image_w,
            p.image_h,
            t.elapsed().as_millis()
        );
        if p.has_image {
            let full = reqwest::Url::parse_with_params(&format!("{base}/api/link-preview/image"), [("url", u)]).unwrap();
            let b = http.get(full).bearer_auth(&token).send().await.unwrap().bytes().await.unwrap();
            println!("  -> thumbnail {} bytes", b.len());
        }
    }
    server.shutdown().await;
}
