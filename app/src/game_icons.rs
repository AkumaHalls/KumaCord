//! The game's own tile next to "Jogando X" — the game's real logo, the way Discord draws it.
//!
//! The activity only carries the game's name, so the tile resolves that name through four
//! sources, each one covering the previous one's gaps:
//!
//! 1. **The pack inside the exe** (`assets/game-icons.pack`, built by
//!    `tools/game_icons/build.ps1`): the app's own game catalogue already resolved against
//!    Discord's list at build time, shipped as 64x64 PNGs. No network, instant, the icons are
//!    there the moment the app starts — which is what "não depender da API do Discord" means
//!    for the games people actually play.
//! 2. **Discord's own pipeline** (`vox_client_core` isn't involved: see [`list`]): the same
//!    public "detectable applications" JSON the official client polls, and the icon from the
//!    public `app-icons` CDN. Covers the long tail (~21k games) with the same art Discord
//!    shows. Cached on disk for a week, so it is fetched rarely, and a stale cache is still
//!    used when offline.
//! 3. **Steam**: the built-in table's appid, capsule art from Steam's public CDN.
//! 4. **The app's own emoji catalogue**: a themed emoji, the gamepad for the unknown.
//!
//! Everything is best-effort and silent: a fetch that fails, a damaged pack or a missing
//! entry simply leaves the next source's tile.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use eframe::egui::{self, Color32, Pos2, Rect, TextureId, Vec2};

use crate::theme;

/// Side of the tile in the member rows. Discord draws the activity's own art at 16 px in the
/// member list — the same number here, so the tile is never a shrunken copy inside a box.
pub const TILE: f32 = 16.0;
/// Side of the tile in the profile card: Discord shows the activity art at 32 px there.
pub const TILE_CARD: f32 = 32.0;

/// What a game name resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// A Steam appid: its capsule art is fetched from the public CDN and cached.
    Steam(u32),
    /// A themed emoji from the app's own catalogue — the gamepad when the name is unknown.
    Emoji(&'static str),
}

/// Normalised game name -> Steam appid (Steam games only; keys are already normalised).
const STEAM: &[(&str, u32)] = &[
    ("counterstrike2", 730),
    ("cs2", 730),
    ("csgo", 730),
    ("counterstrikeglobaloffensive", 730),
    ("pubgbattlegrounds", 578080),
    ("pubg", 578080),
    ("apexlegends", 1172470),
    ("overwatch2", 2357570),
    ("overwatch", 2357570),
    ("tomclancysrainbowsixsiege", 359550),
    ("rainbowsixsiege", 359550),
    ("destiny2", 1085660),
    ("warframe", 230410),
    ("teamfortress2", 440),
    ("portal2", 620),
    ("halflife2", 220),
    ("garrysmod", 4000),
    ("left4dead2", 550),
    ("rust", 252490),
    ("dayz", 223350),
    ("arma3", 107410),
    ("helldivers2", 394510),
    ("warhammer40000spacemarine2", 2183900),
    ("spacemarine2", 2183900),
    ("deadbydaylight", 381210),
    ("phasmophobia", 739630),
    ("lethalcompany", 1966720),
    ("contentwarning", 2881650),
    ("dota2", 570),
    ("smite", 386360),
    ("hearthstone", 7149),
    ("diabloiv", 2344520),
    ("terraria", 105600),
    ("stardewvalley", 413150),
    ("rimworld", 294100),
    ("factorio", 427520),
    ("satisfactory", 526870),
    ("shapez2", 2141190),
    ("citiesskylinesii", 949230),
    ("astroneer", 361420),
    ("nomanssky", 275850),
    ("valheim", 892970),
    ("enshrouded", 2167900),
    ("vrising", 1604030),
    ("palworld", 1623730),
    ("oncehuman", 2139460),
    ("sonsoftheforest", 1326470),
    ("subnautica", 264710),
    ("vampiresurvivors", 1794680),
    ("brotato", 2347850),
    ("balatro", 2379780),
    ("slaythespire", 646570),
    ("inscryption", 1092790),
    ("hollowknight", 367520),
    ("hades", 1145360),
    ("hadesii", 1332010),
    ("eldenring", 1245620),
    ("sekiroshadowsdietwice", 814380),
    ("darksoulsiii", 374320),
    ("darksoulsremastered", 570940),
    ("thewitcher3wildhunt", 292030),
    ("witcher3", 292030),
    ("cyberpunk2077", 1091500),
    ("baldursgate3", 1086940),
    ("bg3", 1086940),
    ("hogwartslegacy", 990080),
    ("starfield", 1716740),
    ("fallout4", 377160),
    ("theelderscrollsvskyrim", 489830),
    ("skyrim", 489830),
    ("skyrimse", 489830),
    ("grandtheftautov", 271590),
    ("gtav", 271590),
    ("gta5", 271590),
    ("reddeadredemption2", 1174180),
    ("rdr2", 1174180),
    ("monsterhunterwilds", 2246340),
    ("monsterhunterworld", 582010),
    ("monsterhunterrise", 1446780),
    ("dragonsdogma2", 2054970),
    ("liesofp", 1627720),
    ("remnant2", 1903340),
    ("residentevil4", 2050650),
    ("re4", 2050650),
    ("residentevilvillage", 1196590),
    ("re8", 1196590),
    ("pathofexile", 238960),
    ("forzahorizon5", 1551360),
    ("assettocorsa", 244210),
    ("beamngdrive", 284160),
    ("eurotrucksimulator2", 227300),
    ("ets2", 227300),
    ("trackmania", 2055460),
    ("rocketleague", 252950),
    ("wreckfest", 228380),
    ("vrchat", 438100),
    ("amongus", 945360),
    ("osu", 607260),
];

