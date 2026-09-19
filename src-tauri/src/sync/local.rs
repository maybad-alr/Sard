//! This device's side of the exchange: read the local state out, and write a merged state back.
//!
//! Nothing here talks to a network, and nothing here decides anything — the merge rules live in
//! `super::merge`. This module is the only place that knows WHICH local rows make up a book's reading
//! state, and the only place that writes them back.
//!
//! # The per-book `settings` keys, and why they are named here
//!
//! The reader keeps three per-book values as `settings` rows rather than columns (the app's own
//! additive pattern, documented at `features/reader/Reader.tsx` and `furthestRead.ts`):
//!
//!   · `chapters_read:<bookId>` — grow-only set of spine sections read
//!   · `seen_start:<bookId>`    — grow-only set of sections whose beginning was seen
//!   · `furthest_read:<bookId>` — the furthest mark reached (cfi + display fields)
//!
//! They are named ONCE, here. Everything else in the sync path asks this module what a key means,
//! including the two write paths that stamp a book dirty, so there is no second list to fall out of
//! step with this one.
//!
//! Everything else in `settings` is deliberately NOT synced, including other per-book rows. Zoom,
//! inversion, PDF theme and the spoiler-safe switch describe how a document should LOOK on the device
//! holding it, and copying a phone's zoom onto a desktop is not continuity — it is one device
//! overwriting a choice the reader made on another. Reader-wide preferences have their own home
//! already (`profiles`, a file the reader sends deliberately).

use rusqlite::{Connection, OptionalExtension};

use super::doc::{BookState, Furthest, Progress, FORMAT};

/// The per-book `settings` key prefixes that make up a book's reading state. See the module note.
pub const PER_BOOK_KEYS: [&str; 3] = ["chapters_read:", "seen_start:", "furthest_read:"];

/// The book a per-book `settings` key belongs to, or `None` when the key is not reading state.
pub fn book_id_for_key(key: &str) -> Option<&str> {
    PER_BOOK_KEYS
        .iter()
        .find_map(|prefix| key.strip_prefix(prefix))
        .filter(|id| !id.is_empty())
}

/// Record that this book has local changes the account has not seen.
///
/// BEST-EFFORT, AND DELIBERATELY SO. It is called from `progress_save` and `settings::set`, which are
/// on the reader's hot path and must not fail because of sync bookkeeping: a connection opened before
/// the sync migration existed (a test's bare connection, an old file) has no `sync_state` table, and
/// on such a connection the only correct answer is "no dirty mark" — never an error that loses the
/// reader's position. Every failure is therefore swallowed here, and the absence of the mark costs at
/// most one unnecessary upload, because a book with no `remote_version` is pushed anyway.
pub fn touch(conn: &Connection, book_id: &str) {
    let _ = conn.execute(
        "INSERT INTO sync_state(book_id, dirty_at) VALUES(?1, unixepoch()) \
         ON CONFLICT(book_id) DO UPDATE SET dirty_at = excluded.dirty_at",
        [book_id],
    );
}

/// `touch`, for a `settings` write: a no-op unless the key is one of the per-book reading-state keys.
pub fn note_key_write(conn: &Connection, key: &str) {
    if let Some(book_id) = book_id_for_key(key) {
        touch(conn, book_id);
    }
}

/// Is there a `books` row for this id on this device?
///
/// The gate on writing a merged document back: `reading_progress` carries a foreign key to `books`,
/// so a state that arrives for a book this device has not imported can be held by the account but
/// cannot be written here. The caller reports it rather than dropping it — the state is still on the
/// server, and it applies the moment the book is imported.
pub fn book_exists(conn: &Connection, book_id: &str) -> rusqlite::Result<bool> {
    conn.query_row("SELECT 1 FROM books WHERE id = ?1", [book_id], |_| Ok(true)).optional().map(|o| o.is_some())
}

