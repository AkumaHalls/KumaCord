//! "Jogando X" (Discord's "Playing X"): which game is running on this computer right now.
//!
//! Discord never reads window titles for this: it polls the native process list (a Toolhelp32
//! snapshot on Windows) and matches every executable's basename against its catalogue of games.
//! This module does the same, for the same reason — the basename is the only signal, so a game
//! minimised, in the background or fullscreen-borderless is detected exactly like a focused one,
//! and a browser tab titled "CS2" is not.
//!
//! The public surface stays small and pure: [`running_exes`] is the only part that touches the OS,
//! [`catalogue_lookup`] and [`detect_from`] are plain functions over `&str` / `&[String]` and are
//! what the unit tests exercise. The UI polls [`detect`] every [`POLL_INTERVAL`].
//!
//! Launchers are deliberately absent from [`GAMES`]: Steam, the Epic launcher, Discord itself,
//! Battle.net, Riot Client and browsers are not games, and showing "Playing Steam" next to a name
//! is exactly the bug this catalogue avoids (see the tests).

use std::time::Duration;

/// How often the running processes are re-polled. Discord uses the same order of magnitude: a
/// game that starts between two polls shows up at most this late, and each poll is a cheap
/// snapshot of the process list.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Executable basename (lowercase, no ".exe") -> the name Discord shows.
///
/// The keys are the basenames people actually have on disk, including the aliases of the same game
/// (Steam's and the publisher launcher's executables differ, and so do the 32- and 64-bit builds).
/// Anything that ships a game *launcher* rather than the game itself is not here: Steam,
/// EpicGamesLauncher, Discord, Battle.net, RiotClient, GalaxyClient, UbisoftConnect, EADesktop and
/// the browsers all have to come back `None` from [`catalogue_lookup`].
const GAMES: &[(&str, &str)] = &[
    // ---------------------------------------------------------------- shooters / competitive
    ("cs2", "Counter-Strike 2"),
    ("csgo", "Counter-Strike: Global Offensive"),
    ("cs16", "Counter-Strike 1.6"),
    ("cstrike", "Counter-Strike 1.6"),
    ("valorant", "Valorant"),
    ("pubg", "PUBG: BATTLEGROUNDS"),
    ("tslgame", "PUBG: BATTLEGROUNDS"),
    ("apexlegends", "Apex Legends"),
    ("r5apex", "Apex Legends"),
    ("overwatch", "Overwatch 2"),
    ("rainbowsix", "Tom Clancy's Rainbow Six Siege"),
    ("destiny2", "Destiny 2"),
    ("warframe", "Warframe"),
    ("teamfortress2", "Team Fortress 2"),
    ("portal2", "Portal 2"),
    // Half-Life 2 and every Source mod run the same "hl2.exe"; Discord shows "Half-Life 2" for it,
    // so this is the honest mapping (Garry's Mod has its own key below and is reached only when the
    // executable really is named after it).
    ("hl2", "Half-Life 2"),
    ("garrysmod", "Garry's Mod"),
    ("gmod", "Garry's Mod"),
    ("left4dead2", "Left 4 Dead 2"),
    ("rustclient", "Rust"),
    ("dayz", "DayZ"),
    ("arma3", "ARMA 3"),
    ("helldivers2", "HELLDIVERS 2"),
    ("spacemarines2", "Warhammer 40,000: Space Marine 2"),
    ("deadbydaylight", "Dead by Daylight"),
    ("phasmophobia", "Phasmophobia"),
    ("lethalcompany", "Lethal Company"),
    ("contentwarning", "Content Warning"),
    // ---------------------------------------------------------------- moba / blizzard
    ("leagueoflegends", "League of Legends"),
    ("league of legends", "League of Legends"),
    ("lol", "League of Legends"),
    ("dota2", "Dota 2"),
    ("dota", "Dota 2"),
    ("smite", "SMITE"),
    ("hearthstone", "Hearthstone"),
    ("diablo iv", "Diablo IV"),
    ("worldofwarcraft", "World of Warcraft"),
    ("wow", "World of Warcraft"),
    ("wow-64", "World of Warcraft"),
    // ---------------------------------------------------------------- survival / crafting / co-op
    ("minecraft", "Minecraft"),
    ("javaw", "Minecraft: Java Edition"),
    ("terraria", "Terraria"),
    ("stardewvalley", "Stardew Valley"),
    ("stardew valley", "Stardew Valley"),
    ("rimworld", "RimWorld"),
    ("rimworldwin64", "RimWorld"),
    ("factorio", "Factorio"),
    ("satisfactory", "Satisfactory"),
    ("shapez2", "shapez 2"),
    ("citiesskylines2", "Cities: Skylines II"),
    ("citiesskylinesii", "Cities: Skylines II"),
    ("astroneer", "ASTRONEER"),
    ("nms", "No Man's Sky"),
    ("valheim", "Valheim"),
    ("enshrouded", "Enshrouded"),
    ("vrising", "V Rising"),
    ("palworld", "Palworld"),
    ("oncehuman", "Once Human"),
    ("sonsoftheforest", "Sons Of The Forest"),
    ("subnautica", "Subnautica"),
    ("vampiresurvivors", "Vampire Survivors"),
    ("brotato", "Brotato"),
    ("balatro", "Balatro"),
    ("slaythespire", "Slay the Spire"),
    ("inscryption", "Inscryption"),
    ("hollowknight", "Hollow Knight"),
    ("hollow knight", "Hollow Knight"),
    ("hades", "Hades"),
    ("hades2", "Hades II"),
    ("hadesii", "Hades II"),
    // ---------------------------------------------------------------- open world / single-player
    ("eldenring", "ELDEN RING"),
    ("sekiro", "Sekiro: Shadows Die Twice"),
    ("darksoulsiii", "DARK SOULS III"),
    ("darksouls3", "DARK SOULS III"),
    ("darksoulsremastered", "Dark Souls: Remastered"),
    ("witcher3", "The Witcher 3: Wild Hunt"),
    ("cyberpunk2077", "Cyberpunk 2077"),
    ("baldursgate3", "Baldur's Gate 3"),
    ("bg3", "Baldur's Gate 3"),
    ("hogwartslegacy", "Hogwarts Legacy"),
    ("starfield", "Starfield"),
    ("fallout4", "Fallout 4"),
    ("fallout76", "Fallout 76"),
    ("skyrimse", "The Elder Scrolls V: Skyrim"),
    ("skyrim", "The Elder Scrolls V: Skyrim"),
    ("tesv", "The Elder Scrolls V: Skyrim"),
    ("gtav", "Grand Theft Auto V"),
    ("gta5", "Grand Theft Auto V"),
    ("rdr2", "Red Dead Redemption 2"),
    ("reddeadredemption2", "Red Dead Redemption 2"),
    ("monsterhunterwilds", "Monster Hunter Wilds"),
    ("monsterhunterworld", "Monster Hunter: World"),
    ("monsterhunterrise", "Monster Hunter Rise"),
    ("dragonsdogma2", "Dragon's Dogma 2"),
    ("liesofp", "Lies of P"),
    ("remnant2", "Remnant II"),
    ("residentevil4", "Resident Evil 4"),
    ("re4", "Resident Evil 4"),
    ("residentevilvillage", "Resident Evil Village"),
    ("re8", "Resident Evil Village"),
    ("pathofexile", "Path of Exile"),
    ("pathofexilesteam", "Path of Exile"),
    // ---------------------------------------------------------------- racing / driving
    ("asphalt9", "Asphalt 9: Legends"),
    ("forza", "Forza"),
    ("forzahorizon5", "Forza Horizon 5"),
    ("acs", "Assetto Corsa"),
    ("assettocorsa", "Assetto Corsa"),
    ("contentmanager", "Assetto Corsa"),
    ("content manager", "Assetto Corsa"),
    ("beamng.drive", "BeamNG.drive"),
    ("ets2te", "Euro Truck Simulator 2"),
    ("eurotrucks2", "Euro Truck Simulator 2"),
    ("trackmania", "Trackmania"),
    ("rocketleague", "Rocket League"),
    ("wreckfest", "Wreckfest"),
    // ---------------------------------------------------------------- gacha / anime
    ("genshinimpact", "Genshin Impact"),
    ("starrail", "Honkai: Star Rail"),
    ("honkaistarrail", "Honkai: Star Rail"),
    ("zenlesszonezero", "Zenless Zone Zero"),
    ("wutheringwaves", "Wuthering Waves"),
    ("honkaiimpact3", "Honkai Impact 3rd"),
    ("toweroffantasy", "Tower of Fantasy"),
    // ---------------------------------------------------------------- party / social
    ("fortnite", "Fortnite"),
    ("robloxplayer", "Roblox"),
    ("robloxplayerbeta", "Roblox"),
    ("vrchat", "VRChat"),
    ("amongus", "Among Us"),
    ("osu!", "osu!"),
];

