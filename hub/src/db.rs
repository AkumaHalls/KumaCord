//! SQLite storage. One connection behind a mutex; calls run on the blocking pool.
//!
//! Schema history (`PRAGMA user_version`):
//! - 0: v0.1 to v0.4.1 (users, sessions, channels, messages, link previews).
//! - 1: item 12, roles and identity. Purely additive: new columns on `users` (all with defaults,
//!   so an older hub still works on a migrated database) and the new `device_tokens` and
//!   `audit_log` tables. The migration checks every column before adding it, so it is safe to run
//!   any number of times and on a half-migrated file.
//! - 2: "quem hospeda vira dono". One new table, `server_meta` (key/value; `owner_user_id` is the
//!   owner when `config.toml` does not pin one). Nothing existing changes, so v0.4.1 and v0.5.0
//!   hubs still run on a migrated database (they never read it).
//! - 3: profiles. One new nullable column, `users.display_name`, and one new table, `profiles`
//!   (bio, colour, custom status, chosen presence). A v0.5.2 hub never reads either, so it still
//!   runs on a migrated database; the column check keeps the migration repeatable.
//! - 4: chat images. Three new tables (`attachment_blobs`, `attachments`, `attachment_uploads`,
//!   see `attachments.rs`); the files themselves are on disk. Nothing existing changes, so a
//!   v0.6.0 hub still runs on a migrated database (it never reads them).
//! - 5: rollback safety. `sessions.auth_epoch` (default 0): this hub writes [`SESSION_EPOCH`] and
//!   accepts nothing older, so a session an older hub created while the server was rolled back
//!   (pre-v0.5 hubs never ask for personal passwords) stops working once this version is back.
//!   When the column is added, sessions of names protected by a personal password are not
//!   carried over (nobody can tell which hub made them; those people log in again once, which the
//!   app does by itself with its remembered device). Plus two triggers that drop a message's
//!   images with it whichever hub deletes it (`attachments::init_links`). Older hubs still run on
//!   the file: their `INSERT INTO sessions` names its columns and gets the default.
//!
//! Mentions, replies and reactions (v0.8, `chat::init`) need no version number of their own: new
//! columns with defaults on `messages` / `dm_messages` (each checked before it is added), two new
//! tables and two triggers, all repeatable. A v0.7.0 hub still runs on the file (it never reads
//! them, its `INSERT`s name their columns, and the triggers clean up after its deletes).
//!
//! Ids that are never reused (v0.8.2, `ids.rs`) need no version number either: one new table,
//! `id_high_water`, and inserts that name their id. Older hubs never read the table and keep
//! inserting with SQLite's own `MAX(id) + 1`, which can never collide with an id this hub hands
//! out.
//!
//! Private channels (v0.9, `access.rs`) need no version number either: `channels.private`
//! (default 0), two new tables of access lists and a trigger, all repeatable. But an older hub
//! must **not** run on a file with private channels (it would show them to everybody), so while
//! one exists `channels` holds a guard row that every older hub fails to read at start (its
//! `position` is text): it refuses to start instead. Without private channels the guard is gone
//! and older hubs run on the file as before. Every query here skips the guard (`id > 0`).

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use vox_proto::{AuditEntry, Channel, ChannelCategory, ChannelId, ChannelKind, CustomRole, Message, Role, User, UserId, badge, perm};

use crate::util::now_ms;

const SESSION_TTL_MS: i64 = 30 * 24 * 3600 * 1000;
/// A remembered device stays valid this long after it was last used.
pub const DEVICE_TOKEN_TTL_MS: i64 = 180 * 24 * 3600 * 1000;
pub const SCHEMA_VERSION: i64 = 5;
/// What [`login`] writes in `sessions.auth_epoch`; [`user_for_session`] accepts nothing less.
pub const SESSION_EPOCH: i64 = 1;

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Db> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;",
        )?;
        init(&mut conn)?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    /// Run `f` on the blocking pool with the connection locked.
    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = db.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut conn)
        })
        .await?
        .map_err(Into::into)
    }

    /// Run `f` right here (the command-line tools, which have no runtime).
    pub fn call_now<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        let mut conn = self.0.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn).map_err(Into::into)
    }
}

fn init(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY,
            username TEXT NOT NULL UNIQUE COLLATE NOCASE,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            token_hash TEXT PRIMARY KEY,
            user_id INTEGER NOT NULL REFERENCES users(id),
            created_at_ms INTEGER NOT NULL,
            expires_at_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS channels (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('text','voice')),
            position INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY,
            channel_id INTEGER NOT NULL REFERENCES channels(id),
            user_id INTEGER NOT NULL REFERENCES users(id),
            content TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS messages_channel_id ON messages(channel_id, id);",
    )?;
    crate::linkpreview::store::init(conn)?;
    migrate(conn)?;
    crate::sounds::init(conn)?;
    crate::dms::init(conn)?;
    crate::chat::init(conn)?;
    // ---- v0.8 invites (feature D): two new tables, `IF NOT EXISTS`, nothing existing changes
    // (a v0.7.0 hub never reads them). See `invites.rs`.
    crate::invites::init(conn)?;
    // ---- end invites
    crate::attachments::init_links(conn)?;
    // ---- v0.8.2: ids are never handed out twice (`ids.rs`); after every table exists.
    crate::ids::init(conn)?;
    // ---- v0.9: private channels (`access.rs`): one column, two tables, a trigger, and the
    // downgrade guard row while a channel is private.
    crate::access::init(conn)?;
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM channels WHERE id > 0", [], |r| r.get(0))?;
    if n == 0 {
        for (name, kind) in [("geral", ChannelKind::Text), ("Sala 1", ChannelKind::Voice), ("Sala 2", ChannelKind::Voice)] {
            create_channel(conn, name, kind)?;
        }
    }
    let now = now_ms();
    conn.execute("DELETE FROM sessions WHERE expires_at_ms < ?", [now])?;
    conn.execute("DELETE FROM device_tokens WHERE expires_at_ms < ?", [now])?;
    Ok(())
}

pub(crate) fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|n| n.eq_ignore_ascii_case(column)))
}

