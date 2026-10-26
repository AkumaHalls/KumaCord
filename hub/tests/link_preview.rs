//! Link previews end to end: a real hub on an ephemeral port, plus local test web servers that play
//! the part of "the internet" (the fetcher is told to treat 127.0.0.1 as allowed; the SSRF tests
//! start a hub without that exception).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Router;
use image::ImageEncoder;
use reqwest::StatusCode as Http;
use vox_proto::*;
use voxhub_server::linkpreview::fetch::Limits;
use voxhub_server::linkpreview::PreviewConfig;
use voxhub_server::{start_with, Options, Server};

const PASSWORD: &str = "hunter2";

// ---------------------------------------------------------------- test web server

#[derive(Default)]
struct Web {
    hits: Mutex<HashMap<String, usize>>,
    /// `/flaky` answers 500 until this is set.
    flaky_ok: AtomicBool,
    /// Where `/to-secret` redirects (the "internal" server).
    secret: Mutex<Option<SocketAddr>>,
}

impl Web {
    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 251) as u8, (y % 241) as u8, 90]));
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8).unwrap();
    out
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 251) as u8, 40, (y % 241) as u8]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(&img).unwrap();
    out
}

fn ok(content_type: &'static str, body: impl Into<Body>) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body.into()).into_response()
}

fn html(body: String) -> Response {
    ok("text/html; charset=utf-8", body)
}

