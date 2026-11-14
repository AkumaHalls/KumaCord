//! The game's own tile next to "Jogando X" — the little icon Discord draws from the game's art.
//!
//! The activity only carries the game's name, so [`resolve`] matches that name against a
//! built-in table: a Steam appid (the capsule art on Steam's public CDN, fetched once in a
//! thread and cached as a texture) or, for games that are not on Steam, a themed emoji from
//! the app's own Twemoji catalogue (drawn at once, no fetch). Anything unknown gets the
//! gamepad. No key and no account: the CDN is public, and a fetch that fails just leaves the
//! gamepad tile there.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use eframe::egui::{self, Color32, Pos2, Rect, TextureId, Vec2};

use crate::theme;

/// Side of the tile in the member rows (the square next to "Jogando X").
pub const TILE: f32 = 16.0;
/// Side of the tile in the profile card.
pub const TILE_CARD: f32 = 22.0;

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

/// The Steam art of each appid this session asked for: loading, ready, or failed (failed stays
/// failed for the session — a CDN hiccup is not worth re-hammering it every frame).
enum State {
    Loading,
    Ready(TextureId),
    Failed,
}

/// The session's cache of game tiles. The fetch runs on its own thread and comes back through a
/// channel, so the UI never blocks; [`Store::steam`] drains the channel into textures.
pub struct Store {
    map: HashMap<u32, State>,
    tx: Sender<(u32, Option<Vec<u8>>)>,
    rx: Receiver<(u32, Option<Vec<u8>>)>,
}

impl Store {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Store { map: HashMap::new(), tx, rx }
    }

    /// Turn the fetches that finished into textures. Called from [`Store::steam`], which the
    /// tile draw calls every frame while the row is visible.
    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok((appid, bytes)) = self.rx.try_recv() {
            let state = match bytes.and_then(|b| vox_client_core::images::decode(&b).ok()) {
                Some(img) => {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    State::Ready(ctx.load_texture(format!("game/{appid}"), color, egui::TextureOptions::LINEAR).id())
                }
                None => State::Failed,
            };
            self.map.insert(appid, state);
        }
    }

    /// Start the fetch of `appid`'s capsule art if nobody did yet.
    fn ensure(&mut self, appid: u32) {
        if self.map.contains_key(&appid) {
            return;
        }
        self.map.insert(appid, State::Loading);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // capsule_184x69.jpg is the small header art (~15 KB): enough for a 16-22 px tile.
            let url = format!("https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/capsule_184x69.jpg");
            let bytes = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .ok()
                .and_then(|c| c.get(&url).send().ok())
                .and_then(|r| if r.status().is_success() { r.bytes().ok().map(|b| b.to_vec()) } else { None });
            let _ = tx.send((appid, bytes));
        });
    }

    /// The texture of `appid`'s art if it has arrived; starts the fetch on the first call and
    /// answers `None` (the caller draws the gamepad meanwhile) until then.
    pub fn steam(&mut self, ctx: &egui::Context, appid: u32) -> Option<TextureId> {
        self.poll(ctx);
        self.ensure(appid);
        match self.map.get(&appid) {
            Some(State::Ready(id)) => Some(*id),
            _ => None,
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

/// Draw the game's tile in `rect`: a dark rounded square with a hairline (concentric with the
/// row's own radius), the Steam art when it arrived, else the themed emoji / the gamepad while
/// it loads. The caller owns the hover (it already shows the full game name).
pub fn tile(ctx: &egui::Context, painter: &egui::Painter, store: &mut Store, emojis: &mut crate::emoji::Textures, game: &str, rect: Rect) {
    let corner = egui::CornerRadius::same(theme::radius::TAG);
    painter.rect_filled(rect, corner, theme::NOITE);
    painter.rect_stroke(rect, corner, egui::Stroke::new(theme::stroke::HAIRLINE, theme::MOVEL_PRESSED), egui::StrokeKind::Inside);
    let inner = rect.shrink(2.5);
    let uv = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0));
    match resolve(game) {
        Resolved::Steam(appid) => {
            if let Some(tex) = store.steam(ctx, appid) {
                painter.image(tex, inner, uv, Color32::WHITE);
            } else {
                draw_emoji(ctx, painter, emojis, GAMEPAD, inner);
            }
        }
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
}
