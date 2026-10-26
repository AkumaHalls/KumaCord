//! Ids that are never handed out twice (v0.8.2).
//!
//! The tables use `id INTEGER PRIMARY KEY` without `AUTOINCREMENT`, so SQLite gives a new row
//! `MAX(id) + 1`: once the newest message (channel, image, sound...) is deleted, the next one gets
//! its id again. Everything that remembers an id (the apps' caches of thumbnails, messages, unread
//! counters and reply quotes, a DM's read pointer) then mixes the old thing up with the new one: a
//! new photo showed the deleted photo's thumbnail.
//!
//! SQLite cannot add `AUTOINCREMENT` to an existing table without rebuilding it (which an older
//! hub would then have to cope with). Instead, one small additive table keeps each table's
//! high-water mark, and every insert names its id explicitly: `max(high water, MAX(id)) + 1`, with
//! the mark raised in the same transaction (a savepoint, so it nests inside the caller's).
//!
//! Compatibility: an older hub (v0.8.1 and before) never reads `id_high_water` and keeps inserting
//! with SQLite's own `MAX(id) + 1`. While it runs it may reuse an id (as it always did), but it
//! cannot collide with this hub: the next id here is above both the mark and every existing row.
//! The marks are raised to the tables' current maximum at every start, so rows an older hub wrote
//! meanwhile are covered too.

use rusqlite::{params, Connection, OptionalExtension};

/// The tables whose ids reach the apps (users are never deleted, so theirs are never reused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seq {
    Channels,
    Messages,
    Attachments,
    DmConversations,
    DmMessages,
    Sounds,
    Invites,
    AuditLog,
}

impl Seq {
    pub const ALL: [Seq; 8] = [Seq::Channels, Seq::Messages, Seq::Attachments, Seq::DmConversations, Seq::DmMessages, Seq::Sounds, Seq::Invites, Seq::AuditLog];

    pub fn table(self) -> &'static str {
        match self {
            Seq::Channels => "channels",
            Seq::Messages => "messages",
            Seq::Attachments => "attachments",
            Seq::DmConversations => "dm_conversations",
            Seq::DmMessages => "dm_messages",
            Seq::Sounds => "sounds",
            Seq::Invites => "invites",
            Seq::AuditLog => "audit_log",
        }
    }
}

/// Create the table (additive, `IF NOT EXISTS`) and raise every mark to its table's current
/// maximum. Run once every table exists.
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    ensure_table(conn)?;
    for seq in Seq::ALL {
        let exists = conn.query_row("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?", [seq.table()], |_| Ok(())).optional()?.is_some();
        if exists {
            let max = max_id(conn, seq)?;
            raise(conn, seq, max)?;
        }
    }
    Ok(())
}

fn ensure_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS id_high_water (
            tbl TEXT PRIMARY KEY,
            high INTEGER NOT NULL
        );",
    )
}

fn max_id(conn: &Connection, seq: Seq) -> rusqlite::Result<i64> {
    conn.query_row(&format!("SELECT COALESCE(MAX(id), 0) FROM {}", seq.table()), [], |r| r.get(0))
}

fn raise(conn: &Connection, seq: Seq, to: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO id_high_water (tbl, high) VALUES (?1, ?2) ON CONFLICT (tbl) DO UPDATE SET high = MAX(high, excluded.high)",
        params![seq.table(), to],
    )?;
    Ok(())
}

/// The highest id this table has ever handed out, as far as this hub knows.
pub fn high_water(conn: &Connection, seq: Seq) -> rusqlite::Result<i64> {
    Ok(conn.query_row("SELECT high FROM id_high_water WHERE tbl = ?", [seq.table()], |r| r.get::<_, i64>(0)).optional()?.unwrap_or(0))
}

