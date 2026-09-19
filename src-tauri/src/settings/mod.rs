//! Settings — key/value persistence backed by the `settings` table (RAWY-08).

use rusqlite::{Connection, OptionalExtension};

/// Read a setting value, or `None` if the key is absent.
pub fn get(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
        .optional()
}

/// Insert or update a setting value (upsert).
///
/// Also the one place that notices a PER-BOOK READING-STATE key changing (`chapters_read:<id>` and the
/// two beside it — the list lives in `sync::local`, not here). Every write to `settings` goes through
/// this function, so marking the book as having something the account has not seen needs no trigger
/// and no second call site. It is a no-op for every other key, and best-effort by design: sync
/// bookkeeping must never fail the reader's own write.
pub fn set(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES(?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    crate::sync::local::note_key_write(conn, key);
    Ok(())
}