/// Read this device's state for one book, or `None` when it has none.
pub fn collect(conn: &Connection, book_id: &str) -> rusqlite::Result<Option<BookState>> {
    let progress = conn
        .query_row(
            "SELECT locator_cfi, fraction, updated_at FROM reading_progress WHERE book_id = ?1",
            [book_id],
            |r| {
                Ok(Progress {
                    cfi: r.get(0)?,
                    fraction: r.get(1)?,
                    // A row with no timestamp (written before the column was always set) sorts oldest
                    // rather than newest — the direction that cannot steal a position from a real write.
                    updated_at: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                })
            },
        )
        .optional()?;

    let state = BookState {
        format: FORMAT,
        progress,
        chapters_read: read_set(conn, &format!("chapters_read:{book_id}"))?,
        seen_start: read_set(conn, &format!("seen_start:{book_id}"))?,
        furthest: read_furthest(conn, &format!("furthest_read:{book_id}"))?,
    };
    Ok(if state.is_empty() { None } else { Some(state) })
}

/// A grow-only set as the reader stores it: a JSON array of section indices.
///
/// Tolerant in exactly the way the reader's own `parseSecs` is — a corrupt or legacy value means
/// "nothing recorded yet", never an error. This runs on the sync path for every book in the library,
/// where one hand-edited row must not be able to stop the others from syncing.
fn read_set(conn: &Connection, key: &str) -> rusqlite::Result<Vec<i64>> {
    let Some(raw) = crate::settings::get(conn, key)? else { return Ok(vec![]) };
    let mut out: Vec<i64> = serde_json::from_str::<Vec<serde_json::Value>>(&raw)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_i64())
        .filter(|n| *n >= 0)
        .collect();
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// The furthest mark, with the reader's own tolerance: a value with no usable cfi is no mark at all.
fn read_furthest(conn: &Connection, key: &str) -> rusqlite::Result<Option<Furthest>> {
    let Some(raw) = crate::settings::get(conn, key)? else { return Ok(None) };
    Ok(serde_json::from_str::<Furthest>(&raw).ok().filter(|f| !f.cfi.is_empty()))
}

/// Are these the same stored number?
///
/// Bit-for-bit, not `==`, because the value comes out of a SQLite REAL column and therefore out of a
/// file that can be hand-edited: a NaN row compared with `==` is unequal to ITSELF, which would make
/// `apply` rewrite an unchanged position on every pass and quietly break the no-op guarantee this
/// module is tested for. `to_bits` is a total comparison, so the only rows that read as changed are
/// rows that really are.
fn same_number(a: Option<f64>, b: Option<f64>) -> bool {
    a.map(f64::to_bits) == b.map(f64::to_bits)
}

