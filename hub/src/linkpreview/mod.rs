//! Link previews (roadmap item 10): the hub fetches a page's title / description / thumbnail so
//! every client can show a card under a chat message.
//!
//! One fetch serves everybody, clients never contact the linked site (no IP leak), and results
//! are cached in SQLite (24 h for a preview, 1 h for "nothing there"). Everything that touches the
//! network goes through [`fetch::Fetcher`], which enforces the SSRF rules in [`ssrf`].

pub mod fetch;
pub mod html;
pub mod ssrf;
pub mod store;
pub mod thumb;
pub mod youtube;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Url;
use tokio::sync::Semaphore;
use vox_proto::{LinkPreview, UserId};

use crate::db::Db;
use crate::util::now_ms;
use fetch::{Fetcher, Kind, Limits};
use ssrf::Policy;
use store::Row;
use thumb::Thumb;

pub const MAX_URL_LEN: usize = 2048;

#[derive(Debug, Clone)]
pub struct PreviewConfig {
    /// Addresses the fetcher may use although they are not public. **Empty in production**; the
    /// integration tests allow their loopback test server this way.
    pub allow_ips: Vec<IpAddr>,
    /// YouTube's oEmbed endpoint (a field so tests can point it at a local server).
    pub youtube_oembed: String,
    pub limits: Limits,
    pub ttl_ok: Duration,
    pub ttl_empty: Duration,
    /// Per-user token bucket for lookups that need a real fetch (cache hits are free).
    pub rate_burst: f64,
    pub rate_per_sec: f64,
    /// Fetches running at once.
    pub max_concurrent: usize,
    /// Sites that answer scrapers with a login wall; their generic title / logo are dropped and
    /// only the domain remains.
    pub wall_hosts: Vec<String>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        PreviewConfig {
            allow_ips: Vec::new(),
            youtube_oembed: "https://www.youtube.com/oembed".into(),
            limits: Limits::default(),
            ttl_ok: Duration::from_secs(24 * 3600),
            ttl_empty: Duration::from_secs(3600),
            rate_burst: 12.0,
            rate_per_sec: 0.5,
            max_concurrent: 4,
            wall_hosts: vec!["instagram.com".into()],
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RateLimited;

pub struct LinkPreviews {
    fetcher: Fetcher,
    db: Db,
    cfg: PreviewConfig,
    public_host: String,
    own_refreshed: Mutex<Option<Instant>>,
    inflight: FlightMap,
    fetch_slots: Semaphore,
    decode_slots: Arc<Semaphore>,
    buckets: Mutex<HashMap<UserId, (f64, Instant)>>,
}

/// What a fetch found, before it is stored.
#[derive(Default)]
struct Built {
    title: String,
    description: String,
    site_name: String,
    thumb: Option<Thumb>,
}

impl LinkPreviews {
    pub fn new(cfg: PreviewConfig, db: Db, public_host: &str) -> anyhow::Result<Self> {
        let policy = Arc::new(Policy::new(cfg.allow_ips.clone()));
        let fetcher = Fetcher::new(policy, cfg.limits.clone())?;
        Ok(LinkPreviews {
            fetcher,
            db,
            public_host: public_host.to_string(),
            own_refreshed: Mutex::new(None),
            inflight: Mutex::default(),
            fetch_slots: Semaphore::new(cfg.max_concurrent.max(1)),
            decode_slots: Arc::new(Semaphore::new(2)),
            buckets: Mutex::default(),
            cfg,
        })
    }

    /// The URL a client asked about, normalised (no fragment); `None` if it is not an http(s) URL.
    pub fn parse_url(raw: &str) -> Option<Url> {
        let raw = raw.trim();
        if raw.is_empty() || raw.len() > MAX_URL_LEN {
            return None;
        }
        let mut url = Url::parse(raw).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return None;
        }
        url.set_fragment(None);
        Some(url)
    }

    fn is_fresh(&self, row: &Row, now: i64) -> bool {
        let ttl = if row.is_empty() { self.cfg.ttl_empty } else { self.cfg.ttl_ok };
        now - row.fetched_at_ms < ttl.as_millis() as i64
    }

    async fn lookup(&self, key: &str) -> Option<Row> {
        let key = key.to_string();
        match self.db.call(move |c| store::get(c, &key)).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("link preview cache read failed: {e:#}");
                None
            }
        }
    }

    fn take_token(&self, user: UserId) -> bool {
        let now = Instant::now();
        let mut g = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let b = g.entry(user).or_insert((self.cfg.rate_burst, now));
        b.0 = (b.0 + now.duration_since(b.1).as_secs_f64() * self.cfg.rate_per_sec).min(self.cfg.rate_burst);
        b.1 = now;
        if b.0 >= 1.0 {
            b.0 -= 1.0;
            true
        } else {
            false
        }
    }

    /// The preview for `url`: from the cache when fresh, else fetched (once, however many people
    /// ask at the same moment).
    pub async fn get(&self, user: UserId, url: &Url) -> Result<Row, RateLimited> {
        let key = url.as_str().to_string();
        // Refused outright (private address, credentials...): nothing to fetch, store or charge.
        if self.fetcher.policy().check_url(url).is_err() {
            return Ok(Row {
                url: key,
                fetched_at_ms: now_ms(),
                title: String::new(),
                description: String::new(),
                site_name: host_label(url),
                image: None,
                image_w: 0,
                image_h: 0,
            });
        }
        let cached = self.lookup(&key).await;
        if let Some(row) = cached.as_ref().filter(|r| self.is_fresh(r, now_ms())) {
            return Ok(row.clone());
        }
        // Single flight: whoever comes second waits for the first fetch and reads its result.
        // The entry goes with the last request using it, also when a request is cancelled (the
        // client hung up) halfway: the guard's `Drop` does the cleanup.
        let flight = Flight::join(&self.inflight, key.clone());
        let _turn = flight.lock.lock().await;
        self.get_locked(user, url, &key).await
    }

    async fn get_locked(&self, user: UserId, url: &Url, key: &str) -> Result<Row, RateLimited> {
        let stale = self.lookup(key).await;
        if let Some(row) = stale.as_ref().filter(|r| self.is_fresh(r, now_ms())) {
            return Ok(row.clone());
        }
        if !self.take_token(user) {
            return Err(RateLimited);
        }
        let _slot = self.fetch_slots.acquire().await.expect("semaphore is never closed");
        self.refresh_own_addresses().await;
        let started = Instant::now();
        let built = self.build(url).await;
        let now = now_ms();
        let mut row = Row {
            url: key.to_string(),
            fetched_at_ms: now,
            title: built.title,
            description: built.description,
            site_name: built.site_name,
            image_w: built.thumb.as_ref().map_or(0, |t| t.w),
            image_h: built.thumb.as_ref().map_or(0, |t| t.h),
            image: built.thumb.map(|t| t.jpeg),
        };
        // A site that is down right now must not wipe a good older preview.
        if row.is_empty() {
            if let Some(old) = stale.filter(|o| !o.is_empty()) {
                row = Row { fetched_at_ms: now, ..old };
            }
        }
        tracing::debug!(
            host = url.host_str().unwrap_or(""),
            found = !row.is_empty(),
            image = row.image.is_some(),
            ms = started.elapsed().as_millis() as u64,
            "link preview fetched"
        );
        let to_store = row.clone();
        if let Err(e) = self.db.call(move |c| store::put(c, &to_store, now)).await {
            tracing::warn!("link preview cache write failed: {e:#}");
        }
        Ok(row)
    }

    /// The cached thumbnail for a page URL (JPEG bytes).
    pub async fn image(&self, url: &Url) -> Option<Vec<u8>> {
        self.lookup(url.as_str()).await.and_then(|r| r.image)
    }

    /// Row -> wire type.
    pub fn to_wire(row: &Row) -> LinkPreview {
        LinkPreview {
            url: row.url.clone(),
            title: row.title.clone(),
            description: row.description.clone(),
            site_name: row.site_name.clone(),
            has_image: row.image.is_some(),
            image_w: row.image_w,
            image_h: row.image_h,
        }
    }

    /// The server's own addresses must not be fetchable: its public address (a hairpin to this
    /// very hub or LiveKit; re-resolved because DDNS addresses change), and every address of the
    /// host's network interfaces with its on-link network (the host itself and its LAN, which on a
    /// native-IPv6 network have public-looking global addresses). Refreshed every minute: home
    /// IPv6 prefixes and temporary addresses change.
    async fn refresh_own_addresses(&self) {
        {
            let mut g = self.own_refreshed.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
                return;
            }
            *g = Some(Instant::now());
        }
        match tokio::task::spawn_blocking(ssrf::local_networks).await {
            Ok(nets) => self.fetcher.policy().set_local_networks(nets),
            Err(e) => tracing::warn!("listing the network interfaces failed: {e}"),
        }
        let host = self.public_host.trim().trim_matches(['[', ']']).to_string();
        let ips: Vec<IpAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) if host.is_empty() => Vec::new(),
            Err(_) => match tokio::time::timeout(Duration::from_secs(2), tokio::net::lookup_host((host.as_str(), 0))).await {
                Ok(Ok(found)) => found.map(|a| a.ip()).collect(),
                _ => Vec::new(),
            },
        };
        self.fetcher.policy().set_own_addresses(ips);
    }

    async fn build(&self, url: &Url) -> Built {
        if let Some(id) = youtube::video_id(url) {
            if let Some(b) = self.build_youtube(&id).await {
                return b;
            }
        }
        self.build_generic(url).await
    }

    /// YouTube's oEmbed answer is reliable (the watch page often is a consent wall for servers).
    async fn build_youtube(&self, id: &str) -> Option<Built> {
        let oembed = youtube::oembed_url(&self.cfg.youtube_oembed, id)?;
        let page = self.fetcher.get(&oembed, Kind::Json).await.ok()?;
        let info = youtube::parse_oembed(&page.body)?;
        let mut thumb = None;
        if let Some(t) = info.thumbnail_url.as_deref().and_then(|u| Url::parse(u).ok()) {
            // `maxresdefault` is a clean 16:9 frame but missing for some videos; the size the
            // oEmbed names always exists (and 4:3 variants are cropped to 16:9).
            for candidate in youtube::thumbnail_candidates(&t) {
                if let Some(th) = self.fetch_thumb(&candidate, true).await {
                    thumb = Some(th);
                    break;
                }
            }
        }
        Some(Built { title: info.title, description: info.author, site_name: "YouTube".into(), thumb })
    }

    async fn build_generic(&self, url: &Url) -> Built {
        let Ok(page) = self.fetcher.get(url, Kind::Html).await else {
            return Built { site_name: host_label(url), ..Built::default() };
        };
        let text = html::decode_body(&page.body, &page.content_type);
        let meta = html::parse(&text);
        let site_name = if meta.site_name.is_empty() { host_label(&page.url) } else { meta.site_name.clone() };
        // Instagram and friends serve scrapers a login wall: its generic title and logo are noise.
        if is_login_wall(&page.url, &meta.title, &self.cfg.wall_hosts) {
            return Built { site_name, ..Built::default() };
        }
        let mut thumb = None;
        if let Some(raw) = meta.image.as_deref() {
            if let Ok(img_url) = page.url.join(raw) {
                thumb = self.fetch_thumb(&img_url, false).await;
            }
        }
        Built { title: meta.title, description: meta.description, site_name, thumb }
    }

    async fn fetch_thumb(&self, url: &Url, crop_16_9: bool) -> Option<Thumb> {
        let img = self.fetcher.get(url, Kind::Image).await.ok()?;
        let permit = self.decode_slots.clone().acquire_owned().await.ok()?;
        let made = tokio::task::spawn_blocking(move || {
            let r = thumb::make(&img.body, crop_16_9);
            drop(permit);
            r
        })
        .await
        .ok()?;
        made.ok()
    }
}