/// Bring an older database up to [`SCHEMA_VERSION`]. Additive and idempotent (see the module docs).
pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (column, decl) in [
        ("role", "TEXT NOT NULL DEFAULT 'member'"),
        ("password_hash", "TEXT"),
        ("reset_hash", "TEXT"),
        ("reset_expires_ms", "INTEGER"),
        ("server_muted", "INTEGER NOT NULL DEFAULT 0"),
        // Profile pictures: the version of the stored picture (the images are in `avatars`).
        ("avatar_version", "TEXT"),
        // Profiles: what people see instead of the username (the rest is in `profiles`).
        ("display_name", "TEXT"),
        // Badges (v1.1): the materialized effective flags — the automatic rules
        // (`badge_grants` is the manual half) recompute them; see [`recompute_badges`].
        ("badges", "INTEGER NOT NULL DEFAULT 0"),
        // Banners (v1.1): the version of the profile banner (the image is in `banner_blobs`).
        ("banner_version", "TEXT"),
    ] {
        if !has_column(&tx, "users", column)? {
            tx.execute_batch(&format!("ALTER TABLE users ADD COLUMN {column} {decl}"))?;
        }
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS device_tokens (
            token_hash TEXT PRIMARY KEY,
            user_id INTEGER NOT NULL REFERENCES users(id),
            created_at_ms INTEGER NOT NULL,
            last_used_ms INTEGER NOT NULL,
            expires_at_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS device_tokens_user ON device_tokens(user_id);
        CREATE TABLE IF NOT EXISTS server_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS audit_log (
            id INTEGER PRIMARY KEY,
            at_ms INTEGER NOT NULL,
            actor_id INTEGER,
            action TEXT NOT NULL,
            target_id INTEGER,
            detail TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS badge_grants (
            user_id INTEGER PRIMARY KEY REFERENCES users(id),
            flags INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS banned (
            user_id INTEGER PRIMARY KEY REFERENCES users(id),
            banned_by INTEGER,
            at_ms INTEGER NOT NULL,
            reason TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS custom_roles (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            color TEXT NOT NULL DEFAULT '#5865F2',
            position INTEGER NOT NULL DEFAULT 0,
            allow INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS custom_role_members (
            role_id INTEGER NOT NULL REFERENCES custom_roles(id),
            user_id INTEGER NOT NULL REFERENCES users(id),
            PRIMARY KEY (role_id, user_id)
        );
        CREATE INDEX IF NOT EXISTS custom_role_members_user ON custom_role_members(user_id);
        CREATE TABLE IF NOT EXISTS categories (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            color TEXT NOT NULL DEFAULT '#5865F2',
            position INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS categories_position ON categories(position);",
    )?;
    crate::avatar::init(&tx)?;
    crate::profile::init(&tx)?;
    crate::attachments::init(&tx)?;
    if !has_column(&tx, "sessions", "auth_epoch")? {
        tx.execute_batch("ALTER TABLE sessions ADD COLUMN auth_epoch INTEGER NOT NULL DEFAULT 0")?;
        // Sessions of unprotected names prove nothing either way: keep them. Protected names log
        // in again (a session made by a rolled-back older hub would skip their password).
        let kept = tx.execute(
            "UPDATE sessions SET auth_epoch = ?1 WHERE user_id IN (SELECT id FROM users WHERE password_hash IS NULL AND reset_hash IS NULL)",
            [SESSION_EPOCH],
        )?;
        tracing::info!(kept, "sessions: auth epoch added");
    }
    if !has_column(&tx, "channels", "category_id")? {
        tx.execute_batch("ALTER TABLE channels ADD COLUMN category_id INTEGER REFERENCES categories(id)")?;
    }
    if version < SCHEMA_VERSION {
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        tracing::info!(from = version, to = SCHEMA_VERSION, "database migrated");
    }
    tx.commit()
}

// ---------------------------------------------------------------- users and identity

/// Everything the hub keeps about one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRow {
    pub id: UserId,
    pub username: String,
    /// The role stored in the database (never `Owner`: that comes from `config.toml`).
    pub stored_role: Role,
    /// argon2id PHC string.
    pub password_hash: Option<String>,
    /// sha256 of a one-time reset code, and when it stops working.
    pub reset_hash: Option<String>,
    pub reset_expires_ms: Option<i64>,
    pub server_muted: bool,
    pub created_at_ms: i64,
    /// Version of the profile picture, if there is one.
    pub avatar: Option<String>,
    /// What people see instead of the username, if the person chose one.
    pub display_name: Option<String>,
    /// Version of the profile banner image, if there is one.
    pub banner_version: Option<String>,
    /// The effective badge flags (automatic rules OR the admin's grants), materialized in
    /// the `users.badges` column by [`recompute_badges`] — the wire reads them from here.
    pub badges: u64,
}

/// `owner` is `config.toml`'s owner name (empty = none); it is compared like SQLite's NOCASE.
pub fn is_owner(username: &str, owner: &str) -> bool {
    !owner.is_empty() && username.eq_ignore_ascii_case(owner)
}

impl UserRow {
    /// The role everyone sees (the owner named in the config is `Owner`).
    pub fn role(&self, owner: &str) -> Role {
        if is_owner(&self.username, owner) {
            Role::Owner
        } else {
            self.stored_role.min(Role::Admin)
        }
    }

    pub fn has_password(&self) -> bool {
        self.password_hash.is_some()
    }

    /// Reset by the owner / the host and no new password chosen yet.
    pub fn reset_pending(&self) -> bool {
        self.password_hash.is_none() && self.reset_hash.is_some()
    }

    /// A personal password is needed but not set: the role's powers stay off until it is.
    pub fn must_set_password(&self, owner: &str) -> bool {
        !self.has_password() && (self.role(owner) > Role::Member || self.reset_pending())
    }

    /// The role used for permission checks.
    pub fn effective_role(&self, owner: &str) -> Role {
        if self.must_set_password(owner) { Role::Member } else { self.role(owner) }
    }

    pub fn user(&self, owner: &str) -> User {
        // v1.1: `role_ids` is filled by the callers that know the database (login, the
        // admin lists); a bare row keeps the default of none.
        User { id: self.id, username: self.username.clone(), role: self.role(owner), flags: self.badges, role_ids: Vec::new(), avatar: self.avatar.clone(), display_name: self.display_name.clone(), banner: self.banner_version.clone() }
    }

    pub fn account(&self, owner: &str) -> vox_proto::Account {
        vox_proto::Account { has_password: self.has_password(), must_set_password: self.must_set_password(owner), ..Default::default() }
    }
}

const USER_COLS: &str = "u.id, u.username, u.role, u.password_hash, u.reset_hash, u.reset_expires_ms, u.server_muted, u.created_at_ms, u.avatar_version, u.display_name, u.banner_version, u.badges";

fn user_row(r: &rusqlite::Row) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        id: r.get(0)?,
        username: r.get(1)?,
        stored_role: Role::from_wire(&r.get::<_, String>(2)?),
        password_hash: r.get(3)?,
        reset_hash: r.get(4)?,
        reset_expires_ms: r.get(5)?,
        server_muted: r.get::<_, i64>(6)? != 0,
        created_at_ms: r.get(7)?,
        avatar: r.get(8)?,
        display_name: r.get(9)?,
        banner_version: r.get(10)?,
        badges: r.get::<_, i64>(11)?.max(0) as u64,
    })
}

pub fn find_user(conn: &Connection, username: &str) -> rusqlite::Result<Option<UserRow>> {
    conn.query_row(&format!("SELECT {USER_COLS} FROM users u WHERE u.username = ?"), [username], user_row).optional()
}

pub fn get_user(conn: &Connection, id: UserId) -> rusqlite::Result<Option<UserRow>> {
    conn.query_row(&format!("SELECT {USER_COLS} FROM users u WHERE u.id = ?"), [id], user_row).optional()
}

pub fn list_users(conn: &Connection) -> rusqlite::Result<Vec<UserRow>> {
    let mut st = conn.prepare(&format!("SELECT {USER_COLS} FROM users u ORDER BY u.username COLLATE NOCASE"))?;
    let rows = st.query_map([], user_row)?;
    rows.collect()
}

/// Find or create the user, then open a session. Returns the user's row (canonical username).
pub fn login(conn: &mut Connection, username: &str, token_hash: &str) -> rusqlite::Result<UserRow> {
    login_inner(conn, username, token_hash, false)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// [`login`] for a name that must be new (an invite only creates accounts, v1.0.1): `None`, and no
/// session, when the name already has an account (somebody took it since the check).
pub fn login_new(conn: &mut Connection, username: &str, token_hash: &str) -> rusqlite::Result<Option<UserRow>> {
    login_inner(conn, username, token_hash, true)
}

fn login_inner(conn: &mut Connection, username: &str, token_hash: &str, only_new: bool) -> rusqlite::Result<Option<UserRow>> {
    let tx = conn.transaction()?;
    let now = now_ms();
    let created = tx.execute("INSERT OR IGNORE INTO users (username, created_at_ms) VALUES (?, ?)", params![username, now])? == 1;
    if only_new && !created {
        return Ok(None);
    }
    let user = tx.query_row(&format!("SELECT {USER_COLS} FROM users u WHERE u.username = ?"), [username], user_row)?;
    tx.execute(
        "INSERT INTO sessions (token_hash, user_id, created_at_ms, expires_at_ms, auth_epoch) VALUES (?, ?, ?, ?, ?)",
        params![token_hash, user.id, now, now + SESSION_TTL_MS, SESSION_EPOCH],
    )?;
    tx.commit()?;
    Ok(Some(user))
}

pub fn user_for_session(conn: &Connection, token_hash: &str) -> rusqlite::Result<Option<UserRow>> {
    conn.query_row(
        &format!("SELECT {USER_COLS} FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ? AND s.expires_at_ms > ? AND s.auth_epoch >= ?"),
        params![token_hash, now_ms(), SESSION_EPOCH],
        user_row,
    )
    .optional()
}

/// Remember a device for `user_id` (the token's sha256 is stored, never the token).
pub fn add_device_token(conn: &Connection, user_id: UserId, token_hash: &str) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO device_tokens (token_hash, user_id, created_at_ms, last_used_ms, expires_at_ms) VALUES (?, ?, ?, ?, ?)",
        params![token_hash, user_id, now, now, now + DEVICE_TOKEN_TTL_MS],
    )?;
    Ok(())
}