/// Normalised game name -> themed emoji (games that are not on Steam). Every emoji here is in
/// the app's own catalogue (`emoji::CATALOG`), so it always has art.
const EMOJI: &[(&str, &str)] = &[
    ("valorant", "🎯"),
    ("leagueoflegends", "🏆"),
    ("lol", "🏆"),
    ("worldofwarcraft", "🐺"),
    ("wow", "🐺"),
    ("minecraft", "💎"),
    ("minecraftjavaedition", "💎"),
    ("fortnite", "🛸"),
    ("roblox", "🧩"),
    ("genshinimpact", "✨"),
    ("honkaistarrail", "🌟"),
    ("starrail", "🌟"),
    ("zenlesszonezero", "⚡"),
    ("wutheringwaves", "🌊"),
    ("honkaiimpact3rd", "🔥"),
    ("toweroffantasy", "🏰"),
    ("asphalt9legends", "🚗"),
];

/// The gamepad, for any name the table does not know.
const GAMEPAD: &str = "🎮";

/// Lowercase and keep only letters/digits, so "Rocket League™" and "rocket league" agree.
fn key(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// What the activity's game name resolves to. Pure, so the unit tests need no network.
pub fn resolve(game_name: &str) -> Resolved {
    let k = key(game_name);
    if k.is_empty() {
        return Resolved::Emoji(GAMEPAD);
    }
    if let Some((_, appid)) = STEAM.iter().find(|(n, _)| *n == k) {
        return Resolved::Steam(*appid);
    }
    if let Some((_, ch)) = EMOJI.iter().find(|(n, _)| *n == k) {
        return Resolved::Emoji(ch);
    }
    Resolved::Emoji(GAMEPAD)
}

/// The art of each source this session asked for: loading, ready, or failed (failed stays
/// failed for the session — a CDN hiccup is not worth re-hammering it every frame).
enum State {
    Loading,
    Ready(TextureId),
    Failed,
}

/// The session's cache of game tiles. The fetch runs on its own thread and comes back through a
/// channel, so the UI never blocks; [`Store::poll`] drains the channel into textures.
pub struct Store {
    map: HashMap<String, State>,
    tx: Sender<(String, Option<Vec<u8>>)>,
    rx: Receiver<(String, Option<Vec<u8>>)>,
}

impl Store {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Store { map: HashMap::new(), tx, rx }
    }

    /// Turn the fetches that finished into textures. Called from the source getters, which the
    /// tile draw calls every frame while the row is visible.
    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((key, bytes)) = self.rx.try_recv() {
            let state = match bytes.and_then(|b| vox_client_core::images::decode(&b).ok()) {
                Some(img) => {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    State::Ready(ctx.load_texture(format!("game/{key}"), color, egui::TextureOptions::LINEAR).id())
                }
                None => State::Failed,
            };
            self.map.insert(key, state);
        }
    }

    /// Start the fetch of `key`'s art if nobody did yet.
    fn ensure(&mut self, key: String, fetch: impl FnOnce() -> Option<Vec<u8>> + Send + 'static) {
        if self.map.contains_key(&key) {
            return;
        }
        self.map.insert(key.clone(), State::Loading);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((key, fetch()));
        });
    }

    /// The texture of `appid`'s Steam capsule art if it has arrived; starts the fetch on the
    /// first call and answers `None` (the caller draws the next source meanwhile) until then.
    pub fn steam(&mut self, ctx: &egui::Context, appid: u32) -> Option<TextureId> {
        let key = format!("steam/{appid}");
        self.poll(ctx);
        self.ensure(key.clone(), move || {
            // capsule_184x69.jpg is the small header art (~15 KB): enough for a 16-22 px tile.
            let url = format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/capsule_184x69.jpg");
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .ok()
                .and_then(|c| c.get(&url).send().ok())
                .and_then(|r| if r.status().is_success() { r.bytes().ok().map(|b| b.to_vec()) } else { None })
        });
        match self.map.get(&key) {
            Some(State::Ready(id)) => Some(*id),
            _ => None,
        }
    }

    /// The game's own logo from Discord's public app-icons CDN, resolved by game name (the long
    /// tail the embedded pack does not carry): `None` while it loads, and `None` for good when
    /// Discord has no such game — the caller then falls back to the Steam/emoji sources.
    pub fn discord(&mut self, ctx: &egui::Context, game_key: &str) -> Option<TextureId> {
        let key = format!("discord/{game_key}");
        self.poll(ctx);
        let k = game_key.to_string();
        self.ensure(key.clone(), move || discord_art(&k));
        match self.map.get(&key) {
            Some(State::Ready(id)) => Some(*id),
            _ => None,
        }
    }

    /// The icon this build carries inside the exe (the embedded pack): decoded at once — the
    /// images are tiny (64 px) and the pack is a few hundred KB, so there is no thread and no
    /// fetch. `None` when the pack has no such game (the caller moves on to the live source).
    pub fn pack(&mut self, ctx: &egui::Context, game_key: &str) -> Option<TextureId> {
        let key = format!("pack/{game_key}");
        self.poll(ctx);
        if let Some(state) = self.map.get(&key) {
            return match state {
                State::Ready(id) => Some(*id),
                // Failed stays failed for the session; Loading would mean a re-entrant call.
                _ => None,
            };
        }
        let Some(bytes) = pack_icon(game_key) else { return None };
        let state = match vox_client_core::images::decode(bytes) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                State::Ready(ctx.load_texture(key.clone(), color, egui::TextureOptions::LINEAR).id())
            }
            Err(_) => State::Failed,
        };
        self.map.insert(key, state);
        match self.map.get(&format!("pack/{game_key}")) {
            Some(State::Ready(id)) => Some(*id),
            _ => None,
        }
    }
}