/// The suffixes a game appends to its executable and that carry no meaning: the Unreal shipping
/// suffix, the architecture marker and the "client" of a launcher-style process name. They are
/// peeled off one at a time (each intermediate is also a candidate), so
/// "FortniteClient-Win64-Shipping" reaches "fortnite" and "arma3_x64" reaches "arma3".
const STRIPPABLE_SUFFIXES: &[&str] = &["-win64-shipping", "_x64", "-x64", "x64", "64", "client"];

/// The basename as [`GAMES`] is keyed: lowercased, without a directory and without the ".exe".
fn normalise(name: &str) -> String {
    let lowered = name.trim().to_ascii_lowercase();
    let last = lowered.rsplit(['\\', '/']).next().unwrap_or(&lowered);
    last.strip_suffix(".exe").unwrap_or(last).to_string()
}

/// Every key tried for an already-normalised basename, the basename first and then the suffixes of
/// [`STRIPPABLE_SUFFIXES`] peeled off one at a time.
fn candidate_keys(name: &str) -> Vec<&str> {
    let mut out = vec![name];
    let mut cur = name;
    loop {
        let next = STRIPPABLE_SUFFIXES.iter().find_map(|s| cur.strip_suffix(s)).filter(|s| !s.is_empty());
        match next {
            Some(shorter) => {
                out.push(shorter);
                cur = shorter;
            }
            None => break,
        }
    }
    out
}