/// Is this a live device token of `user_id`? A valid one is refreshed (sliding expiry).
pub fn use_device_token(conn: &Connection, user_id: UserId, token_hash: &str) -> rusqlite::Result<bool> {
    let now = now_ms();
    let n = conn.execute(
        "UPDATE device_tokens SET last_used_ms = ?1, expires_at_ms = ?2
         WHERE token_hash = ?3 AND user_id = ?4 AND expires_at_ms > ?1",
        params![now, now + DEVICE_TOKEN_TTL_MS, token_hash, user_id],
    )?;
    Ok(n > 0)
}

/// New personal password (or none), clearing any reset code. Revokes every device token and
/// every session but `keep_session`.
pub fn set_password(conn: &mut Connection, user_id: UserId, hash: Option<&str>, keep_session: Option<&str>) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE users SET password_hash = ?, reset_hash = NULL, reset_expires_ms = NULL WHERE id = ?",
        params![hash, user_id],
    )?;
    tx.execute("DELETE FROM device_tokens WHERE user_id = ?", [user_id])?;
    tx.execute("DELETE FROM sessions WHERE user_id = ? AND token_hash IS NOT ?", params![user_id, keep_session])?;
    tx.commit()
}

/// "Redefinir senha pessoal": the password is dropped, the name is protected by a one-time code
/// instead, and every session and device token of the user ends.
pub fn set_reset_code(conn: &mut Connection, user_id: UserId, code_hash: &str, expires_ms: i64) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE users SET password_hash = NULL, reset_hash = ?, reset_expires_ms = ? WHERE id = ?",
        params![code_hash, expires_ms, user_id],
    )?;
    tx.execute("DELETE FROM device_tokens WHERE user_id = ?", [user_id])?;
    tx.execute("DELETE FROM sessions WHERE user_id = ?", [user_id])?;
    tx.commit()
}

// ---------------------------------------------------------------- the server's identity

const SERVER_ID_KEY: &str = "server_id";

/// This hub's identity (v1.0.1): 128 random bits as 32 hex digits, made once and kept in
/// `server_meta`, so it survives restarts, updates and address changes, and a copy of the data
/// folder is the same server. Apps keep it with what they saved for a server and check it
/// (`GET /api/health`) before sending a saved password or login token to an address. An older hub
/// ignores the row.
pub fn server_id(conn: &Connection) -> rusqlite::Result<String> {
    let fresh = crate::util::random_hex::<16>();
    conn.execute("INSERT OR IGNORE INTO server_meta (key, value) VALUES (?, ?)", params![SERVER_ID_KEY, fresh])?;
    let id: String = conn.query_row("SELECT value FROM server_meta WHERE key = ?", [SERVER_ID_KEY], |r| r.get(0))?;
    if vox_proto::is_server_id(&id) {
        return Ok(id);
    }
    // A hand-edited value: a fresh one (the apps then see another server, which is the truth).
    conn.execute("UPDATE server_meta SET value = ? WHERE key = ?", params![fresh, SERVER_ID_KEY])?;
    Ok(fresh)
}

// ---------------------------------------------------------------- the server's owner

const OWNER_KEY: &str = "owner_user_id";

/// The stored owner ("quem hospeda vira dono"), if there is one. Ignored when `config.toml` pins an owner.
pub fn owner_row(conn: &Connection) -> rusqlite::Result<Option<UserRow>> {
    conn.query_row(
        &format!("SELECT {USER_COLS} FROM server_meta m JOIN users u ON u.id = CAST(m.value AS INTEGER) WHERE m.key = ?"),
        [OWNER_KEY],
        user_row,
    )
    .optional()
}