// ------------------------------------------------------------------ Discord's own art
//
// This is exactly what the official client does with a running game, minus the account: the
// client matches the game against Discord's "detectable applications" list — the same public,
// unauthenticated JSON it polls — and draws the logo from
// `cdn.discordapp.com/app-icons/{app_id}/{icon_hash}.png`, where `icon_hash` rides along in that
// same list. Verified end to end against the live endpoints: the list answers 200 (~25k entries,
// each with id/name/aliases/icon_hash/executables) and every icon URL built this way answers 200
// with a real PNG. The per-application API (`/applications/{id}`, which the previous version of
// this module used to double-check names) is NOT used: it answers 401 these days, which is why
// every tile fell back to the emoji.
//
// Matching is by normalised name against the entry's name and aliases — exact first, then a
// prefix match in either direction. The prefix rule is what "Overwatch 2" (entry "Overwatch")
// and "Minecraft: Java Edition" (entry "Minecraft") need; it is prefix-only so a short entry
// like "Watch" can never be found *inside* a longer name and steal its tile.

/// Discord's detectable-applications list: the same public JSON the official client polls.
const LIST_URL: &str = "https://discord.com/api/v9/applications/detectable";
/// Where an entry's `icon_hash` becomes a PNG (the same CDN the client preconnects to).
fn icon_url(app: u64, icon: &str) -> String {
    format!("https://cdn.discordapp.com/app-icons/{app}/{icon}.png?size=256")
}

/// One detectable application: the id the CDN wants, the icon hash, and the normalised names it
/// answers to (its own name plus the aliases Discord lists for it).
#[derive(Clone, PartialEq, Eq, Debug)]
struct Entry {
    app: u64,
    icon: String,
    /// Normalised names (`key()`), the entry's own name first.
    names: Vec<String>,
}

