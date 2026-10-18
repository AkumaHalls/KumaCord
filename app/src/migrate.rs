//! v1.0.1: voxhub became concord, and its folders moved with the name:
//! - `%APPDATA%\voxhub` → `%APPDATA%\concord`: settings.json, servers.json, encoders.json;
//! - `%LOCALAPPDATA%\voxhub` → `%LOCALAPPDATA%\concord`: `host\` (the servers hosted in the app,
//!   their databases and the downloaded voice server) and `sounds\` (the soundboard's cache).
//!   The logs are not copied: concord starts its own in `%LOCALAPPDATA%\concord\logs`.
//!
//! On the first start of concord the old items are COPIED (never moved): the old folders stay as
//! they were, so going back to a voxhub version still finds everything. The DPAPI-encrypted values
//! in settings.json are tied to the Windows user, not to the path, so they keep working.
//!
//! When: the old folder has one of the items and the new one has none of them yet. (A `logs`
//! folder alone, from a test run or an early crash, does not count as "already migrated".) Each
//! item is copied under a temporary name and then renamed, so an interrupted copy is never taken
//! for a finished one: the next start throws the leftover away and copies again.
//!
//! Autotests never migrate (they must not read or write the real profile).

use std::path::{Path, PathBuf};

/// The folder name before v1.0.1.
pub const OLD_DIR: &str = "voxhub";
/// The folder name since v1.0.1.
pub const NEW_DIR: &str = "concord";

/// What lives in `%APPDATA%\<app>`.
pub const ROAMING_ITEMS: &[&str] = &["settings.json", "servers.json", "encoders.json"];
/// What lives in `%LOCALAPPDATA%\<app>` (not `logs`).
pub const LOCAL_ITEMS: &[&str] = &["host", "sounds"];

/// The suffix of an item while it is being copied.
const PARTIAL: &str = ".migrating";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The old folder has data and the new one has none of it: copy.
    Copy,
    /// Nothing to bring (a new user, or a folder of another kind).
    NothingOld,
    /// The new folder already has data of its own: leave both alone.
    AlreadyMigrated,
}

/// Whether to copy `items` from `old_root` to `new_root`.
pub fn decide(old_root: &Path, new_root: &Path, items: &[&str]) -> Decision {
    // (An empty folder, e.g. made by merely looking at the list of hosted servers, is no data.)
    let has = |p: &Path| p.is_file() || std::fs::read_dir(p).is_ok_and(|mut d| d.next().is_some());
    let has_any = |root: &Path| items.iter().any(|i| has(&root.join(i)));
    if !has_any(old_root) {
        Decision::NothingOld
    } else if has_any(new_root) {
        Decision::AlreadyMigrated
    } else {
        Decision::Copy
    }
}

/// What one root's migration did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// The items copied (in the order of the list).
    pub items: Vec<String>,
    pub files: u64,
    pub bytes: u64,
    /// Files that could not be copied (the item is then left out).
    pub errors: Vec<String>,
}

/// Copy the `items` that exist under `old_root` into `new_root`, if [`decide`] says so.
pub fn migrate_root(old_root: &Path, new_root: &Path, items: &[&str]) -> (Decision, Report) {
    let decision = decide(old_root, new_root, items);
    let mut report = Report::default();
    if decision != Decision::Copy {
        return (decision, report);
    }
    if let Err(e) = std::fs::create_dir_all(new_root) {
        report.errors.push(format!("{}: {e}", new_root.display()));
        return (decision, report);
    }
    for item in items {
        let from = old_root.join(item);
        if !from.exists() {
            continue;
        }
        let to = new_root.join(item);
        let tmp = new_root.join(format!("{item}{PARTIAL}"));
        remove_any(&tmp);
        let (mut files, mut bytes) = (0, 0);
        let copied = copy_any(&from, &tmp, &mut files, &mut bytes).and_then(|()| std::fs::rename(&tmp, &to));
        match copied {
            Ok(()) => {
                report.items.push(item.to_string());
                report.files += files;
                report.bytes += bytes;
            }
            Err(e) => {
                remove_any(&tmp);
                report.errors.push(format!("{item}: {e}"));
            }
        }
    }
    (decision, report)
}

fn copy_any(from: &Path, to: &Path, files: &mut u64, bytes: &mut u64) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_any(&entry.path(), &to.join(entry.file_name()), files, bytes)?;
        }
        Ok(())
    } else if meta.is_file() {
        *bytes += std::fs::copy(from, to)?;
        *files += 1;
        Ok(())
    } else {
        // (A link or anything else the app never makes: left behind.)
        Ok(())
    }
}

fn remove_any(p: &Path) {
    if p.is_dir() {
        let _ = std::fs::remove_dir_all(p);
    } else if p.exists() {
        let _ = std::fs::remove_file(p);
    }
}