/// Make `user_id` the owner, but only if there is none: of two simultaneous claims exactly one wins.
pub fn claim_owner(conn: &Connection, user_id: UserId) -> rusqlite::Result<bool> {
    let n = conn.execute("INSERT OR IGNORE INTO server_meta (key, value) VALUES (?, ?)", params![OWNER_KEY, user_id.to_string()])?;
    Ok(n == 1)
}

/// Hand the server from `from` (who must still be the owner) to `to`; `from` becomes Admin.
pub fn transfer_owner(conn: &mut Connection, from: UserId, to: UserId) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    let n = tx.execute("UPDATE server_meta SET value = ? WHERE key = ? AND value = ?", params![to.to_string(), OWNER_KEY, from.to_string()])?;
    if n == 1 {
        tx.execute("UPDATE users SET role = ? WHERE id = ?", params![Role::Admin.wire_name(), from])?;
    }
    tx.commit()?;
    Ok(n == 1)
}

/// `--reset-owner`: nobody is the owner any more. Returns whether there was one.
pub fn clear_owner(conn: &Connection) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM server_meta WHERE key = ?", [OWNER_KEY])? > 0)
}

pub fn set_role(conn: &Connection, user_id: UserId, role: Role) -> rusqlite::Result<()> {
    conn.execute("UPDATE users SET role = ? WHERE id = ?", params![role.min(Role::Admin).wire_name(), user_id])?;
    Ok(())
}

pub fn set_server_muted(conn: &Connection, user_id: UserId, muted: bool) -> rusqlite::Result<()> {
    conn.execute("UPDATE users SET server_muted = ? WHERE id = ?", params![muted as i64, user_id])?;
    Ok(())
}

pub fn server_muted_ids(conn: &Connection) -> rusqlite::Result<Vec<UserId>> {
    let mut st = conn.prepare("SELECT id FROM users WHERE server_muted != 0")?;
    let rows = st.query_map([], |r| r.get(0))?;
    rows.collect()
}

// ---------------------------------------------------------------- channels

/// Every channel (with its access list when private, v0.9), in order. `id > 0` skips the
/// downgrade guard row (`access.rs`), here and in every query on `channels`.
pub fn list_channels(conn: &Connection) -> rusqlite::Result<Vec<Channel>> {
    let mut st = conn.prepare("SELECT id, name, kind, position, category_id FROM channels WHERE id > 0 ORDER BY position, id")?;
    let rows = st.query_map([], |r| {
        let kind: String = r.get(2)?;
        Ok(Channel {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: if kind == "voice" { ChannelKind::Voice } else { ChannelKind::Text },
            position: r.get(3)?,
            private: None,
            category_id: r.get(4)?,
        })
    })?;
    let mut list = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::access::fill(conn, &mut list)?;
    Ok(list)
}

pub fn create_channel(conn: &Connection, name: &str, kind: ChannelKind) -> rusqlite::Result<Channel> {
    let pos: i32 = conn.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM channels WHERE id > 0", [], |r| r.get(0))?;
    let k = if kind == ChannelKind::Voice { "voice" } else { "text" };
    // A fresh id: a deleted channel's id never comes back (the apps keep per-channel state).
    let id = crate::ids::insert_with(conn, crate::ids::Seq::Channels, |id| {
        conn.execute("INSERT INTO channels (id, name, kind, position) VALUES (?, ?, ?, ?)", params![id, name, k, pos])?;
        Ok(id)
    })?;
    Ok(Channel { id, name: name.to_string(), kind, position: pos, private: None, category_id: None })
}

pub fn rename_channel(conn: &Connection, id: i64, name: &str) -> rusqlite::Result<bool> {
    Ok(conn.execute("UPDATE channels SET name = ? WHERE id = ?", params![name, id])? > 0)
}

/// Deletes the channel and its messages.
pub fn delete_channel(conn: &mut Connection, id: i64) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    crate::attachments::unlink_channel(&tx, id)?;
    tx.execute("DELETE FROM messages WHERE channel_id = ?", [id])?;
    let n = tx.execute("DELETE FROM channels WHERE id = ? AND id > 0", [id])?;
    // The last private channel gone: older hubs may run on the file again (`access.rs`).
    crate::access::sync_guard(&tx)?;
    tx.commit()?;
    Ok(n > 0)
}

/// `ids` in their new order (positions 0, 1, 2...).
pub fn reorder_channels(conn: &mut Connection, ids: &[i64]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for (pos, id) in ids.iter().enumerate() {
        tx.execute("UPDATE channels SET position = ? WHERE id = ?", params![pos as i64, id])?;
    }
    tx.commit()
}

// ---------------------------------------------------------------- messages

/// Store a channel message whose extras were already checked ([`crate::chat::resolve_channel`]).
fn insert_message(conn: &Connection, channel_id: i64, user: &User, content: &str, extras: crate::chat::Resolved) -> rusqlite::Result<Message> {
    let now = now_ms();
    // A fresh id: a deleted message's id never comes back (the apps' caches, reply quotes).
    let id = crate::ids::insert_with(conn, crate::ids::Seq::Messages, |id| {
        conn.execute(
            "INSERT INTO messages (id, channel_id, user_id, content, created_at_ms, mentions, mention_everyone, reply_to) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, channel_id, user.id, content, now, crate::chat::mentions_column(&extras.mentions), extras.everyone as i64, extras.reply.as_ref().map(|r| r.id)],
        )?;
        Ok(id)
    })?;
    Ok(Message {
        id,
        channel_id,
        author: user.clone(),
        content: content.to_string(),
        created_at_ms: now,
        attachments: Vec::new(),
        mentions: extras.mentions,
        mention_everyone: extras.everyone,
        reply: extras.reply,
        reactions: Vec::new(),
    })
}

/// A channel message with `attachments` (my pending uploads, checked here in the same
/// transaction) and the mentions / reply of `draft` (checked too, see
/// [`crate::chat::resolve_channel`]). `caption` is what was typed; the stored content gets the
/// `[imagem]` marker.
#[allow(clippy::too_many_arguments)]
pub fn insert_message_with(
    conn: &mut Connection,
    channel_id: i64,
    user: &User,
    caption: &str,
    attachments: &[vox_proto::AttachmentId],
    draft: &crate::chat::Draft,
    owner: &str,
) -> rusqlite::Result<Result<Message, crate::attachments::LinkError>> {
    let tx = conn.transaction()?;
    if let Err(e) = crate::attachments::check_pending(&tx, user.id, attachments)? {
        return Ok(Err(e));
    }
    let extras = crate::chat::resolve_channel(&tx, user.id, channel_id, caption, draft, owner)?;
    let content = vox_proto::with_attachment_marker(caption, attachments.len());
    let mut msg = insert_message(&tx, channel_id, user, &content, extras)?;
    msg.attachments = crate::attachments::link(&tx, attachments, crate::attachments::Owner::Channel(msg.id))?;
    tx.commit()?;
    Ok(Ok(msg))
}