/// Write a merged state back, touching only the parts that actually changed.
///
/// ONLY WHAT CHANGED, because every write here goes through `settings::set` / `progress_adopt` and
/// therefore reaches the reader's own tables: rewriting an identical value would churn the file and,
/// worse, make an untouched book look locally modified on the next pass. A no-op sync must leave the
/// database byte-identical, which is what the tests assert.
///
/// The caller is responsible for the book existing locally (`book_exists`): `reading_progress` has a
/// foreign key and this function does not swallow that error.
pub fn apply(conn: &Connection, book_id: &str, state: &BookState) -> Result<bool, String> {
    let mut changed = false;

    let held = collect(conn, book_id).map_err(|e| e.to_string())?;

    if let Some(incoming) = state.progress.as_ref() {
        let same = held
            .as_ref()
            .and_then(|h| h.progress.as_ref())
            .is_some_and(|h| h.cfi == incoming.cfi && same_number(h.fraction, incoming.fraction));
        if !same {
            crate::library::progress_adopt(
                conn,
                book_id,
                incoming.cfi.as_deref(),
                incoming.fraction,
                incoming.updated_at,
            )
            .map_err(|e| e.to_string())?;
            changed = true;
        }
    }

    for (prefix, incoming) in [
        ("chapters_read:", &state.chapters_read),
        ("seen_start:", &state.seen_start),
    ] {
        if incoming.is_empty() {
            continue;
        }
        let key = format!("{prefix}{book_id}");
        let current = read_set(conn, &key).map_err(|e| e.to_string())?;
        if current != *incoming {
            crate::settings::set(conn, &key, &serde_json::to_string(incoming).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            changed = true;
        }
    }

    if let Some(incoming) = state.furthest.as_ref() {
        let key = format!("furthest_read:{book_id}");
        let current = read_furthest(conn, &key).map_err(|e| e.to_string())?;
        if current.as_ref() != Some(incoming) {
            crate::settings::set(conn, &key, &serde_json::to_string(incoming).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            changed = true;
        }
    }

    Ok(changed)
}

/// Books this device has something to SEND, or a write since the last pass.
///
/// A book that is already in step is NOT here, and that matters: the reading tables are joined in only
/// to find books that have never been pushed, so a library that has synced stays out of the way of its
/// own next pass. Books with a state row but no version are included, which is what makes the first
/// push happen for a book whose local state was written before sync existed.
///
/// This is only HALF of what a pass must visit — the other half is books the ACCOUNT has state for,
/// which is the account's answer to give, not this function's (see `sync_all`).
///
/// Sorted, so the order a pass visits books in is a property of the library rather than of the query
/// planner — which is what lets the tests assert on a whole pass.
pub fn pending_local(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT books_with_state.book_id FROM ( \
             SELECT book_id FROM reading_progress \
             UNION SELECT substr(key, length('chapters_read:') + 1) FROM settings WHERE key LIKE 'chapters_read:%' \
             UNION SELECT substr(key, length('seen_start:') + 1) FROM settings WHERE key LIKE 'seen_start:%' \
             UNION SELECT substr(key, length('furthest_read:') + 1) FROM settings WHERE key LIKE 'furthest_read:%' \
             UNION SELECT book_id FROM sync_state \
         ) AS books_with_state \
         LEFT JOIN sync_state s ON s.book_id = books_with_state.book_id \
         WHERE s.remote_version IS NULL OR s.dirty_at IS NOT NULL \
         ORDER BY books_with_state.book_id",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// The version the account held when this device last agreed with it, or `None` when it never has.
pub fn remote_version(conn: &Connection, book_id: &str) -> rusqlite::Result<Option<u64>> {
    conn.query_row("SELECT remote_version FROM sync_state WHERE book_id = ?1", [book_id], |r| {
        r.get::<_, Option<i64>>(0)
    })
    .optional()
    .map(|o| o.flatten().map(|v| v as u64))
}

/// Record that this book is in step with the account at `version`, clearing the dirty mark.
///
/// Called at the END of a pass, after any write to the reader's own tables — including the writes
/// `apply` makes, which stamp the book dirty on their way through `settings::set`. That order is the
/// point: the mark exists to say "the account has not seen this yet", and by the time this runs it has.
pub fn mark_synced(conn: &Connection, book_id: &str, version: Option<u64>) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_state(book_id, remote_version, synced_at, dirty_at) VALUES(?1, ?2, unixepoch(), NULL) \
         ON CONFLICT(book_id) DO UPDATE SET \
            remote_version = COALESCE(excluded.remote_version, sync_state.remote_version), \
            synced_at      = excluded.synced_at, \
            dirty_at       = NULL",
        rusqlite::params![book_id, version.map(|v| v as i64)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    const BOOK: &str = "book-sha";

    /// A database on the real migration path with one imported book — the same shape a reader's file
    /// has, so these tests exercise the schema that ships rather than a fixture.
    fn db(tag: &str) -> Connection {
        let dir = std::env::temp_dir().join(format!("sard_sync_local_{tag}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.db");
        let _ = std::fs::remove_file(&path);
        let conn = crate::db::open_database(&path).unwrap();
        migrations::run(&conn, None).unwrap();
        conn.execute("INSERT INTO books(id, file_path) VALUES(?1, ?2)", rusqlite::params![BOOK, "x.epub"])
            .unwrap();
        conn
    }

    /// The key map is the single source of truth for what counts as reading state — including the
    /// per-book rows that are deliberately NOT state.
    #[test]
    fn only_the_three_reading_state_keys_name_a_book() {
        assert_eq!(book_id_for_key("chapters_read:abc"), Some("abc"));
        assert_eq!(book_id_for_key("seen_start:abc"), Some("abc"));
        assert_eq!(book_id_for_key("furthest_read:abc"), Some("abc"));

        assert_eq!(book_id_for_key("chapters_read:"), None, "no book, nothing to sync");
        assert_eq!(book_id_for_key("tts_position:abc"), None, "where the voice stopped is not a position");
        assert_eq!(book_id_for_key("pdf.zoom.abc"), None, "how a document looks belongs to the device");
        assert_eq!(book_id_for_key("book_style:abc"), None);
        assert_eq!(book_id_for_key("book_theme_id"), None, "a reader-wide preference is not a book's");
    }

    /// A book nobody has read has no state, and therefore nothing to send.
    #[test]
    fn a_book_with_no_reading_state_collects_to_nothing() {
        let conn = db("empty");
        assert_eq!(collect(&conn, BOOK).unwrap(), None);
        assert_eq!(pending_local(&conn).unwrap(), Vec::<String>::new());
    }

    /// What the reader wrote is what travels, including the display fields of the furthest mark.
    #[test]
    fn collect_reads_progress_and_the_three_per_book_keys() {
        let conn = db("collect");
        crate::library::progress_save(&conn, BOOK, "/6/4!", 0.4).unwrap();
        crate::settings::set(&conn, &format!("chapters_read:{BOOK}"), "[3,1,2]").unwrap();
        crate::settings::set(&conn, &format!("seen_start:{BOOK}"), "[1]").unwrap();
        crate::settings::set(
            &conn,
            &format!("furthest_read:{BOOK}"),
            r#"{"cfi":"/6/9!","fraction":0.9,"label":"التاسع","href":"t.xhtml","sec":8}"#,
        )
        .unwrap();

        let state = collect(&conn, BOOK).unwrap().expect("state");
        assert_eq!(state.progress.unwrap().cfi.as_deref(), Some("/6/4!"));
        assert_eq!(state.chapters_read, vec![1, 2, 3], "sorted as it is stored");
        assert_eq!(state.seen_start, vec![1]);
        let f = state.furthest.unwrap();
        assert_eq!((f.cfi.as_str(), f.fraction, f.sec), ("/6/9!", 0.9, 8));
        assert_eq!(f.label.as_deref(), Some("التاسع"));
    }

    /// One corrupt row cannot stop a library from syncing, and a corrupt value is never carried.
    #[test]
    fn a_corrupt_value_reads_as_nothing_rather_than_failing() {
        let conn = db("corrupt");
        crate::settings::set(&conn, &format!("chapters_read:{BOOK}"), "{not json").unwrap();
        crate::settings::set(&conn, &format!("furthest_read:{BOOK}"), r#"{"fraction":0.5}"#).unwrap();

        let state = collect(&conn, BOOK).unwrap();
        // The furthest row has no cfi, so it is not a mark; the set row is unreadable, so it is empty.
        assert_eq!(state, None);
    }

    /// Applying is monotone in the ways that matter: it writes what changed, and touches nothing when
    /// there is nothing to change.
    #[test]
    fn apply_writes_once_and_then_leaves_the_database_alone() {
        let conn = db("apply");
        let incoming = BookState {
            format: FORMAT,
            progress: Some(Progress { cfi: Some("/6/4!".into()), fraction: Some(0.4), updated_at: 111 }),
            chapters_read: vec![1, 2],
            seen_start: vec![],
            furthest: Some(Furthest {
                cfi: "/6/4!".into(),
                fraction: 0.4,
                label: None,
                href: None,
                sec: 3,
            }),
        };

        assert!(apply(&conn, BOOK, &incoming).unwrap(), "the first apply writes");
        assert!(!apply(&conn, BOOK, &incoming).unwrap(), "the second has nothing to do");

        let state = collect(&conn, BOOK).unwrap().unwrap();
        assert_eq!(state.progress.unwrap().updated_at, 111, "the REMOTE timestamp is preserved");
        assert_eq!(state.chapters_read, vec![1, 2]);
    }

    /// The dirty mark: stamped by the two write paths, and cleared only by a completed pass.
    #[test]
    fn the_write_paths_stamp_a_book_dirty_and_a_pass_clears_it() {
        let conn = db("dirty");
        assert_eq!(pending_local(&conn).unwrap(), Vec::<String>::new(), "nothing to sync yet");

        crate::library::progress_save(&conn, BOOK, "/6/2!", 0.2).unwrap();
        assert_eq!(pending_local(&conn).unwrap(), vec![BOOK.to_string()], "a position is state to send");

        mark_synced(&conn, BOOK, Some(7)).unwrap();
        assert_eq!(pending_local(&conn).unwrap(), Vec::<String>::new(), "in step, nothing to send");
        assert_eq!(remote_version(&conn, BOOK).unwrap(), Some(7));

        // A per-book reading-state key stamps too; the device's own settings do not.
        crate::settings::set(&conn, &format!("seen_start:{BOOK}"), "[4]").unwrap();
        assert_eq!(pending_local(&conn).unwrap(), vec![BOOK.to_string()]);
        mark_synced(&conn, BOOK, Some(8)).unwrap();
        crate::settings::set(&conn, "immersive_scroll", "1").unwrap();
        crate::settings::set(&conn, &format!("pdf.zoom.{BOOK}"), "fit-page").unwrap();
        assert_eq!(pending_local(&conn).unwrap(), Vec::<String>::new(), "device preferences never travel");
    }

    /// A book the account knows and this device has never seen is pending until it is in step.
    #[test]
    fn a_book_never_pushed_is_pending_even_with_no_local_writes() {
        let conn = db("never");
        conn.execute("INSERT INTO sync_state(book_id) VALUES(?1)", [BOOK]).unwrap();
        assert_eq!(pending_local(&conn).unwrap(), vec![BOOK.to_string()]);
    }

    /// The recorded version is the PULL trigger: it is what a later pass compares against the
    /// account's index to notice that another device has moved a book since this one last agreed.
    #[test]
    fn the_agreed_version_is_what_a_later_pass_compares_against() {
        let conn = db("version");
        assert_eq!(remote_version(&conn, BOOK).unwrap(), None, "never synced");

        mark_synced(&conn, BOOK, Some(4)).unwrap();
        assert_eq!(remote_version(&conn, BOOK).unwrap(), Some(4));
        assert_ne!(remote_version(&conn, BOOK).unwrap(), Some(5), "a moved version is a book to fetch");

        mark_synced(&conn, BOOK, Some(5)).unwrap();
        assert_eq!(remote_version(&conn, BOOK).unwrap(), Some(5), "and adopting it settles the book");
    }

    /// THE BEST-EFFORT GUARANTEE. `progress_save` and `settings::set` run on the reader's hot path
    /// against connections that may predate the sync tables (every unit test that opens a bare
    /// connection, and any file that never ran the migration). Bookkeeping that cannot be written
    /// must cost nothing and break nothing.
    #[test]
    fn a_connection_without_the_sync_tables_can_still_save_reading_state() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);").unwrap();
        // No `sync_state`, no `reading_progress`, no `books`: exactly the bare-connection case.
        crate::settings::set(&conn, &format!("chapters_read:{BOOK}"), "[1,2]").unwrap();
        crate::settings::set(&conn, "immersive_scroll", "1").unwrap();
        assert_eq!(
            crate::settings::get(&conn, &format!("chapters_read:{BOOK}")).unwrap().as_deref(),
            Some("[1,2]"),
            "the reader's own write must survive the missing bookkeeping table"
        );
    }
}
