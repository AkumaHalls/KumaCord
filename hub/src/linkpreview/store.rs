//! SQLite cache of link previews: metadata plus the re-encoded thumbnail.

use rusqlite::{params, Connection, OptionalExtension};

/// More rows than a 15-person server will ever need; the oldest go first.
const MAX_ROWS: i64 = 3000;
/// Rows older than this are useless even as a stale fallback.
const KEEP_MS: i64 = 7 * 24 * 3600 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub url: String,
    pub fetched_at_ms: i64,
    pub title: String,
    pub description: String,
    pub site_name: String,
    pub image: Option<Vec<u8>>,
    pub image_w: u32,
    pub image_h: u32,
}

impl Row {
    /// A fetch that found nothing worth a card (blocked, failed, or just the domain).
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.description.is_empty() && self.image.is_none()
    }
}

pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS link_previews (
            url TEXT PRIMARY KEY,
            fetched_at_ms INTEGER NOT NULL,
            title TEXT NOT NULL,
            description TEXT NOT NULL,
            site_name TEXT NOT NULL,
            image BLOB,
            image_w INTEGER NOT NULL DEFAULT 0,
            image_h INTEGER NOT NULL DEFAULT 0
        );",
    )
}

pub fn get(conn: &Connection, url: &str) -> rusqlite::Result<Option<Row>> {
    conn.query_row(
        "SELECT url, fetched_at_ms, title, description, site_name, image, image_w, image_h
         FROM link_previews WHERE url = ?",
        [url],
        |r| {
            Ok(Row {
                url: r.get(0)?,
                fetched_at_ms: r.get(1)?,
                title: r.get(2)?,
                description: r.get(3)?,
                site_name: r.get(4)?,
                image: r.get(5)?,
                image_w: r.get(6)?,
                image_h: r.get(7)?,
            })
        },
    )
    .optional()
}

pub fn put(conn: &Connection, row: &Row, now_ms: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO link_previews
         (url, fetched_at_ms, title, description, site_name, image, image_w, image_h)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![row.url, row.fetched_at_ms, row.title, row.description, row.site_name, row.image, row.image_w, row.image_h],
    )?;
    conn.execute("DELETE FROM link_previews WHERE fetched_at_ms < ?", [now_ms - KEEP_MS])?;
    conn.execute(
        "DELETE FROM link_previews WHERE url NOT IN
         (SELECT url FROM link_previews ORDER BY fetched_at_ms DESC LIMIT ?)",
        [MAX_ROWS],
    )?;
    Ok(())
}

pub fn count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM link_previews", [], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(url: &str, at: i64) -> Row {
        Row {
            url: url.into(),
            fetched_at_ms: at,
            title: "t".into(),
            description: String::new(),
            site_name: "s".into(),
            image: Some(vec![1, 2, 3]),
            image_w: 4,
            image_h: 5,
        }
    }

    #[test]
    fn roundtrip_replace_and_prune() {
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let now = 10 * KEEP_MS;
        assert_eq!(get(&conn, "https://a/").unwrap(), None);
        put(&conn, &row("https://a/", now), now).unwrap();
        assert_eq!(get(&conn, "https://a/").unwrap().unwrap(), row("https://a/", now));
        let mut r = row("https://a/", now + 1);
        r.title = "new".into();
        put(&conn, &r, now + 1).unwrap();
        assert_eq!(count(&conn).unwrap(), 1);
        assert_eq!(get(&conn, "https://a/").unwrap().unwrap().title, "new");
        // Old rows go when something is stored later.
        put(&conn, &row("https://old/", now - KEEP_MS - 5), now).unwrap();
        assert_eq!(get(&conn, "https://old/").unwrap(), None);
        // The table is capped.
        for i in 0..(MAX_ROWS + 20) {
            put(&conn, &row(&format!("https://x/{i}"), now + 10 + i), now + 10 + i).unwrap();
        }
        assert_eq!(count(&conn).unwrap(), MAX_ROWS);
        assert!(get(&conn, "https://x/0").unwrap().is_none());
        assert!(get(&conn, &format!("https://x/{}", MAX_ROWS + 19)).unwrap().is_some());
    }
}