/// Newest first; `before` is an exclusive message-id cursor. Authors carry their current role.
pub fn list_messages(conn: &Connection, channel_id: i64, before: Option<i64>, limit: i64, owner: &str) -> rusqlite::Result<Vec<Message>> {
    let mut st = conn.prepare(
        "SELECT m.id, m.content, m.created_at_ms, u.id, u.username, u.role, u.avatar_version, u.banner_version, u.display_name, u.badges,
                m.mentions, m.mention_everyone, m.reply_to FROM messages m
         JOIN users u ON u.id = m.user_id
         WHERE m.channel_id = ?1 AND m.id < ?2 ORDER BY m.id DESC LIMIT ?3",
    )?;
    let rows = st.query_map(params![channel_id, before.unwrap_or(i64::MAX), limit], |r| {
        let mut m = Message {
            id: r.get(0)?,
            channel_id,
            content: r.get(1)?,
            created_at_ms: r.get(2)?,
            author: display_user(r.get(3)?, r.get(4)?, &r.get::<_, String>(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get::<_, i64>(9)?.max(0) as u64, owner),
            attachments: Vec::new(),
            mentions: Vec::new(),
            mention_everyone: false,
            reply: None,
            reactions: Vec::new(),
        };
        crate::chat::raw_fields(&mut m, &r.get::<_, String>(10)?, r.get::<_, i64>(11)? != 0, r.get(12)?);
        Ok(m)
    })?;
    let mut msgs = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::attachments::fill_messages(conn, &mut msgs)?;
    crate::chat::fill_messages(conn, &mut msgs, owner)?;
    Ok(msgs)
}

pub(crate) fn display_user(id: UserId, username: String, stored_role: &str, avatar: Option<String>, banner_version: Option<String>, display_name: Option<String>, badges: u64, owner: &str) -> User {
    let role = if is_owner(&username, owner) { Role::Owner } else { Role::from_wire(stored_role).min(Role::Admin) };
    User { id, username, role, flags: badges, role_ids: Vec::new(), avatar, banner: banner_version, display_name }
}

// ---------------------------------------------------------------- badges (v1.1)

/// The "first members" award covers the first accounts ever created (ids are row order in
/// SQLite, so the rank is the creation order).
pub const FIRST_MEMBERS_COUNT: i64 = 100;
/// A tenure tier month, for the ladder: a 30-day month is close enough at this granularity.
const MONTH_MS: i64 = 30 * 24 * 3600 * 1000;

/// One user's manual grants — the half of the flags the admin sets by hand.
pub fn badge_grant(conn: &Connection, id: UserId) -> rusqlite::Result<u64> {
    Ok(conn.query_row("SELECT flags FROM badge_grants WHERE user_id = ?1", [id], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0).max(0) as u64)
}

/// Replaces one user's manual grants (0 leaves no row). The caller refreshes the
/// materialized `users.badges` right after (see [`refresh_badges`]).
pub fn set_badge_grant(conn: &Connection, id: UserId, flags: u64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO badge_grants (user_id, flags) VALUES (?1, ?2) ON CONFLICT(user_id) DO UPDATE SET flags = ?2",
        params![id, flags as i64],
    )?;
    Ok(())
}

/// The automatic badge bits: staff for the owner, the tenure ladder by account age, and
/// the "first members" award by creation rank (1 = the first account ever).
fn auto_badges(row: &UserRow, owner: &str, rank: i64) -> u64 {
    let mut flags = 0u64;
    if is_owner(&row.username, owner) {
        flags |= badge::STAFF;
    }
    let months = ((now_ms() - row.created_at_ms).max(0) / MONTH_MS) as u64;
    flags |= badge::tenure_bits(months);
    if rank <= FIRST_MEMBERS_COUNT {
        flags |= badge::FIRST_MEMBERS;
    }
    flags
}

/// Recomputes one user's materialized `users.badges` (manual grants OR the automatic
/// rules) and returns them. Called after a grant change and on login, so a tier that
/// crossed while the hub ran shows up without a restart.
pub fn refresh_badges(conn: &Connection, id: UserId, owner: &str) -> rusqlite::Result<u64> {
    let row: Option<UserRow> = conn.query_row(
        &format!("SELECT {USER_COLS} FROM users u WHERE u.id = ?1"),
        [id],
        user_row,
    ).optional()?;
    let Some(row) = row else { return Ok(0) };
    let rank = conn.query_row("SELECT COUNT(*) FROM users WHERE id < ?1", [id], |r| r.get::<_, i64>(0))?;
    let flags = badge_grant(conn, id)? | auto_badges(&row, owner, rank);
    if row.badges != flags {
        conn.execute("UPDATE users SET badges = ?1 WHERE id = ?2", params![flags as i64, id])?;
    }
    Ok(flags)
}

/// Recomputes everybody at once (startup). Returns how many users changed.
pub fn recompute_badges(conn: &Connection, owner: &str) -> rusqlite::Result<usize> {
    let mut st = conn.prepare(
        "SELECT u.id, u.username, u.created_at_ms, u.badges, COALESCE(g.flags, 0)
         FROM users u LEFT JOIN badge_grants g ON g.user_id = u.id",
    )?;
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, UserId>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
    })?;
    let mut changed = 0usize;
    for row in rows {
        let (id, username, created_at_ms, have, grant) = row?;
        let row = UserRow {
            id,
            username,
            stored_role: Role::Member,
            password_hash: None,
            reset_hash: None,
            reset_expires_ms: None,
            server_muted: false,
            created_at_ms,
            avatar: None,
            display_name: None,
            banner_version: None,
            badges: have.max(0) as u64,
        };
        let rank = conn.query_row("SELECT COUNT(*) FROM users WHERE id < ?1", [id], |r| r.get::<_, i64>(0))?;
        let flags = grant.max(0) as u64 | auto_badges(&row, owner, rank);
        if have != flags as i64 {
            conn.execute("UPDATE users SET badges = ?1 WHERE id = ?2", params![flags as i64, id])?;
            changed += 1;
        }
    }
    Ok(changed)
}

// ---------------------------------------------------------------- bans (v1.1)

/// One ban as the wire tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BanRow {
    pub user_id: UserId,
    pub banned_by: Option<UserId>,
    pub at_ms: i64,
    pub reason: String,
}

/// Whether the account is banned (with the ban itself, for the refusal message).
pub fn ban_of(conn: &Connection, id: UserId) -> rusqlite::Result<Option<BanRow>> {
    conn.query_row("SELECT user_id, banned_by, at_ms, reason FROM banned WHERE user_id = ?1", [id], |r| {
        Ok(BanRow {
            user_id: r.get(0)?,
            banned_by: r.get(1)?,
            at_ms: r.get(2)?,
            reason: r.get(3)?,
        })
    })
    .optional()
}