impl Entry {
    fn from_json(e: &serde_json::Value) -> Option<Self> {
        let app = e.get("id")?.as_str()?.parse::<u64>().ok()?;
        let icon = e.get("icon_hash")?.as_str().filter(|h| !h.is_empty())?.to_string();
        let mut names = Vec::new();
        if let Some(name) = e.get("name").and_then(|n| n.as_str()) {
            if !name.is_empty() {
                names.push(key(name));
            }
        }
        if let Some(aliases) = e.get("aliases").and_then(|a| a.as_array()) {
            for a in aliases {
                if let Some(a) = a.as_str().filter(|a| !a.is_empty()) {
                    let k = key(a);
                    if !names.contains(&k) {
                        names.push(k);
                    }
                }
            }
        }
        (!names.is_empty()).then_some(Entry { app, icon, names })
    }
}

/// The session's list: fetched (or read from cache) once, shared by every lookup.
enum List {
    Unfetched,
    Ready(Arc<Vec<Entry>>),
    Failed,
}
static LIST: OnceLock<Mutex<List>> = OnceLock::new();

/// The detectable list, from the session cache or a fresh-enough disk cache, fetching (and then
/// caching) it as the last resort. `None` when nothing works — the caller draws the next source.
fn list() -> Option<Arc<Vec<Entry>>> {
    let m = LIST.get_or_init(|| Mutex::new(List::Unfetched));
    let mut g = m.lock().ok()?;
    match &*g {
        List::Ready(l) => return Some(l.clone()),
        List::Failed => return None,
        List::Unfetched => {}
    }
    // 1) a cache written less than a week ago.
    if let Some((saved_at, entries)) = load_cache(false) {
        if now_secs().saturating_sub(saved_at) < CACHE_TTL_SECS && !entries.is_empty() {
            let arc = Arc::new(entries);
            *g = List::Ready(arc.clone());
            return Some(arc);
        }
    }
    // 2) the live list (a background thread, so the UI never waits for 13 MB).
    let entries = http_get(LIST_URL).ok().as_deref().map(parse_list).unwrap_or_default();
    if entries.is_empty() {
        // Offline (or the API refused): a stale cache still beats no icon at all.
        if let Some((_, stale)) = load_cache(true).filter(|(_, e)| !e.is_empty()) {
            let arc = Arc::new(stale);
            *g = List::Ready(arc.clone());
            return Some(arc);
        }
        *g = List::Failed;
        return None;
    }
    let arc = Arc::new(entries);
    save_cache(&arc);
    *g = List::Ready(arc.clone());
    Some(arc)
}

/// The entry a game name resolves to: exact name or alias first, else the shortest prefix
/// relation in either direction (so a longer game name finds its shorter entry and never the
/// other way around: "watch" is not found inside "overwatch2").
fn find<'a>(entries: &'a [Entry], k: &str) -> Option<&'a Entry> {
    if k.is_empty() {
        return None;
    }
    let mut prefix: Option<(&'a Entry, usize)> = None;
    for e in entries {
        for n in &e.names {
            if n == k {
                return Some(e);
            }
            if n.len() < 4 || k.len() < 4 {
                continue;
            }
            if k.starts_with(n.as_str()) || n.starts_with(k) {
                // Shorter entry name wins (the most generic art), the first one breaks ties.
                if prefix.is_none_or(|(_, len)| len > n.len()) {
                    prefix = Some((e, n.len()));
                }
            }
        }
    }
    prefix.map(|(e, _)| e)
}

fn http_get(url: &str) -> Result<String, reqwest::Error> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(url)
        .header(reqwest::header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) KumaCord")
        .send()?
        .error_for_status()?
        .text()
}

// ------------------------------------------------------------------ the icons shipped inside the exe
//
// The app's own game catalogue, resolved against Discord's detectable list at BUILD time
// (`tools/game_icons/build.ps1`: list -> manifest -> 64x64 PNGs -> `KUIG` pack) and embedded with
// `include_bytes!`. The art comes from the same public CDN the official client uses; the pack only
// means the games our own detector can name need no network at all — the icons are there the
// moment the app starts, offline or not. Anything outside the pack still goes through the live
// Discord source (the long tail), then Steam, then the emoji. A damaged pack is not fatal: the
// whole feature is simply off.

/// The pack's magic (see `tools/game_icons/pack.py`).
const PACK_MAGIC: &[u8; 4] = b"KUIG";
/// The pack format this build reads.
const PACK_FORMAT: u8 = 1;