/// The game a running executable is, or `None` when it is not in the catalogue (a launcher, a
/// browser, an editor, this very app).
///
/// Pure: the same basename always answers the same thing, so it is testable without starting a
/// single process. The exact basename wins first; only then the suffixes of
/// [`STRIPPABLE_SUFFIXES`] are peeled off, so "cs2" never becomes "cs" and "arma3_x64" does reach
/// "arma3".
pub fn catalogue_lookup(basename: &str) -> Option<&'static str> {
    let key = normalise(basename);
    if key.is_empty() {
        return None;
    }
    candidate_keys(&key).into_iter().find_map(|c| GAMES.iter().find(|(k, _)| *k == c).map(|(_, name)| *name))
}

/// The game of the first catalogue hit among `exes`, or `None`.
///
/// The list is sorted first, so the answer never flickers between polls: two games running at the
/// same time always resolve to the same one, whichever order the process snapshot came out in.
pub fn detect_from(exes: &[String]) -> Option<&'static str> {
    let mut sorted: Vec<&str> = exes.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.iter().find_map(|exe| catalogue_lookup(exe))
}

/// The game running right now, if any. Pure core: [`detect_from`] over [`running_exes`].
pub fn detect() -> Option<&'static str> {
    detect_from(&running_exes())
}

// ---------------------------------------------------------------- Windows: the real thing