/// Reserve a never-used id of `seq` and run `insert` with it, atomically: the mark is raised in
/// the same savepoint as the insert (if `insert` fails, both are undone). Works inside a
/// transaction or on its own.
pub fn insert_with<T>(conn: &Connection, seq: Seq, insert: impl FnOnce(i64) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    conn.execute_batch("SAVEPOINT fresh_id")?;
    let result = (|| {
        // (Always there after `init`; this keeps partial schemas, like unit tests', working.)
        ensure_table(conn)?;
        let id = high_water(conn, seq)?.max(max_id(conn, seq)?) + 1;
        raise(conn, seq, id)?;
        insert(id)
    })();
    match result {
        Ok(v) => {
            conn.execute_batch("RELEASE fresh_id")?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO fresh_id; RELEASE fresh_id");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE channels (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE messages (id INTEGER PRIMARY KEY, content TEXT);
             CREATE TABLE attachments (id INTEGER PRIMARY KEY);
             CREATE TABLE dm_conversations (id INTEGER PRIMARY KEY);
             CREATE TABLE dm_messages (id INTEGER PRIMARY KEY);
             CREATE TABLE sounds (id INTEGER PRIMARY KEY);
             CREATE TABLE invites (id INTEGER PRIMARY KEY);
             CREATE TABLE audit_log (id INTEGER PRIMARY KEY);
             INSERT INTO messages (content) VALUES ('a'), ('b'), ('c');",
        )
        .unwrap();
        init(&c).unwrap();
        c
    }

    fn add(c: &Connection, text: &str) -> i64 {
        insert_with(c, Seq::Messages, |id| {
            c.execute("INSERT INTO messages (id, content) VALUES (?, ?)", params![id, text])?;
            Ok(id)
        })
        .unwrap()
    }

    #[test]
    fn the_newest_id_is_not_handed_out_again_after_a_delete() {
        let c = mem();
        assert_eq!(high_water(&c, Seq::Messages).unwrap(), 3, "seeded from the existing rows");
        assert_eq!(add(&c, "d"), 4);
        c.execute("DELETE FROM messages WHERE id = 4", []).unwrap();
        assert_eq!(add(&c, "e"), 5, "SQLite alone would give 4 again");
        // Everything deleted: still counting up.
        c.execute("DELETE FROM messages", []).unwrap();
        assert_eq!(add(&c, "f"), 6);
        // Other tables have their own marks.
        assert_eq!(high_water(&c, Seq::Channels).unwrap(), 0);
    }

    #[test]
    fn rows_an_older_hub_wrote_are_respected() {
        let c = mem();
        add(&c, "d"); // 4
        c.execute("DELETE FROM messages WHERE id = 4", []).unwrap();
        // An older hub's own SQL (no id): SQLite reuses 4, and then 5.
        c.execute("INSERT INTO messages (content) VALUES ('old 1')", []).unwrap();
        c.execute("INSERT INTO messages (content) VALUES ('old 2')", []).unwrap();
        assert_eq!(c.query_row("SELECT MAX(id) FROM messages", [], |r| r.get::<_, i64>(0)).unwrap(), 5);
        assert_eq!(add(&c, "new"), 6, "never below a row that exists");
        init(&c).unwrap();
        assert_eq!(high_water(&c, Seq::Messages).unwrap(), 6, "a restart keeps (or raises) the mark");
    }

    #[test]
    fn a_failed_insert_undoes_its_reservation_and_nests_in_transactions() {
        let mut c = mem();
        let r: rusqlite::Result<i64> = insert_with(&c, Seq::Messages, |id| {
            c.execute("INSERT INTO messages (id, content) VALUES (?, ?)", params![id, "x"])?;
            Err(rusqlite::Error::QueryReturnedNoRows)
        });
        assert!(r.is_err());
        assert_eq!(high_water(&c, Seq::Messages).unwrap(), 3);
        assert_eq!(c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0)).unwrap(), 3);
        // Inside an outer transaction that rolls back: nothing stays.
        let tx = c.transaction().unwrap();
        let id = insert_with(&tx, Seq::Messages, |id| {
            tx.execute("INSERT INTO messages (id, content) VALUES (?, ?)", params![id, "y"])?;
            Ok(id)
        })
        .unwrap();
        assert_eq!(id, 4);
        drop(tx);
        assert_eq!(high_water(&c, Seq::Messages).unwrap(), 3);
        // And one that commits keeps both.
        let tx = c.transaction().unwrap();
        insert_with(&tx, Seq::Messages, |id| tx.execute("INSERT INTO messages (id, content) VALUES (?, ?)", params![id, "z"])).unwrap();
        tx.commit().unwrap();
        assert_eq!(high_water(&c, Seq::Messages).unwrap(), 4);
    }
}