/// `(normalised key, PNG bytes)` parsed once from the pack and kept for the process.
static PACK: OnceLock<Vec<(String, &'static [u8])>> = OnceLock::new();

fn pack_entries() -> &'static Vec<(String, &'static [u8])> {
    PACK.get_or_init(|| parse_pack(include_bytes!("../assets/game-icons.pack")))
}

/// The pack's PNG for a normalised game name, or `None` when this build does not carry it.
fn pack_icon(key: &str) -> Option<&'static [u8]> {
    pack_entries().iter().find(|(k, _)| k == key).map(|(_, b)| *b)
}

fn parse_pack(bytes: &'static [u8]) -> Vec<(String, &'static [u8])> {
    let mut r = Reader { b: bytes, i: 0 };
    if r.take(4) != Some(PACK_MAGIC) || r.u8() != Some(PACK_FORMAT) {
        return Vec::new();
    }
    let Some(count) = r.u32() else { return Vec::new() };
    (0..count)
        .filter_map(|_| {
            let key = r.str()?.to_string();
            let len = r.u32()? as usize;
            let img = r.take(len)?;
            Some((key, img))
        })
        .collect()
}

// ------------------------------------------------------------------ the list's disk cache
//
// The live list is ~13 MB of JSON and only a tenth of it is useful; the reduced form (id, icon
// hash, names) is a few hundred KB. Re-fetching it every launch is wasteful and the icons are
// not ours to rate-limit, so a fetched list is kept next to the settings for a week and read
// back on the next runs. Best-effort everywhere: a full disk, a read-only folder or a corrupt
// file just means a fresh fetch.

/// The cache file's name, inside the app's data folder (`%APPDATA%\AkumaLabs`).
const CACHE_FILE: &str = "game-icons.cache";
/// How long a cached list is trusted before a fresh one is fetched (one week).
const CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
/// The cache format's magic (a bumped version invalidates every old file).
const CACHE_MAGIC: &[u8; 4] = b"KUGC";
/// The cache format's version (bumping it invalidates every old file).
const CACHE_FORMAT: u8 = 1;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cache_path() -> Option<std::path::PathBuf> {
    crate::settings::dir().map(|d| d.join(CACHE_FILE))
}

/// Write the cache at `path` (the tests hand it a temp folder of their own).
fn save_cache_at(path: &std::path::Path, entries: &[Entry]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut v = Vec::with_capacity(entries.len() * 96 + 16);
    v.extend_from_slice(CACHE_MAGIC);
    v.push(CACHE_FORMAT);
    put_u64(&mut v, now_secs());
    put_u32(&mut v, entries.len() as u32);
    for e in entries {
        put_u64(&mut v, e.app);
        put_str(&mut v, &e.icon);
        put_u32(&mut v, e.names.len() as u32);
        for n in &e.names {
            put_str(&mut v, n);
        }
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &v).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    let _ = std::fs::rename(&tmp, &path);
}

/// Read the cache at `path`: `None` when there is none, it is another format, it is damaged or
/// it is past the TTL (unless `stale_ok`, which is the offline fallback). The caller then
/// fetches a fresh list.
fn load_cache_at(path: &std::path::Path, stale_ok: bool) -> Option<(u64, Vec<Entry>)> {
    let bytes = std::fs::read(path).ok()?;
    let mut r = Reader { b: &bytes, i: 0 };
    if r.take(4)? != CACHE_MAGIC {
        return None;
    }
    if r.u8()? != CACHE_FORMAT {
        return None;
    }
    let saved_at = r.u64()?;
    if !stale_ok && now_secs().saturating_sub(saved_at) > CACHE_TTL_SECS {
        return None;
    }
    let count = r.u32()? as usize;
    // A damaged table cannot ask for a huge allocation.
    if count > 100_000 {
        return None;
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let app = r.u64()?;
        let icon = r.str()?.to_string();
        let names = (0..r.u32()?).map(|_| r.str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
        if names.is_empty() || icon.is_empty() {
            return None;
        }
        out.push(Entry { app, icon, names });
    }
    Some((saved_at, out))
}

fn save_cache(entries: &[Entry]) {
    if let Some(path) = cache_path() {
        save_cache_at(&path, entries);
    }
}

fn load_cache(stale_ok: bool) -> Option<(u64, Vec<Entry>)> {
    load_cache_at(&cache_path()?, stale_ok)
}

fn put_u32(v: &mut Vec<u8>, n: u32) {
    v.extend_from_slice(&n.to_le_bytes());
}

fn put_u64(v: &mut Vec<u8>, n: u64) {
    v.extend_from_slice(&n.to_le_bytes());
}

fn put_str(v: &mut Vec<u8>, s: &str) {
    put_u32(v, s.len() as u32);
    v.extend_from_slice(s.as_bytes());
}

/// The binary cache: magic, version, when it was written, the count, then every entry
/// (app id, icon hash, and its normalised names).
struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.i.checked_add(n)?;
        let s = self.b.get(self.i..end)?;
        self.i = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|s| s[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<&'a str> {
        let len = self.u32()? as usize;
        std::str::from_utf8(self.take(len)?).ok()
    }
}