async fn web_handler(State(st): State<Arc<Web>>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    *st.hits.lock().unwrap().entry(path.clone()).or_default() += 1;
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
    match path.as_str() {
        "/news" => html(String::from(
            r#"<!doctype html><html><head><meta charset="utf-8"><title>Fallback title</title>
<meta property="og:site_name" content="Portal Teste">
<meta property="og:title" content="Governo anuncia &quot;novo plano&quot;">
<meta property="og:description" content="Uma descrição   longa
 com quebra de linha &amp; entidades.">
<meta property="og:image" content="/img/cover.png">
</head><body>corpo</body></html>"#
        )),
        "/img/cover.png" => ok("image/png", png(1200, 630)),
        "/noimage" => html("<head><title>Só texto</title><meta name=description content='sem imagem'></head>".into()),
        "/latin1" => ok("text/html; charset=iso-8859-1", b"<head><title>Informa\xe7\xe3o</title></head>".to_vec()),
        "/flaky" => {
            if st.flaky_ok.load(Ordering::SeqCst) {
                html("<head><title>Agora funciona</title></head>".into())
            } else {
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
        "/forbidden" => StatusCode::FORBIDDEN.into_response(),
        "/pdf" => ok("application/pdf", b"%PDF-1.4 <title>nope</title>".to_vec()),
        "/redir" => Redirect::temporary("/news").into_response(),
        "/redir2" => Redirect::permanent("/redir").into_response(),
        "/loop" => Redirect::temporary("/loop").into_response(),
        "/chain1" => Redirect::temporary("/chain2").into_response(),
        "/chain2" => Redirect::temporary("/chain3").into_response(),
        "/chain3" => Redirect::temporary("/chain4").into_response(),
        "/chain4" => Redirect::temporary("/news").into_response(),
        "/to-secret" => {
            let a = st.secret.lock().unwrap().expect("secret server");
            Redirect::temporary(&format!("http://{a}/secret")).into_response()
        }
        "/to-metadata" => Redirect::temporary("http://169.254.169.254/latest/meta-data/").into_response(),
        "/to-lan" => Redirect::temporary("http://192.168.1.1/admin").into_response(),
        "/to-ten" => Redirect::temporary("http://10.0.0.5/").into_response(),
        "/to-loopback6" => Redirect::temporary("http://[::1]:7890/api/health").into_response(),
        "/to-file" => Redirect::temporary("file:///C:/Windows/win.ini").into_response(),
        "/secret" => html("<head><title>TOP SECRET</title></head>".into()),
        // More than the 1 MB cap and no </head>: the title comes first, the rest must not be read.
        "/big" => {
            let chunks = futures_util::stream::iter((0..300).map(|i| {
                let part = if i == 0 { b"<head><title>Cabe no limite</title>".to_vec() } else { vec![b'x'; 16 * 1024] };
                Ok::<_, std::io::Error>(axum::body::Bytes::from(part))
            }));
            ok("text/html", Body::from_stream(chunks))
        }
        "/slow" => {
            tokio::time::sleep(Duration::from_secs(4)).await;
            html("<head><title>Tarde demais</title></head>".into())
        }
        "/drip" => {
            let s = futures_util::stream::unfold(0u32, |n| async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let part = if n == 0 { "<head><title>Gotejando</title>".to_string() } else { "<!-- . -->".to_string() };
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(part)), n + 1))
            });
            ok("text/html", Body::from_stream(s))
        }
        "/page-hugeimg" => html(format!(r#"<head><title>Imagem enorme</title><meta property="og:image" content="http://{host}/hugeimg"></head>"#)),
        "/hugeimg" => ok("image/png", png_claiming(30000, 30000)),
        "/page-bigfile" => html(format!(r#"<head><title>Arquivo grande</title><meta property="og:image" content="http://{host}/bigfile"></head>"#)),
        "/bigfile" => ok("image/png", vec![0u8; 6 * 1024 * 1024]),
        "/page-notimage" => html(format!(r#"<head><title>Imagem falsa</title><meta property="og:image" content="http://{host}/news"></head>"#)),
        "/page-evil-image" => html(r#"<head><title>Imagem interna</title><meta property="og:image" content="http://10.0.0.7/logo.png"></head>"#.into()),
        // YouTube's oEmbed + thumbnails.
        "/oembed" => ok(
            "application/json",
            format!(
                r#"{{"title":"Vídeo de teste (HD)","author_name":"Canal Teste","thumbnail_url":"http://{host}/vi/abc123XYZ_-/hqdefault.jpg","type":"video"}}"#
            ),
        ),
        "/vi/abc123XYZ_-/hqdefault.jpg" => ok("image/jpeg", jpeg(480, 360)),
        "/vi/abc123XYZ_-/maxresdefault.jpg" => StatusCode::NOT_FOUND.into_response(),
        "/ig-wall" => html(
            r#"<head><title>Instagram</title><meta property="og:site_name" content="Instagram"><meta property="og:image" content="/static/logo.png"></head>"#.into(),
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A PNG header claiming a gigantic picture, then a stub data stream.
fn png_claiming(w: u32, h: u32) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend(data);
        out.extend(&body);
        out.extend(crc32(&body).to_be_bytes());
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 0, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &[0x78, 0x9c, 0x01, 0x00, 0x00, 0xff, 0xff]);
    chunk(&mut out, b"IEND", &[]);
    out
}

async fn spawn_web(bind: &str) -> (SocketAddr, Arc<Web>) {
    let st = Arc::new(Web::default());
    let app = Router::new().fallback(web_handler).with_state(st.clone());
    let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, st)
}

// ---------------------------------------------------------------- hub under test

struct Env {
    server: Server,
    base: String,
    http: reqwest::Client,
    token: String,
    _dir: tempfile::TempDir,
}

fn tight_limits() -> Limits {
    Limits { timeout: Duration::from_millis(1500), ..Limits::default() }
}

async fn hub(previews: PreviewConfig) -> Env {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        format!("server_password = \"{PASSWORD}\"\npublic_host = \"198.51.100.7\"\n[livekit]\napi_key = \"APItest0000\"\napi_secret = \"0123456789abcdef0123456789abcdef0123456789abcdef\"\nsignal_port = 1\n"),
    )
    .unwrap();
    let server = start_with(
        Options { data_dir: dir.path().to_path_buf(), no_livekit: true, livekit_bin: "unused".into(), bind_override: Some("127.0.0.1:0".into()) },
        previews,
    )
    .await
    .unwrap();
    let base = format!("http://127.0.0.1:{}", server.addr.port());
    let http = reqwest::Client::new();
    let mut env = Env { server, base, http, token: String::new(), _dir: dir };
    env.token = env.login("alice").await;
    env
}

fn allow_loopback() -> PreviewConfig {
    PreviewConfig { allow_ips: vec!["127.0.0.1".parse::<IpAddr>().unwrap()], limits: tight_limits(), ..PreviewConfig::default() }
}

impl Env {
    async fn login(&self, user: &str) -> String {
        let req = LoginRequest { username: user.into(), server_password: PASSWORD.into(), protocol_version: PROTOCOL_VERSION, ..Default::default() };
        let r = self.http.post(format!("{}/api/login", self.base)).json(&req).send().await.unwrap();
        assert_eq!(r.status(), Http::OK);
        r.json::<LoginResponse>().await.unwrap().token
    }

    async fn raw(&self, path: &str, url: &str, token: Option<&str>) -> reqwest::Response {
        let full = reqwest::Url::parse_with_params(&format!("{}{path}", self.base), [("url", url)]).unwrap();
        let mut rq = self.http.get(full);
        if let Some(t) = token {
            rq = rq.bearer_auth(t);
        }
        rq.send().await.unwrap()
    }

    async fn preview_as(&self, token: &str, url: &str) -> LinkPreview {
        let r = self.raw("/api/link-preview", url, Some(token)).await;
        assert_eq!(r.status(), Http::OK, "preview of {url}");
        r.json().await.unwrap()
    }

    async fn preview(&self, url: &str) -> LinkPreview {
        let t = self.token.clone();
        self.preview_as(&t, url).await
    }

    async fn image(&self, url: &str) -> reqwest::Response {
        let t = self.token.clone();
        self.raw("/api/link-preview/image", url, Some(&t)).await
    }
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn open_graph_page_gives_text_and_a_small_jpeg() {
    let (web, st) = spawn_web("127.0.0.1:0").await;
    let env = hub(allow_loopback()).await;
    let p = env.preview(&format!("http://{web}/news#ignored-fragment")).await;
    assert_eq!(p.url, format!("http://{web}/news"));
    assert_eq!(p.title, "Governo anuncia \"novo plano\"");
    assert_eq!(p.description, "Uma descrição longa com quebra de linha & entidades.");
    assert_eq!(p.site_name, "Portal Teste");
    assert!(p.has_image);
    assert!(p.image_w <= 400 && p.image_h <= 225 && p.image_w > 0, "{}x{}", p.image_w, p.image_h);
    assert!(!p.is_empty());

    let r = env.image(&format!("http://{web}/news")).await;
    assert_eq!(r.status(), Http::OK);
    assert_eq!(r.headers()["content-type"], "image/jpeg");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    let bytes = r.bytes().await.unwrap();
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).unwrap();
    assert_eq!((img.width(), img.height()), (p.image_w, p.image_h));
    assert!(bytes.len() < 60_000, "thumbnail is small: {}", bytes.len());
    assert_eq!(st.hits("/news"), 1);
    assert_eq!(st.hits("/img/cover.png"), 1);

    // No thumbnail for a page without an image; the image route never fetches anything itself.
    let n = env.preview(&format!("http://{web}/noimage")).await;
    assert_eq!((n.title.as_str(), n.description.as_str(), n.has_image), ("Só texto", "sem imagem", false));
    assert_eq!(n.site_name, "127.0.0.1");
    assert_eq!(env.image(&format!("http://{web}/noimage")).await.status(), Http::NOT_FOUND);
    assert_eq!(env.image(&format!("http://{web}/never-asked")).await.status(), Http::NOT_FOUND);
    assert_eq!(st.hits("/never-asked"), 0);
    env.server.shutdown().await;
}

#[tokio::test]
async fn latin1_pages_are_decoded() {
    let (web, _) = spawn_web("127.0.0.1:0").await;
    let env = hub(allow_loopback()).await;
    assert_eq!(env.preview(&format!("http://{web}/latin1")).await.title, "Informação");
    env.server.shutdown().await;
}

#[tokio::test]
async fn results_are_cached_and_failures_are_cached_briefly() {
    let (web, st) = spawn_web("127.0.0.1:0").await;
    let env = hub(PreviewConfig { ttl_empty: Duration::from_millis(900), ..allow_loopback() }).await;
    let bob = env.login("bob").await;

    // Hit / miss: the second and third asks (even by someone else) do not touch the site.
    let u = format!("http://{web}/news");
    let a = env.preview(&u).await;
    let b = env.preview_as(&bob, &u).await;
    let c = env.preview(&u).await;
    assert_eq!((a.clone(), b), (c, a));
    assert_eq!(st.hits("/news"), 1);

    // Negative cache: a failing site is asked once, then answered "nothing" from the cache...
    let f = format!("http://{web}/flaky");
    assert!(env.preview(&f).await.is_empty());
    assert!(env.preview(&f).await.is_empty());
    assert!(env.preview_as(&bob, &f).await.is_empty());
    assert_eq!(st.hits("/flaky"), 1);
    // ...and tried again once the (short, in this test) negative TTL is over.
    st.flaky_ok.store(true, Ordering::SeqCst);
    assert!(env.preview(&f).await.is_empty(), "still inside the negative TTL");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(env.preview(&f).await.title, "Agora funciona");
    assert_eq!(st.hits("/flaky"), 2);
    env.server.shutdown().await;
}

#[tokio::test]
async fn a_good_preview_survives_the_site_going_down() {
    let (web, st) = spawn_web("127.0.0.1:0").await;
    let env = hub(PreviewConfig { ttl_ok: Duration::from_millis(300), ..allow_loopback() }).await;
    let f = format!("http://{web}/flaky");
    st.flaky_ok.store(true, Ordering::SeqCst);
    assert_eq!(env.preview(&f).await.title, "Agora funciona");
    st.flaky_ok.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(400)).await;
    // Expired, refetch fails: the old card is kept rather than replaced by nothing.
    assert_eq!(env.preview(&f).await.title, "Agora funciona");
    assert_eq!(st.hits("/flaky"), 2);
    env.server.shutdown().await;
}

#[tokio::test]
async fn simultaneous_requests_share_one_fetch() {
    let (web, st) = spawn_web("127.0.0.1:0").await;
    let env = Arc::new(hub(allow_loopback()).await);
    let u = format!("http://{web}/news");
    let mut tasks = Vec::new();
    for i in 0..8 {
        let env = env.clone();
        let u = u.clone();
        tasks.push(tokio::spawn(async move {
            let t = env.login(&format!("user{i}")).await;
            env.preview_as(&t, &u).await
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap().title, "Governo anuncia \"novo plano\"");
    }
    assert_eq!(st.hits("/news"), 1);
    assert_eq!(st.hits("/img/cover.png"), 1);
}

#[tokio::test]
async fn auth_is_required_and_bad_urls_are_rejected() {
    let (web, st) = spawn_web("127.0.0.1:0").await;
    let env = hub(allow_loopback()).await;
    let u = format!("http://{web}/news");
    for path in ["/api/link-preview", "/api/link-preview/image"] {
        assert_eq!(env.raw(path, &u, None).await.status(), Http::UNAUTHORIZED, "{path} without a token");
        assert_eq!(env.raw(path, &u, Some("0000")).await.status(), Http::UNAUTHORIZED, "{path} with a bad token");
    }
    assert_eq!(st.hits("/news"), 0, "nothing is fetched for unauthenticated callers");
    let t = env.token.clone();
    for bad in ["", "not a url", "ftp://example.com/x", "file:///C:/Windows/win.ini", "javascript:alert(1)", &format!("https://example.com/{}", "a".repeat(3000))] {
        assert_eq!(env.raw("/api/link-preview", bad, Some(&t)).await.status(), Http::BAD_REQUEST, "{bad:.40}");
    }
    let no_param = env.http.get(format!("{}/api/link-preview", env.base)).bearer_auth(&t).send().await.unwrap();
    assert_eq!(no_param.status(), Http::BAD_REQUEST);
    env.server.shutdown().await;
}

#[tokio::test]
async fn fresh_lookups_are_rate_limited_per_user_but_cache_hits_are_free() {
    let (web, _) = spawn_web("127.0.0.1:0").await;
    let env = hub(PreviewConfig { rate_burst: 3.0, rate_per_sec: 0.01, ..allow_loopback() }).await;
    let bob = env.login("bob").await;
    for i in 0..3 {
        let r = env.raw("/api/link-preview", &format!("http://{web}/noimage?{i}"), Some(&env.token)).await;
        assert_eq!(r.status(), Http::OK);
    }
    let fourth = env.raw("/api/link-preview", &format!("http://{web}/noimage?3"), Some(&env.token)).await;
    assert_eq!(fourth.status(), Http::TOO_MANY_REQUESTS);
    // Already-cached links still answer; another user has their own allowance.
    assert_eq!(env.raw("/api/link-preview", &format!("http://{web}/noimage?1"), Some(&env.token)).await.status(), Http::OK);
    assert_eq!(env.raw("/api/link-preview", &format!("http://{web}/noimage?3"), Some(&bob)).await.status(), Http::OK);
    env.server.shutdown().await;
}

#[tokio::test]
async fn private_addresses_are_never_contacted() {
    // A hub with NO exception: even the local test server (127.0.0.1) is "internal" to it.
    let (web, st) = spawn_web("127.0.0.1:0").await;
    let env = hub(PreviewConfig { limits: Limits { timeout: Duration::from_secs(5), ..Limits::default() }, ..PreviewConfig::default() }).await;
    let port = web.port();
    let started = Instant::now();
    for url in [
        format!("http://127.0.0.1:{port}/news"),
        format!("http://localhost:{port}/news"),
        format!("http://[::1]:{port}/news"),
        format!("http://[::ffff:127.0.0.1]:{port}/news"),
        format!("http://2130706433:{port}/news"), // 127.0.0.1 written as one number
        format!("http://0x7f.0.0.1:{port}/news"),
        format!("http://127.1:{port}/news"),
        "http://10.0.0.5/".to_string(),
        "http://10.255.255.254:8080/".to_string(),
        "http://172.16.0.1/".to_string(),
        "http://192.168.1.1/".to_string(),
        "http://169.254.169.254/latest/meta-data/".to_string(),
        "http://100.64.0.1/".to_string(),
        "http://0.0.0.0/".to_string(),
        "http://[fc00::1]/".to_string(),
        "http://[fe80::1]/".to_string(),
        "http://router/".to_string(),
        "http://printer.local/".to_string(),
        "http://user:pass@example.com/".to_string(),
    ] {
        let p = env.preview(&url).await;
        assert!(p.is_empty(), "{url} must give nothing");
        assert!(env.image(&url).await.status() == Http::NOT_FOUND);
    }
    assert_eq!(st.hits("/news"), 0, "the local server was never contacted");
    // Blocked, not timed out: unreachable-but-routable targets would have eaten the 5 s deadline.
    assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
    env.server.shutdown().await;
}

#[tokio::test]
async fn redirects_are_validated_at_every_hop() {
    let (web, st) = spawn_web("127.0.0.1:0").await;
    // The "internal" service lives on another loopback address, which is NOT allowed.
    let (secret, secret_state) = spawn_web("127.0.0.2:0").await;
    *st.secret.lock().unwrap() = Some(secret);
    let env = hub(allow_loopback()).await;

    for path in ["to-secret", "to-metadata", "to-lan", "to-ten", "to-loopback6", "to-file"] {
        let p = env.preview(&format!("http://{web}/{path}")).await;
        assert!(p.is_empty(), "/{path} must not lead anywhere");
    }
    assert_eq!(secret_state.hits("/secret"), 0, "the redirect target was never contacted");

    // Allowed redirects are followed (two hops here)...
    let p = env.preview(&format!("http://{web}/redir2")).await;
    assert_eq!(p.title, "Governo anuncia \"novo plano\"");
    // ...but loops and chains longer than 3 redirects are not.
    assert!(env.preview(&format!("http://{web}/loop")).await.is_empty());
    let news_before = st.hits("/news");
    assert!(env.preview(&format!("http://{web}/chain1")).await.is_empty());
    assert_eq!(st.hits("/chain4"), 1, "the third redirect is followed...");
    assert_eq!(st.hits("/news"), news_before, "...the fourth is not");

    // An og:image pointing inside is skipped, the text still shows.
    let p = env.preview(&format!("http://{web}/page-evil-image")).await;
    assert_eq!((p.title.as_str(), p.has_image), ("Imagem interna", false));
    env.server.shutdown().await;
}

#[tokio::test]
async fn size_and_time_limits_hold() {
    let (web, st) = spawn_web("127.0.0.1:0").await;
    let env = hub(allow_loopback()).await;

    // 4.8 MB of head with no </head>: read only up to the cap, the early title is still found.
    let p = env.preview(&format!("http://{web}/big")).await;
    assert_eq!(p.title, "Cabe no limite");
    assert_eq!(st.hits("/big"), 1);

    // Slow server: the deadline (1.5 s here) cuts it off, and the answer is "nothing", fast.
    let t = Instant::now();
    assert!(env.preview(&format!("http://{web}/slow")).await.is_empty());
    assert!(t.elapsed() < Duration::from_millis(3000), "{:?}", t.elapsed());
    // A server dripping bytes forever does not hold the request open either.
    let t = Instant::now();
    assert!(env.preview(&format!("http://{web}/drip")).await.is_empty());
    assert!(t.elapsed() < Duration::from_millis(3000), "{:?}", t.elapsed());

    // Images: a claimed 30000x30000 picture (a decompression bomb), a 6 MB download, and a
    // "picture" that is really HTML are all refused; the text of the page still shows.
    for path in ["page-hugeimg", "page-bigfile", "page-notimage"] {
        let p = env.preview(&format!("http://{web}/{path}")).await;
        assert!(!p.title.is_empty() && !p.has_image, "/{path}: {p:?}");
    }
    // Not a web page at all / error statuses.
    assert!(env.preview(&format!("http://{web}/pdf")).await.is_empty());
    assert!(env.preview(&format!("http://{web}/forbidden")).await.is_empty());
    assert!(env.preview(&format!("http://{web}/missing")).await.is_empty());
    env.server.shutdown().await;
}

#[tokio::test]
async fn youtube_uses_oembed_and_falls_back_to_the_named_thumbnail() {
    let (web, st) = spawn_web("127.0.0.1:0").await;
    let env = hub(PreviewConfig { youtube_oembed: format!("http://{web}/oembed"), ..allow_loopback() }).await;
    let u = "https://www.youtube.com/watch?v=abc123XYZ_-&t=42s";
    let p = env.preview(u).await;
    assert_eq!(p.title, "Vídeo de teste (HD)");
    assert_eq!(p.description, "Canal Teste");
    assert_eq!(p.site_name, "YouTube");
    assert!(p.has_image);
    // The 4:3 hqdefault (480x360) is cut to 16:9 and fitted in 400x225.
    assert_eq!((p.image_w, p.image_h), (400, 225));
    assert_eq!(st.hits("/oembed"), 1);
    assert_eq!(st.hits("/vi/abc123XYZ_-/maxresdefault.jpg"), 1, "maxres is tried first");
    assert_eq!(st.hits("/vi/abc123XYZ_-/hqdefault.jpg"), 1);
    assert_eq!(st.hits("/watch"), 0, "the watch page itself is never scraped");
    let r = env.image(u).await;
    assert_eq!(r.status(), Http::OK);
    // youtu.be links work the same way (different URL, so a separate cache entry).
    let p2 = env.preview("https://youtu.be/abc123XYZ_-").await;
    assert_eq!(p2.title, "Vídeo de teste (HD)");
    env.server.shutdown().await;
}

#[tokio::test]
async fn login_walls_fall_back_to_the_domain_only() {
    let (web, _) = spawn_web("127.0.0.1:0").await;
    // Instagram itself is not reachable from a test, so the local server plays the walled site.
    let env = hub(PreviewConfig { wall_hosts: vec!["127.0.0.1".into()], ..allow_loopback() }).await;
    let p = env.preview(&format!("http://{web}/ig-wall")).await;
    assert!(p.is_empty(), "{p:?}");
    assert_eq!(p.site_name, "Instagram");
    assert_eq!(env.image(&format!("http://{web}/ig-wall")).await.status(), Http::NOT_FOUND);
    // The same page on a site that is not on the list is shown as it is.
    let env2 = hub(allow_loopback()).await;
    assert_eq!(env2.preview(&format!("http://{web}/ig-wall")).await.title, "Instagram");
    env.server.shutdown().await;
    env2.server.shutdown().await;
}