/// Bans the account: their sessions and remembered devices die with the ban, so no token
/// brings them back; the account row stays (badges, name, history) for an eventual unban.
pub fn ban(conn: &Connection, id: UserId, by: Option<UserId>, reason: &str) -> rusqlite::Result<()> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO banned (user_id, banned_by, at_ms, reason) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id) DO UPDATE SET banned_by = ?2, at_ms = ?3, reason = ?4",
        params![id, by, now, reason],
    )?;
    conn.execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
    conn.execute("DELETE FROM device_tokens WHERE user_id = ?1", [id])?;
    Ok(())
}

/// Removes the ban: the person can log in again (their old password still works, and
/// they still have to type the server password like anybody else).
pub fn unban(conn: &Connection, id: UserId) -> rusqlite::Result<bool> {
    Ok(conn.execute("DELETE FROM banned WHERE user_id = ?1", [id])? > 0)
}

// ---------------------------------------------------------------- custom roles (v1.1)

/// Every custom role, display order.
pub fn list_custom_roles(conn: &Connection) -> rusqlite::Result<Vec<CustomRole>> {
    let mut st = conn.prepare("SELECT id, name, color, position, allow FROM custom_roles ORDER BY position, id")?;
    let rows = st.query_map([], |r| {
        Ok(CustomRole {
            id: r.get(0)?,
            name: r.get(1)?,
            color: r.get(2)?,
            position: r.get(3)?,
            allow: r.get::<_, i64>(4)?.max(0) as u64,
        })
    })?;
    rows.collect()
}

/// Creates a role and returns it. The id comes from the hub's id mint (never reused).
pub fn create_custom_role(conn: &Connection, name: &str, color: &str, position: i64, allow: u64) -> rusqlite::Result<CustomRole> {
    crate::ids::insert_with(conn, crate::ids::Seq::CustomRoles, |id| {
        conn.execute(
            "INSERT INTO custom_roles (id, name, color, position, allow) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, name, color, position, allow as i64],
        )?;
        Ok(CustomRole { id, name: name.to_owned(), color: color.to_owned(), position, allow })
    })
}

/// Updates name, colour, order and permissions in one go (`false`: no such role).
pub fn update_custom_role(conn: &Connection, role: &CustomRole) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE custom_roles SET name = ?1, color = ?2, position = ?3, allow = ?4 WHERE id = ?5",
        params![role.name, role.color, role.position, role.allow as i64, role.id],
    )? > 0)
}

/// Deletes a role: the memberships follow it (the trigger below), the users keep going.
pub fn delete_custom_role(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.execute("DELETE FROM custom_role_members WHERE role_id = ?1", [id])?;
    Ok(conn.execute("DELETE FROM custom_roles WHERE id = ?1", [id])? > 0)
}

/// v1.1: every channel category, in display order.
pub fn list_categories(conn: &Connection) -> rusqlite::Result<Vec<ChannelCategory>> {
    let mut st = conn.prepare("SELECT id, name, color, position FROM categories ORDER BY position, id")?;
    let rows = st.query_map([], |r| {
        Ok(ChannelCategory {
            id: r.get(0)?,
            name: r.get(1)?,
            color: r.get(2)?,
            position: r.get(3)?,
        })
    })?;
    rows.collect()
}

/// v1.1: create a category. The id comes from the hub's id mint.
pub fn create_category(conn: &Connection, name: &str, color: &str, position: i64) -> rusqlite::Result<ChannelCategory> {
    crate::ids::insert_with(conn, crate::ids::Seq::Categories, |id| {
        conn.execute(
            "INSERT INTO categories (id, name, color, position) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, color, position],
        )?;
        Ok(ChannelCategory { id, name: name.to_owned(), color: color.to_owned(), position: position as i32 })
    })
}

/// v1.1: update name, colour and order (`false`: no such category).
pub fn update_category(conn: &Connection, cat: &ChannelCategory) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE categories SET name = ?1, color = ?2, position = ?3 WHERE id = ?4",
        params![cat.name, cat.color, cat.position, cat.id],
    )? > 0)
}

/// v1.1: delete a category; channels in it become standalone (`category_id` set to NULL).
pub fn delete_category(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.execute("UPDATE channels SET category_id = NULL WHERE category_id = ?1", [id])?;
    Ok(conn.execute("DELETE FROM categories WHERE id = ?1", [id])? > 0)
}

/// v1.1: put a channel in a category (`None`: standalone again). `false`: no such channel.
/// The caller checked that `category` exists when `Some`.
pub fn set_channel_category(conn: &Connection, channel: ChannelId, category: Option<i64>) -> rusqlite::Result<bool> {
    Ok(conn.execute("UPDATE channels SET category_id = ?1 WHERE id = ?2 AND id > 0", params![category, channel])? > 0)
}

/// One user's custom role ids, display order.
pub fn custom_role_ids(conn: &Connection, user: UserId) -> rusqlite::Result<Vec<i64>> {
    let mut st = conn.prepare(
        "SELECT m.role_id FROM custom_role_members m JOIN custom_roles r ON r.id = m.role_id
         WHERE m.user_id = ?1 ORDER BY r.position, r.id",
    )?;
    let rows = st.query_map([user], |r| r.get(0))?;
    rows.collect()
}

/// Replaces one user's assignments (empty clears them).
pub fn set_custom_role_ids(conn: &Connection, user: UserId, role_ids: &[i64]) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM custom_role_members WHERE user_id = ?1", [user])?;
    for &role in role_ids {
        conn.execute("INSERT OR IGNORE INTO custom_role_members (role_id, user_id) VALUES (?1, ?2)", params![role, user])?;
    }
    Ok(())
}

/// The union of the permissions of one user's custom roles.
pub fn custom_allow(conn: &Connection, user: UserId) -> rusqlite::Result<u64> {
    let allow: i64 = conn.query_row(
        "SELECT COALESCE(SUM(r.allow), 0) FROM custom_role_members m JOIN custom_roles r ON r.id = m.role_id WHERE m.user_id = ?1",
        [user],
        |r| Ok(r.get::<_, i64>(0)?),
    )?;
    Ok(allow.max(0) as u64)
}