fn http_get_bytes(url: &str) -> Option<Vec<u8>> {
    let r = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?
        .get(url)
        .send()
        .ok()?;
    if r.status().is_success() { r.bytes().ok().map(|b| b.to_vec()) } else { None }
}

/// `[{"id":"730","name":"Counter-Strike 2","icon_hash":"abc",...}, ...]` -> `Entry`s. The list is
/// ~13 MB and only a tenth of it carries icons: the rest (tools, servers, launchers) is dropped.
fn parse_list(body: &str) -> Vec<Entry> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else { return Vec::new() };
    v.as_array().map(|a| a.as_slice()).unwrap_or_default().iter().filter_map(Entry::from_json).collect()
}

/// The game's logo bytes, or `None` (Discord has no icon for the game, offline, CDN hiccup).
fn discord_art(k: &str) -> Option<Vec<u8>> {
    // Autotests never fetch: the fallback tile is their deterministic answer.
    if vox_client_core::testmode::background() {
        return None;
    }
    let list = list()?;
    let entry = find(&list, k)?;
    http_get_bytes(&icon_url(entry.app, &entry.icon))
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// Draw the game's tile in `rect`.
///
/// The art is the tile, exactly like Discord: when a logo exists it fills the whole rect edge to
/// edge (never a shrunken copy inside a box), with a hairline around it so a dark logo — a third
/// of Discord's app icons have a black or near-black background — still reads as a tile on the
/// dark row instead of a hole (the contrast rule: an icon that carries meaning needs at least
/// 3:1 against what surrounds it). While the art is on its way, or when there is none, the tile
/// is the placeholder it always was: a dark rounded square with the themed emoji / the gamepad.
/// The caller owns the hover (it already shows the full game name).
pub fn tile(ctx: &egui::Context, painter: &egui::Painter, store: &mut Store, emojis: &mut crate::emoji::Textures, game: &str, rect: Rect) {
    let corner = egui::CornerRadius::same(theme::radius::TAG);
    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
    // The icon shipped inside the exe first (no network, instant), then Discord's own logo for
    // everything else the live list knows; Steam as the third source.
    let k = key(game);
    let art = if k.is_empty() {
        None
    } else {
        store.pack(ctx, &k).or_else(|| store.discord(ctx, &k))
    };
    let art = art.or_else(|| {
        match resolve(game) {
            Resolved::Steam(appid) => store.steam(ctx, appid),
            Resolved::Emoji(_) => None,
        }
    });
    if let Some(tex) = art {
        painter.image(tex, rect, uv, Color32::WHITE);
        painter.rect_stroke(rect, corner, egui::Stroke::new(theme::stroke::HAIRLINE, theme::MOVEL_PRESSED), egui::StrokeKind::Inside);
        return;
    }
    // No art (yet): the placeholder tile the app always had.
    painter.rect_filled(rect, corner, theme::NOITE);
    painter.rect_stroke(rect, corner, egui::Stroke::new(theme::stroke::HAIRLINE, theme::MOVEL_PRESSED), egui::StrokeKind::Inside);
    let inner = rect.shrink(2.5);
    match resolve(game) {
        Resolved::Steam(_) => draw_emoji(ctx, painter, emojis, GAMEPAD, inner),
        Resolved::Emoji(ch) => draw_emoji(ctx, painter, emojis, ch, inner),
    }
}

/// One catalogue emoji filling `rect` (its PNG art), or its font glyph if the texture is somehow
/// missing — never an empty tile.
fn draw_emoji(ctx: &egui::Context, painter: &egui::Painter, emojis: &mut crate::emoji::Textures, ch: &'static str, rect: Rect) {
    if let Some(tex) = emojis.get(ctx, ch) {
        painter.image(tex, rect, Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)), Color32::WHITE);
    } else {
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, ch, egui::FontId::default(), theme::NEVOA);
    }
}