type FlightMap = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// One request's share of a single-flight entry; the last one out removes the entry.
struct Flight<'a> {
    map: &'a FlightMap,
    key: String,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> Flight<'a> {
    fn join(map: &'a FlightMap, key: String) -> Self {
        let lock = map.lock().unwrap_or_else(|e| e.into_inner()).entry(key.clone()).or_default().clone();
        Flight { map, key, lock }
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        // The map's copy and ours: nobody else is waiting on this entry.
        if Arc::strong_count(&self.lock) <= 2 && map.get(&self.key).is_some_and(|l| Arc::ptr_eq(l, &self.lock)) {
            map.remove(&self.key);
        }
    }
}

/// `www.example.com` -> `example.com`
fn host_label(url: &Url) -> String {
    let h = url.host_str().unwrap_or("");
    h.strip_prefix("www.").unwrap_or(h).to_string()
}

fn is_login_wall(url: &Url, title: &str, wall_hosts: &[String]) -> bool {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let walled = wall_hosts.iter().any(|w| host == *w || host.ends_with(&format!(".{w}")));
    let t = title.to_lowercase();
    walled && (t.is_empty() || t == "instagram" || t.contains("login") || t.contains("log in"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_normalised_and_limited() {
        let u = LinkPreviews::parse_url(" https://Example.COM:443/a?b=1#frag ").unwrap();
        assert_eq!(u.as_str(), "https://example.com/a?b=1");
        assert!(LinkPreviews::parse_url("ftp://example.com").is_none());
        assert!(LinkPreviews::parse_url("javascript:alert(1)").is_none());
        assert!(LinkPreviews::parse_url("file:///etc/passwd").is_none());
        assert!(LinkPreviews::parse_url("not a url").is_none());
        assert!(LinkPreviews::parse_url("").is_none());
        assert!(LinkPreviews::parse_url(&format!("https://example.com/{}", "a".repeat(MAX_URL_LEN))).is_none());
    }

    /// Requests that are cancelled while they wait (the client hung up) leave nothing behind in
    /// the single-flight map.
    #[tokio::test]
    async fn cancelled_requests_leave_no_single_flight_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let cfg = PreviewConfig { allow_ips: vec!["127.0.0.1".parse().unwrap()], rate_burst: 1000.0, ..PreviewConfig::default() };
        let previews = Arc::new(LinkPreviews::new(cfg, db, "").unwrap());
        // A server that accepts and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hold = tokio::spawn(async move {
            let mut keep = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                keep.push(sock);
            }
        });
        let mut tasks = Vec::new();
        for i in 0..20 {
            let p = previews.clone();
            let url = LinkPreviews::parse_url(&format!("http://127.0.0.1:{port}/{}", i % 10)).unwrap();
            tasks.push(tokio::spawn(async move { p.get(1, &url).await }));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!previews.inflight.lock().unwrap().is_empty(), "the requests are waiting");
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        assert!(previews.inflight.lock().unwrap().is_empty(), "every entry left with its last request");
        hold.abort();
    }

    #[test]
    fn instagram_login_wall_is_recognised() {
        let ig = Url::parse("https://www.instagram.com/reel/abc/").unwrap();
        let w = PreviewConfig::default().wall_hosts;
        assert!(is_login_wall(&ig, "", &w));
        assert!(is_login_wall(&ig, "Instagram", &w));
        assert!(is_login_wall(&ig, "Login • Instagram", &w));
        assert!(!is_login_wall(&ig, "Fulano on Instagram: \"bom dia\"", &w));
        assert!(!is_login_wall(&Url::parse("https://example.com/").unwrap(), "Login", &w));
        assert!(!is_login_wall(&Url::parse("https://notinstagram.com/").unwrap(), "Instagram", &w));
    }
}