/// The executable basenames (lowercased, without ".exe") of the processes running right now.
///
/// A Toolhelp32 process snapshot: the same call Discord makes, and the only one that gives the
/// executable name of every process without opening a handle on each. Empty on any failure (no
/// snapshot, no first entry): a failed poll is indistinguishable from "nothing running", which is
/// the safe answer for a presence field.
#[cfg(windows)]
pub fn running_exes() -> Vec<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};

    let mut out: Vec<String> = Vec::new();
    // SAFETY: `snap` is an opaque read-only handle to a snapshot of the process list, used only as
    // an argument to Process32FirstW/NextW and closed with CloseHandle on every path below.
    // `entry` is a local struct whose `dwSize` is set to its own size before the first call, which
    // is what both APIs require, and its `szExeFile` is a fixed-size buffer owned by this frame.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snap, &mut entry);
        while more != 0 {
            if let Some(name) = basename_from_wide(&entry.szExeFile) {
                out.push(name);
            }
            more = Process32NextW(snap, &mut entry);
        }
        CloseHandle(snap);
    }
    out.sort();
    out.dedup();
    out
}

/// The basename in a `szExeFile` buffer: up to the first NUL, the last path segment (some builds
/// hand back a full path), lowercased and without the ".exe".
#[cfg(windows)]
fn basename_from_wide(buf: &[u16]) -> Option<String> {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    if end == 0 {
        return None;
    }
    let full = String::from_utf16_lossy(&buf[..end]).to_ascii_lowercase();
    let name = full.rsplit(['\\', '/']).next().unwrap_or(&full);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Non-Windows: nothing to poll, the feature is Windows-only (as it is in Discord).
#[cfg(not(windows))]
pub fn running_exes() -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalogue itself: lowercase keys, no ".exe", no duplicate keys, every key reachable, and
    /// a sane number of distinct games (the aliases do not count towards it).
    #[test]
    fn games_catalogue_is_well_formed() {
        let mut keys = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        for (key, name) in GAMES {
            assert_eq!(*key, key.to_ascii_lowercase(), "{key} must be lowercase");
            assert!(!key.ends_with(".exe"), "{key} must not carry the extension");
            assert!(!key.is_empty() && !name.is_empty(), "{key}");
            assert!(keys.insert(*key), "{key} is listed twice");
            names.insert(*name);
            assert_eq!(catalogue_lookup(key).as_deref(), Some(*name), "{key} must resolve to itself");
        }
        assert!(names.len() >= 70, "only {} games catalogued", names.len());
        assert!(names.len() <= 100, "{} games catalogued, that is a lot to keep honest", names.len());
    }

    /// The aliases people actually have on disk, one per common family.
    #[test]
    fn games_catalogue_lookup_finds_the_common_ones_and_their_aliases() {
        let cases = [
            ("cs2", "Counter-Strike 2"),
            ("csgo", "Counter-Strike: Global Offensive"),
            ("cs16", "Counter-Strike 1.6"),
            ("valorant", "Valorant"),
            ("leagueoflegends", "League of Legends"),
            ("league of legends", "League of Legends"),
            ("lol", "League of Legends"),
            ("dota2", "Dota 2"),
            ("apexlegends", "Apex Legends"),
            ("r5apex", "Apex Legends"),
            ("pubg", "PUBG: BATTLEGROUNDS"),
            ("overwatch", "Overwatch 2"),
            ("destiny2", "Destiny 2"),
            ("warframe", "Warframe"),
            ("teamfortress2", "Team Fortress 2"),
            ("hl2", "Half-Life 2"),
            ("portal2", "Portal 2"),
            ("garrysmod", "Garry's Mod"),
            ("left4dead2", "Left 4 Dead 2"),
            ("rustclient", "Rust"),
            ("dayz", "DayZ"),
            ("arma3", "ARMA 3"),
            ("hearthstone", "Hearthstone"),
            ("diablo iv", "Diablo IV"),
            ("worldofwarcraft", "World of Warcraft"),
            ("wow", "World of Warcraft"),
            ("pathofexile", "Path of Exile"),
            ("pathofexilesteam", "Path of Exile"),
            ("minecraft", "Minecraft"),
            ("javaw", "Minecraft: Java Edition"),
            ("terraria", "Terraria"),
            ("stardewvalley", "Stardew Valley"),
            ("rimworldwin64", "RimWorld"),
            ("factorio", "Factorio"),
            ("satisfactory", "Satisfactory"),
            ("citiesskylines2", "Cities: Skylines II"),
            ("valheim", "Valheim"),
            ("palworld", "Palworld"),
            ("lethalcompany", "Lethal Company"),
            ("phasmophobia", "Phasmophobia"),
            ("deadbydaylight", "Dead by Daylight"),
            ("helldivers2", "HELLDIVERS 2"),
            ("monsterhunterwilds", "Monster Hunter Wilds"),
            ("eldenring", "ELDEN RING"),
            ("sekiro", "Sekiro: Shadows Die Twice"),
            ("darksoulsiii", "DARK SOULS III"),
            ("witcher3", "The Witcher 3: Wild Hunt"),
            ("cyberpunk2077", "Cyberpunk 2077"),
            ("baldursgate3", "Baldur's Gate 3"),
            ("bg3", "Baldur's Gate 3"),
            ("hogwartslegacy", "Hogwarts Legacy"),
            ("starfield", "Starfield"),
            ("fallout4", "Fallout 4"),
            ("fallout76", "Fallout 76"),
            ("skyrimse", "The Elder Scrolls V: Skyrim"),
            ("tesv", "The Elder Scrolls V: Skyrim"),
            ("gtav", "Grand Theft Auto V"),
            ("gta5", "Grand Theft Auto V"),
            ("rdr2", "Red Dead Redemption 2"),
            ("reddeadredemption2", "Red Dead Redemption 2"),
            ("asphalt9", "Asphalt 9: Legends"),
            ("forzahorizon5", "Forza Horizon 5"),
            ("acs", "Assetto Corsa"),
            ("contentmanager", "Assetto Corsa"),
            ("beamng.drive", "BeamNG.drive"),
            ("ets2te", "Euro Truck Simulator 2"),
            ("eurotrucks2", "Euro Truck Simulator 2"),
            ("trackmania", "Trackmania"),
            ("rocketleague", "Rocket League"),
            ("genshinimpact", "Genshin Impact"),
            ("starrail", "Honkai: Star Rail"),
            ("honkaistarrail", "Honkai: Star Rail"),
            ("zenlesszonezero", "Zenless Zone Zero"),
            ("wutheringwaves", "Wuthering Waves"),
            ("toweroffantasy", "Tower of Fantasy"),
            ("fortnite", "Fortnite"),
            ("robloxplayer", "Roblox"),
            ("osu!", "osu!"),
            ("balatro", "Balatro"),
            ("hollowknight", "Hollow Knight"),
            ("hades2", "Hades II"),
        ];
        for (exe, want) in cases {
            assert_eq!(catalogue_lookup(exe).as_deref(), Some(want), "{exe}");
        }
    }

    /// The normalisations: the exact basename wins, then the shipping / architecture / client
    /// suffixes come off one at a time. Case and the extension never matter.
    #[test]
    fn games_catalogue_lookup_normalises_case_extension_and_suffixes() {
        // The extension and the case: the OS hands back the name as the game wrote it.
        assert_eq!(catalogue_lookup("CS2.exe").as_deref(), Some("Counter-Strike 2"));
        assert_eq!(catalogue_lookup("VALORANT-Win64-Shipping.EXE").as_deref(), Some("Valorant"));
        assert_eq!(catalogue_lookup("DayZ_X64.exe").as_deref(), Some("DayZ"));
        assert_eq!(catalogue_lookup("PathOfExileSteam_x64.exe").as_deref(), Some("Path of Exile"));
        assert_eq!(catalogue_lookup("BeamNG.drive_x64.exe").as_deref(), Some("BeamNG.drive"));
        // Unreal's shipping suffix, then the "client" of a launcher-style process name.
        assert_eq!(catalogue_lookup("FortniteClient-Win64-Shipping").as_deref(), Some("Fortnite"));
        assert_eq!(catalogue_lookup("fortniteclient-win64-shipping.exe").as_deref(), Some("Fortnite"));
        assert_eq!(catalogue_lookup("RustClient").as_deref(), Some("Rust"));
        assert_eq!(catalogue_lookup("R5Apex").as_deref(), Some("Apex Legends"));
        // A path, when a build hands one back.
        assert_eq!(catalogue_lookup(r"C:\Games\CS2\bin\cs2.exe").as_deref(), Some("Counter-Strike 2"));
        // Only the trailing "64" comes off: "cs2" stays "cs2", never "cs".
        assert_eq!(catalogue_lookup("cs2").as_deref(), Some("Counter-Strike 2"));
        // Peeling never empties a key into a match.
        assert_eq!(catalogue_lookup("client-win64-shipping"), None);
    }

    /// Launchers, browsers, editors and this app are not games.
    #[test]
    fn games_catalogue_lookup_never_matches_launchers_or_browsers() {
        for exe in [
            "steam",
            "steam.exe",
            "steamwebhelper",
            "epicgameslauncher",
            "epicwebhelper",
            "discord",
            "discordptb",
            "battle.net",
            "riotclient",
            "riotclientux",
            "bethesdanetwork",
            "ubisoftconnect",
            "goggalaxy",
            "galaxyclient",
            "origin",
            "eaapp",
            "eadesktop",
            "xboxapp",
            "minecraftlauncher",
            "chrome",
            "msedge",
            "firefox",
            "brave",
            "opera",
            "vivaldi",
            "safari",
            "code",
            "devenv",
            "notepad",
            "explorer",
            "spotify",
            "obs64",
            "kumacord",
        ] {
            assert_eq!(catalogue_lookup(exe), None, "{exe} is not a game");
        }
    }

    /// Anything unknown answers `None` instead of guessing.
    #[test]
    fn games_catalogue_lookup_unknown_executables_are_none() {
        for exe in ["", "  ", "mygame2025", "notagame", "a", "game.exe", "cs3", "csgo2"] {
            assert_eq!(catalogue_lookup(exe), None, "{exe:?}");
        }
    }

    /// `detect_from` is the pure core: nothing running, nothing to show.
    #[test]
    fn games_detect_from_an_empty_list_is_none() {
        assert_eq!(detect_from(&[]), None);
        let only_launchers = vec!["steam".to_string(), "discord".to_string(), "chrome".to_string()];
        assert_eq!(detect_from(&only_launchers), None);
    }

    /// The first hit in sorted order, so the answer never depends on the snapshot's order.
    #[test]
    fn games_detect_from_sorts_before_matching_so_it_never_flickers() {
        let a = ["zzz".to_string(), "cs2".to_string(), "explorer".to_string()];
        let b = ["explorer".to_string(), "cs2".to_string(), "zzz".to_string()];
        assert_eq!(detect_from(&a).as_deref(), Some("Counter-Strike 2"));
        assert_eq!(detect_from(&b).as_deref(), Some("Counter-Strike 2"));
        assert_eq!(detect_from(&a), detect_from(&b), "the same set must always answer the same");
        // With two games up, the alphabetically first basename wins, every time.
        let two = ["valorant".to_string(), "cs2".to_string()];
        let flipped = ["cs2".to_string(), "valorant".to_string()];
        assert_eq!(detect_from(&two).as_deref(), Some("Counter-Strike 2"));
        assert_eq!(detect_from(&flipped).as_deref(), Some("Counter-Strike 2"));
    }

    /// The real list comes out of `running_exes` already sorted and deduplicated, but `detect`
    /// works over whatever it is given.
    #[test]
    fn games_detect_over_a_realistic_process_list() {
        let exes = [
            "kumacord".to_string(),
            "steam".to_string(),
            "steamwebhelper".to_string(),
            "explorer".to_string(),
            "cs2".to_string(),
            "discord".to_string(),
        ];
        assert_eq!(detect_from(&exes).as_deref(), Some("Counter-Strike 2"));
    }

    /// The suffix peeling is total: it always terminates and never yields an empty key.
    #[test]
    fn games_candidate_keys_peel_suffixes_one_at_a_time() {
        assert_eq!(candidate_keys("fortniteclient-win64-shipping"), vec!["fortniteclient-win64-shipping", "fortniteclient", "fortnite"]);
        assert_eq!(candidate_keys("arma3_x64"), vec!["arma3_x64", "arma3"]);
        assert_eq!(candidate_keys("cs2"), vec!["cs2"]);
        assert_eq!(candidate_keys("client-win64-shipping"), vec!["client-win64-shipping", "client"]);
    }
}