/// (channel, author id, content) of a message.
pub fn get_message(conn: &Connection, id: i64) -> rusqlite::Result<Option<(i64, UserId, String)>> {
    conn.query_row("SELECT channel_id, user_id, content FROM messages WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()
}

/// Deletes the message and its attachment rows (the caller collects the unused files).
pub fn delete_message(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    crate::attachments::unlink_message(conn, id)?;
    Ok(conn.execute("DELETE FROM messages WHERE id = ?", [id])? > 0)
}

// ---------------------------------------------------------------- audit log

/// `actor`: `None` = the host on the command line.
pub fn audit(conn: &Connection, actor: Option<UserId>, action: &str, target: Option<UserId>, detail: &str) -> rusqlite::Result<()> {
    crate::ids::insert_with(conn, crate::ids::Seq::AuditLog, |id| {
        conn.execute(
            "INSERT INTO audit_log (id, at_ms, actor_id, action, target_id, detail) VALUES (?, ?, ?, ?, ?, ?)",
            params![id, now_ms(), actor, action, target, detail],
        )
    })?;
    Ok(())
}

/// Newest first; `before` is an exclusive id cursor.
pub fn list_audit(conn: &Connection, before: Option<i64>, limit: i64, owner: &str) -> rusqlite::Result<Vec<AuditEntry>> {
    let mut st = conn.prepare(
        "SELECT a.id, a.at_ms, a.action, a.detail, au.id, au.username, au.role, au.avatar_version, au.banner_version, au.display_name, au.badges, tu.id, tu.username, tu.role, tu.avatar_version, tu.banner_version, tu.display_name, tu.badges
         FROM audit_log a
         LEFT JOIN users au ON au.id = a.actor_id
         LEFT JOIN users tu ON tu.id = a.target_id
         WHERE a.id < ?1 ORDER BY a.id DESC LIMIT ?2",
    )?;
    let rows = st.query_map(params![before.unwrap_or(i64::MAX), limit], |r| {
        let person = |i: usize| -> rusqlite::Result<Option<User>> {
            let id: Option<i64> = r.get(i)?;
            Ok(match id {
                Some(id) => Some(display_user(id, r.get(i + 1)?, &r.get::<_, String>(i + 2)?, r.get(i + 3)?, r.get(i + 4)?, r.get(i + 5)?, r.get::<_, i64>(i + 6)?.max(0) as u64, owner)),
                None => None,
            })
        };
        Ok(AuditEntry { id: r.get(0)?, at_ms: r.get(1)?, action: r.get(2)?, detail: r.get(3)?, actor: person(4)?, target: person(11)? })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The v0.4.1 schema, exactly as `git show v0.4.1:hub/src/db.rs` creates it (plus the link
    /// preview table, unchanged since).
    fn v041_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE, created_at_ms INTEGER NOT NULL);
             CREATE TABLE sessions (token_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id), created_at_ms INTEGER NOT NULL, expires_at_ms INTEGER NOT NULL);
             CREATE TABLE channels (id INTEGER PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('text','voice')), position INTEGER NOT NULL);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, channel_id INTEGER NOT NULL REFERENCES channels(id), user_id INTEGER NOT NULL REFERENCES users(id), content TEXT NOT NULL, created_at_ms INTEGER NOT NULL);
             INSERT INTO users (username, created_at_ms) VALUES ('Ana', 1), ('dono', 2);
             INSERT INTO channels (name, kind, position) VALUES ('geral', 'text', 0), ('Sala 1', 'voice', 1);
             INSERT INTO messages (channel_id, user_id, content, created_at_ms) VALUES (1, 1, 'oi', 3);
             INSERT INTO sessions VALUES ('h', 1, 1, 99999999999999);",
        )
        .unwrap();
    }

    #[test]
    fn migration_is_additive_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxhub.db");
        v041_db(&path);
        for _ in 0..3 {
            let db = Db::open(&path).unwrap();
            db.call_now(|c| {
                assert_eq!(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?, SCHEMA_VERSION);
                let ana = find_user(c, "ana")?.unwrap();
                assert_eq!((ana.stored_role, ana.has_password(), ana.server_muted), (Role::Member, false, false));
                assert_eq!(ana.avatar, None, "no pictures in a v0.4.1 database, and the column exists");
                assert_eq!(c.query_row("SELECT COUNT(*) FROM avatars", [], |r| r.get::<_, i64>(0))?, 0);
                assert_eq!(ana.display_name, None, "no display names in a v0.4.1 database, and the column exists");
                assert_eq!(c.query_row("SELECT COUNT(*) FROM profiles", [], |r| r.get::<_, i64>(0))?, 0);
                assert_eq!(user_for_session(c, "h")?.unwrap().username, "Ana", "an unprotected name keeps its session");
                assert_eq!(list_messages(c, 1, None, 10, "dono")?.len(), 1);
                assert_eq!(list_messages(c, 1, None, 10, "dono")?[0].author, User::new(1, "Ana"));
                assert_eq!(find_user(c, "DONO")?.unwrap().role("dono"), Role::Owner);
                Ok(())
            })
            .unwrap();
        }
        // The v0.4.1 hub's own queries still work on the migrated file (downgrade safety).
        let conn = Connection::open(&path).unwrap();
        conn.execute("INSERT OR IGNORE INTO users (username, created_at_ms) VALUES ('Bia', 5)", []).unwrap();
        let (id, name): (i64, String) = conn.query_row("SELECT id, username FROM users WHERE username = 'bia'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!((id, name.as_str()), (3, "Bia"));
    }

    /// Rollback: a pre-v0.5 hub runs on the file for a while and logs somebody in as a protected
    /// name (it never asks for personal passwords). Back on this version, that session is refused,
    /// while sessions this hub made keep working.
    #[test]
    fn sessions_from_a_rolled_back_hub_do_not_survive_the_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxhub.db");
        let db = Db::open(&path).unwrap();
        let mine = db
            .call_now(|c| {
                let dono = login(c, "dono", "mine")?;
                set_password(c, dono.id, Some("$argon2id$fake"), Some("mine"))?;
                Ok(dono)
            })
            .unwrap();
        drop(db);
        // The older hub's own login SQL (git show v0.4.1:hub/src/db.rs), no password check.
        let conn = Connection::open(&path).unwrap();
        let now = now_ms();
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at_ms, expires_at_ms) VALUES (?, ?, ?, ?)",
            params!["legacy", mine.id, now, now + SESSION_TTL_MS],
        )
        .unwrap();
        drop(conn);
        let db = Db::open(&path).unwrap();
        db.call_now(|c| {
            assert!(user_for_session(c, "legacy")?.is_none(), "made by the older hub: refused");
            assert_eq!(user_for_session(c, "mine")?.unwrap().username, "dono");
            Ok(())
        })
        .unwrap();
    }

    /// The upgrade that adds the epoch: protected names log in again, the others keep going.
    #[test]
    fn adding_the_epoch_drops_only_sessions_of_protected_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxhub.db");
        v041_db(&path);
        // As a v0.6.1 file would be: users with passwords, sessions without the epoch column.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "ALTER TABLE users ADD COLUMN password_hash TEXT; ALTER TABLE users ADD COLUMN reset_hash TEXT;
             UPDATE users SET password_hash = '$argon2id$x' WHERE username = 'dono';
             INSERT INTO sessions VALUES ('d', 2, 1, 99999999999999);",
        )
        .unwrap();
        drop(conn);
        let db = Db::open(&path).unwrap();
        db.call_now(|c| {
            assert!(user_for_session(c, "h")?.is_some(), "Ana has no personal password: kept");
            assert!(user_for_session(c, "d")?.is_none(), "dono has one: logs in again");
            Ok(())
        })
        .unwrap();
    }

    // ------------------------------------------------------------ badges (v1.1)

    /// Inserts a user of a known age in days and returns their id.
    fn badge_user(c: &Connection, name: &str, age_days: i64) -> rusqlite::Result<UserId> {
        c.execute(
            "INSERT INTO users (username, created_at_ms) VALUES (?1, ?2)",
            params![name, now_ms() - age_days * 24 * 3600 * 1000],
        )?;
        Ok(c.last_insert_rowid() as UserId)
    }

    #[test]
    fn badges_start_empty_and_follow_manual_grants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxhub.db");
        let db = Db::open(&path).unwrap();
        db.call_now(|c| {
            let id = badge_user(c, "Ana", 0)?;
            assert_eq!(find_user(c, "Ana")?.unwrap().badges, 0, "a fresh account has no badges");
            set_badge_grant(c, id, badge::BUG_HUNTER | badge::HYPESQUAD)?;
            let flags = refresh_badges(c, id, "")?;
            assert_eq!(flags, badge::BUG_HUNTER | badge::HYPESQUAD | badge::FIRST_MEMBERS, "grants OR the first-members rule");
            // Clearing the grants leaves only the automatic half.
            set_badge_grant(c, id, 0)?;
            assert_eq!(refresh_badges(c, id, "")?, badge::FIRST_MEMBERS);
            // And the row's copy is what the wire reads.
            assert_eq!(find_user(c, "Ana")?.unwrap().user("").flags, badge::FIRST_MEMBERS);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_tenure_ladder_follows_the_account_age() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxhub.db");
        let db = Db::open(&path).unwrap();
        db.call_now(|c| {
            // 400 days ≈ 13 months: the 1, 3, 6 and 12 month tiers.
            let id = badge_user(c, "Veterana", 400)?;
            assert_eq!(refresh_badges(c, id, "")?, badge::FIRST_MEMBERS | badge::tenure_bits(13));
            assert_eq!(find_user(c, "Veterana")?.unwrap().badges & badge::TENURE_24M, 0, "24 months not yet");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_owner_is_staff_and_the_startup_pass_covers_everybody() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxhub.db");
        let db = Db::open(&path).unwrap();
        db.call_now(|c| {
            let owner_id = badge_user(c, "Akuma", 0)?;
            let other = badge_user(c, "Bia", 0)?;
            set_badge_grant(c, other, badge::PARTNER)?;
            // The startup pass materializes both (nothing was computed yet).
            assert_eq!(recompute_badges(c, "Akuma")?, 2, "both users changed");
            assert_eq!(find_user(c, "Akuma")?.unwrap().badges, badge::STAFF | badge::FIRST_MEMBERS);
            assert_eq!(find_user(c, "Bia")?.unwrap().badges, badge::PARTNER | badge::FIRST_MEMBERS);
            // And it is idempotent: nothing changes the second time.
            assert_eq!(recompute_badges(c, "Akuma")?, 0);
            let _ = owner_id;
            Ok(())
        })
        .unwrap();
    }

    // ------------------------------------------------------------ bans (v1.1)

    // ------------------------------------------------------------ bans (v1.1)

    #[test]
    fn a_ban_closes_the_door_and_an_unban_opens_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxhub.db");
        let db = Db::open(&path).unwrap();
        db.call_now(|c| {
            let id = badge_user(c, "Rebelde", 0)?;
            assert!(ban_of(c, id)?.is_none(), "nobody starts banned");
            ban(c, id, Some(42), " desrespeito ")?;
            let b = ban_of(c, id)?.unwrap();
            assert_eq!(b.banned_by, Some(42));
            assert_eq!(b.reason, " desrespeito ", "the reason is stored as given");
            // Their sessions and remembered devices die with the ban: no old token
            // brings them back.
            assert_eq!(c.query_row("SELECT COUNT(*) FROM sessions WHERE user_id = ?1", [id], |r| r.get::<_, i64>(0))?, 0);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM device_tokens WHERE user_id = ?1", [id], |r| r.get::<_, i64>(0))?, 0);
            // Ban again (an unban followed by a new ban): the row is replaced, not doubled.
            ban(c, id, None, "de novo")?;
            assert_eq!(c.query_row("SELECT COUNT(*) FROM banned", [], |r| r.get::<_, i64>(0))?, 1);
            // And the unban opens the door — once.
            assert!(unban(c, id)?);
            assert!(ban_of(c, id)?.is_none());
            assert!(!unban(c, id)?, "unbanning twice is a no-op, not an error");
            Ok(())
        })
        .unwrap();
    }

    // ------------------------------------------------------------ custom roles (v1.1)

    #[test]
    fn custom_roles_crud_and_assignments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voxhub.db");
        let db = Db::open(&path).unwrap();
        db.call_now(|c| {
            let owner = badge_user(c, "Akuma", 0)?;
            let member = badge_user(c, "Bia", 0)?;
            assert!(list_custom_roles(c)?.is_empty());
            // Create, assign, and the union of permissions follows.
            let vip = create_custom_role(c, "VIP", "#E91B62", 1, perm::BAN_MEMBERS | perm::VIEW_AUDIT)?;
            let modx = create_custom_role(c, "Modix", "#29A3A6", 2, perm::MUTE_MEMBERS)?;
            set_custom_role_ids(c, member, &[vip.id, modx.id])?;
            assert_eq!(custom_role_ids(c, member)?, [vip.id, modx.id], "display order");
            assert_eq!(custom_allow(c, member)?, perm::BAN_MEMBERS | perm::VIEW_AUDIT | perm::MUTE_MEMBERS);
            assert_eq!(custom_allow(c, owner)?, 0, "nobody else gets anything");
            // Replacing the set clears the old ones at once.
            set_custom_role_ids(c, member, &[modx.id])?;
            assert_eq!(custom_allow(c, member)?, perm::MUTE_MEMBERS);
            // Editing keeps the memberships, deleting drops them.
            let mut edited = modx.clone();
            edited.allow = perm::MOVE_MEMBERS;
            assert!(update_custom_role(c, &edited)?);
            assert_eq!(custom_allow(c, member)?, perm::MOVE_MEMBERS);
            assert!(delete_custom_role(c, modx.id)?);
            assert!(custom_role_ids(c, member)?.is_empty());
            assert!(!delete_custom_role(c, modx.id)?, "deleting twice is a no-op");
            Ok(())
        })
        .unwrap();
    }
}