/// The two migrations of a real start, from the environment's `APPDATA` / `LOCALAPPDATA`.
/// Returns one line for the log (written once logging is up), or `None` when nothing happened.
pub fn run_at_startup() -> Option<String> {
    let mut parts = Vec::new();
    let mut total_files = 0;
    let mut errors = Vec::new();
    for (var, items) in [("APPDATA", ROAMING_ITEMS), ("LOCALAPPDATA", LOCAL_ITEMS)] {
        let Some(base) = std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from) else { continue };
        let (decision, report) = migrate_root(&base.join(OLD_DIR), &base.join(NEW_DIR), items);
        if decision == Decision::Copy {
            parts.push(format!("{var}: {}", report.items.join(", ")));
            total_files += report.files;
            errors.extend(report.errors);
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut line = format!("data copied from the voxhub folders (they stay for a downgrade): {} ({total_files} files)", parts.join("; "));
    if !errors.is_empty() {
        line.push_str(&format!("; not copied: {}", errors.join("; ")));
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("concord-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_lists_are_what_the_app_keeps() {
        assert_eq!(ROAMING_ITEMS, ["settings.json", "servers.json", "encoders.json"]);
        assert_eq!(LOCAL_ITEMS, ["host", "sounds"]);
        assert!(!LOCAL_ITEMS.contains(&"logs"), "concord starts its own logs");
        assert_eq!((OLD_DIR, NEW_DIR), ("voxhub", "concord"));
        // The files the app reads are the ones in the list.
        let settings = crate::settings::encoder_cache_path();
        if let Some(p) = settings {
            assert!(ROAMING_ITEMS.contains(&p.file_name().unwrap().to_str().unwrap()));
        }
    }

    #[test]
    fn decision() {
        let d = tmp("decide");
        let (old, new) = (d.join("voxhub"), d.join("concord"));
        // Nothing anywhere.
        assert_eq!(decide(&old, &new, ROAMING_ITEMS), Decision::NothingOld);
        // An old folder without any of our items.
        std::fs::create_dir_all(old.join("logs")).unwrap();
        assert_eq!(decide(&old, &new, LOCAL_ITEMS), Decision::NothingOld);
        // Old data, no new folder.
        std::fs::write(old.join("settings.json"), "{}").unwrap();
        assert_eq!(decide(&old, &new, ROAMING_ITEMS), Decision::Copy);
        // A new folder with only logs (a test run): still copy.
        std::fs::create_dir_all(new.join("logs")).unwrap();
        assert_eq!(decide(&old, &new, ROAMING_ITEMS), Decision::Copy);
        // A leftover of an interrupted copy does not count either.
        std::fs::write(new.join("settings.json.migrating"), "{").unwrap();
        assert_eq!(decide(&old, &new, ROAMING_ITEMS), Decision::Copy);
        // Nor does an empty folder of ours (local items), on either side.
        std::fs::create_dir_all(new.join("host")).unwrap();
        std::fs::create_dir_all(old.join("host")).unwrap();
        assert_eq!(decide(&old, &new, LOCAL_ITEMS), Decision::NothingOld);
        std::fs::write(old.join("host").join("x.toml"), "").unwrap();
        assert_eq!(decide(&old, &new, LOCAL_ITEMS), Decision::Copy);
        // The new folder has data of its own: never overwritten.
        std::fs::write(new.join("servers.json"), "[]").unwrap();
        assert_eq!(decide(&old, &new, ROAMING_ITEMS), Decision::AlreadyMigrated);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copies_files_and_folders_and_leaves_the_old_ones() {
        let d = tmp("copy");
        let (old, new) = (d.join("voxhub"), d.join("concord"));
        std::fs::create_dir_all(old.join("host").join("abc")).unwrap();
        std::fs::create_dir_all(old.join("host").join("bin")).unwrap();
        std::fs::create_dir_all(old.join("logs").join("app")).unwrap();
        std::fs::write(old.join("host").join("abc").join("voxhub.db"), b"db").unwrap();
        std::fs::write(old.join("host").join("abc").join("config.toml"), b"server_name = \"x\"").unwrap();
        std::fs::write(old.join("host").join("bin").join("livekit-server.exe"), b"exe").unwrap();
        std::fs::write(old.join("logs").join("app").join("app.log"), b"log").unwrap();
        // A leftover from an interrupted copy is replaced.
        std::fs::create_dir_all(new.join("host.migrating").join("junk")).unwrap();

        let (decision, report) = migrate_root(&old, &new, LOCAL_ITEMS);
        assert_eq!(decision, Decision::Copy);
        assert_eq!(report.items, ["host"], "sounds did not exist");
        assert_eq!((report.files, report.bytes), (3, 2 + 17 + 3));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(std::fs::read(new.join("host").join("abc").join("voxhub.db")).unwrap(), b"db");
        assert_eq!(std::fs::read(new.join("host").join("bin").join("livekit-server.exe")).unwrap(), b"exe");
        assert!(!new.join("host.migrating").exists());
        assert!(!new.join("host").join("junk").exists());
        assert!(!new.join("logs").exists(), "the logs are not copied");
        // The old folder is untouched (a downgrade still works).
        assert_eq!(std::fs::read(old.join("host").join("abc").join("voxhub.db")).unwrap(), b"db");
        assert!(old.join("logs").join("app").join("app.log").exists());

        // Once is enough: a second start changes nothing, even after the old data changed.
        std::fs::write(old.join("host").join("abc").join("voxhub.db"), b"newer").unwrap();
        let (again, r2) = migrate_root(&old, &new, LOCAL_ITEMS);
        assert_eq!(again, Decision::AlreadyMigrated);
        assert!(r2.items.is_empty());
        assert_eq!(std::fs::read(new.join("host").join("abc").join("voxhub.db")).unwrap(), b"db");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn settings_files_keep_their_content() {
        let d = tmp("roaming");
        let (old, new) = (d.join("voxhub"), d.join("concord"));
        std::fs::create_dir_all(&old).unwrap();
        for (i, f) in ROAMING_ITEMS.iter().enumerate() {
            std::fs::write(old.join(f), format!("{{\"n\":{i}}}")).unwrap();
        }
        let (decision, report) = migrate_root(&old, &new, ROAMING_ITEMS);
        assert_eq!(decision, Decision::Copy);
        assert_eq!(report.items, ROAMING_ITEMS);
        for (i, f) in ROAMING_ITEMS.iter().enumerate() {
            assert_eq!(std::fs::read_to_string(new.join(f)).unwrap(), format!("{{\"n\":{i}}}"));
            assert!(old.join(f).exists());
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