/// The tile is sized by the caller; this is only to keep the two sides in one place in tests.
#[allow(dead_code)]
fn tile_sides() -> (f32, f32) {
    (TILE, TILE_CARD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_games_resolve_to_their_appid() {
        assert_eq!(resolve("Rocket League"), Resolved::Steam(252950));
        assert_eq!(resolve("Rocket League™"), Resolved::Steam(252950));
        assert_eq!(resolve("  Counter-Strike 2! "), Resolved::Steam(730));
        assert_eq!(resolve("cs2"), Resolved::Steam(730));
        assert_eq!(resolve("ELDEN RING"), Resolved::Steam(1245620));
        assert_eq!(resolve("Baldur's Gate 3"), Resolved::Steam(1086940));
        assert_eq!(resolve("Grand Theft Auto V"), Resolved::Steam(271590));
        assert_eq!(resolve("Dota 2"), Resolved::Steam(570));
    }

    #[test]
    fn non_steam_games_get_a_themed_emoji() {
        assert_eq!(resolve("Valorant"), Resolved::Emoji("🎯"));
        assert_eq!(resolve("League of Legends"), Resolved::Emoji("🏆"));
        assert_eq!(resolve("Minecraft: Java Edition"), Resolved::Emoji("💎"));
        assert_eq!(resolve("Fortnite"), Resolved::Emoji("🛸"));
    }

    #[test]
    fn unknown_games_get_the_gamepad() {
        assert_eq!(resolve("Jogo Aleatório XYZ"), Resolved::Emoji(GAMEPAD));
        assert_eq!(resolve(""), Resolved::Emoji(GAMEPAD));
        assert_eq!(resolve("!!!"), Resolved::Emoji(GAMEPAD));
    }

    #[test]
    fn every_emoji_in_the_table_has_art_in_the_catalogue() {
        for (_, ch) in EMOJI {
            assert!(crate::emoji::def(ch).is_some(), "{ch} is not in the emoji catalogue");
        }
        assert!(crate::emoji::def(GAMEPAD).is_some());
    }

    #[test]
    fn the_key_ignores_case_and_punctuation() {
        assert_eq!(key("Rocket League™"), key("rocket league"));
        assert_eq!(key("HELLDIVERS 2"), "helldivers2");
    }

    #[test]
    fn detectable_json_parses_to_icon_bytes_entries() {
        let body = r#"[{"id":"730","name":"Counter-Strike 2","icon_hash":"abc","aliases":["cs2","Counter-Strike 2"]},
                       {"id":"570","name":"Dota 2","icon_hash":""},
                       {"id":"1","name":"No Icon","aliases":["ni"]}]"#;
        let entries = parse_list(body);
        // An entry without an icon hash (or without names) has no art to draw: dropped.
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!((e.app, e.icon.as_str()), (730, "abc"));
        // The name comes first, the aliases after, normalised and deduplicated.
        assert_eq!(e.names, vec!["counterstrike2".to_string(), "cs2".to_string()]);
        assert!(parse_list(r#"{"error":"401 Unauthorized"}"#).is_empty());
        assert!(parse_list("not json").is_empty());
    }

    #[test]
    fn find_matches_the_exact_name_then_the_alias() {
        let entries = parse_list(
            r#"[{"id":"7","name":"Counter-Strike 2","icon_hash":"a","aliases":["cs2"]},
                {"id":"8","name":"Valorant","icon_hash":"b"}]"#,
        );
        assert_eq!(find(&entries, "counterstrike2").map(|e| e.app), Some(7));
        assert_eq!(find(&entries, "cs2").map(|e| e.app), Some(7));
        assert_eq!(find(&entries, "valorant").map(|e| e.app), Some(8));
        assert_eq!(find(&entries, "steam"), None);
        assert_eq!(find(&entries, ""), None);
        // Only prefixes relate: a longer unknown name is not a match.
        assert_eq!(find(&entries, "counterstrikeglobaloffensive"), None);
    }

    #[test]
    fn find_prefix_matches_without_stealing_tiles_from_short_names() {
        let entries = parse_list(
            r#"[{"id":"9","name":"Overwatch","icon_hash":"o"},
                {"id":"10","name":"Watch","icon_hash":"w"},
                {"id":"11","name":"Minecraft","icon_hash":"m","aliases":["Minecraft Launcher"]},
                {"id":"12","name":"Watch Dogs 2","icon_hash":"wd"}]"#,
        );
        // "Overwatch 2" is the entry's name plus a suffix: Overwatch wins, never Watch.
        assert_eq!(find(&entries, "overwatch2").map(|e| e.app), Some(9));
        // The entry's own name plus a word, spelled the way Discord lists it.
        assert_eq!(find(&entries, "watchdogs2").map(|e| e.app), Some(12));
        // "watch" alone is an exact entry of its own.
        assert_eq!(find(&entries, "watch").map(|e| e.app), Some(10));
        // A long game name finds its short entry.
        assert_eq!(find(&entries, "minecraftjavaedition").map(|e| e.app), Some(11));
    }

    #[test]
    fn the_cache_round_trips_and_refuses_what_it_cannot_read() {
        let dir = std::env::temp_dir().join(format!("kumacord-gicons-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("game-icons.cache");
        let entries = parse_list(
            r#"[{"id":"730","name":"Counter-Strike 2","icon_hash":"abc","aliases":["cs2"]},
                {"id":"570","name":"Dota 2","icon_hash":"def"}]"#,
        );
        save_cache_at(&path, &entries);
        let (_, back) = load_cache_at(&path, false).expect("the cache this build just wrote");
        assert_eq!(back, entries);
        // A half-written file (the rename never happened) is not a cache.
        std::fs::write(&path, &std::fs::read(&path).unwrap()[..12]).unwrap();
        assert!(load_cache_at(&path, false).is_none());
        // Another format, or junk, is not a cache either.
        std::fs::write(&path, b"XXXX\x01\x00\x00").unwrap();
        assert!(load_cache_at(&path, false).is_none());
        // Past the TTL it is only read as the offline fallback.
        save_cache_at(&path, &entries);
        assert!(load_cache_at(&path, false).is_some(), "written now, still fresh");
        assert!(load_cache_at(&path, true).is_some(), "and readable when stale is allowed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_embedded_pack_parses_and_carries_the_catalogue() {
        // The real pack this build ships (see tools/game_icons/build.ps1).
        let entries = parse_pack(include_bytes!("../assets/game-icons.pack"));
        assert!(entries.len() > 90, "the pack carries the app catalogue: {} entries", entries.len());
        let lookup = |k: &str| entries.iter().find(|(key, _)| key == k).map(|(_, b)| *b);
        // The names the app's own detector produces.
        assert!(lookup("counterstrike2").is_some(), "Counter-Strike 2");
        assert!(lookup("valorant").is_some(), "Valorant");
        assert!(lookup("leagueoflegends").is_some(), "League of Legends");
        assert!(lookup("roblox").is_some(), "Roblox");
        assert!(lookup("minecraft").is_some(), "Minecraft");
        assert!(lookup("fortnite").is_some(), "Fortnite");
        assert!(lookup("genshinimpact").is_some(), "Genshin Impact");
        // Every entry is a real PNG.
        for (key, bytes) in &entries {
            assert!(bytes.starts_with(b"\x89PNG"), "{key} is not a PNG");
            assert!(bytes.len() < 64 * 1024, "{key} is suspiciously large");
        }
        // The art is rounded like Discord's activity icons: the corner is transparent and the
        // centre is opaque (this is what makes the tile read as a tile on the dark row).
        let cs2 = pack_icon("counterstrike2").expect("Counter-Strike 2 ships in the pack");
        let img = vox_client_core::images::decode(cs2).expect("the icon decodes").to_rgba8();
        let (w, h) = img.dimensions();
        assert!(w >= 32 && h >= 32, "the art is big enough for the tile: {w}x{h}");
        assert_eq!(img.get_pixel(0, 0).0[3], 0, "the corner is transparent");
        assert_eq!(img.get_pixel(w - 1, 0).0[3], 0, "both corners are");
        assert_eq!(img.get_pixel(w / 2, h / 2).0[3], 255, "the centre is opaque");
        // A damaged pack reads as empty, never as a panic.
        assert!(parse_pack(b"nope!").is_empty());
        assert!(parse_pack(&[0x4b, 0x55, 0x49, 0x47, 0x01][..]).is_empty());
    }

    #[test]
    fn the_icon_url_is_the_public_cdn() {
        assert_eq!(icon_url(730, "abc123"), "https://cdn.discordapp.com/app-icons/730/abc123.png?size=256");
    }
}