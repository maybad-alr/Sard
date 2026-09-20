//! TWO DEVICES, ONE ACCOUNT — the acceptance tests for the whole feature, on real databases.
//!
//! Each device is a real SQLite file on the real migration path, holding the books it was given. They
//! share one in-memory account (`mock`). Every assertion is made on what a READER would see (the
//! position the book opens at, the chapters marked read, the furthest mark) rather than on the shape
//! of the document that carried it — so these tests cannot pass by agreeing with the implementation's
//! internals.
//!
//! What is NOT covered here, stated plainly: the network itself, and two devices genuinely syncing at
//! the same instant (the race is simulated by the account refusing one store, which is the same thing
//! as seen from this side). Both need the account backend, and neither can be honest before it exists.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use rusqlite::Connection;

use super::doc::{BookState, Furthest, Progress, FORMAT};
use super::local;
use super::{sync_all, sync_one, Outcome, RemoteDoc, SyncBackend, SyncError};
use crate::db::migrations;

/// THE ACCOUNT, WITH NO NETWORK BEHIND IT — the whole reason the merge rules can be tested on one
/// machine, against two real databases, before any account exists.
///
/// It is deliberately the SIMPLEST thing that satisfies the trait's contract: a map, a version
/// counter, and a refusal to store anything that does not carry the version the caller read. A real
/// backend's job is exactly this plus transport, so a merge that converges here converges there.
///
/// It lives in this file rather than beside the engine because it is test infrastructure and nothing
/// else: `scripts/production-tree-rules.mjs` excludes test modules from the published tree, and a
/// helper the product never compiles belongs on that side of the line with the tests that use it.
#[derive(Default)]
struct MockBackend {
    inner: Mutex<Inner>,
    counters: Counters,
    /// Set by `lose_next_race`: the next store is refused as if another device had written first.
    conflict_next_store: AtomicBool,
}

#[derive(Default)]
struct Inner {
    docs: HashMap<String, (BookState, u64)>,
    next_version: u64,
}

/// How many times the account was READ and WRITTEN — so a test can prove that a second pass over a
/// synced library does no work, rather than merely producing the same result.
#[derive(Default)]
struct Counters {
    fetches: AtomicUsize,
    stores: AtomicUsize,
}

impl MockBackend {
    fn new() -> Self {
        Self::default()
    }

    /// Put a document on the account as if another device had pushed it.
    fn seed(&self, book_id: &str, doc: BookState) {
        let mut inner = self.inner.lock().unwrap();
        inner.next_version += 1;
        let version = inner.next_version;
        inner.docs.insert(book_id.to_string(), (doc, version));
    }

    /// What the account currently holds for a book.
    fn doc(&self, book_id: &str) -> Option<BookState> {
        self.inner.lock().unwrap().docs.get(book_id).map(|(d, _)| d.clone())
    }

    fn version(&self, book_id: &str) -> Option<u64> {
        self.inner.lock().unwrap().docs.get(book_id).map(|(_, v)| *v)
    }

    /// Every book id on the account, sorted — for assertions, not part of the trait.
    fn book_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.inner.lock().unwrap().docs.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Make the NEXT store lose a race it cannot see: the account moves between the caller's fetch and
    /// its store, which is what two devices syncing in the same second looks like from the inside.
    fn lose_next_race(&self) {
        self.conflict_next_store.store(true, Ordering::SeqCst);
    }
}

impl SyncBackend for MockBackend {
    fn fetch(&self, book_id: &str) -> Result<Option<RemoteDoc>, SyncError> {
        self.counters.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(self
            .inner
            .lock()
            .unwrap()
            .docs
            .get(book_id)
            .map(|(doc, version)| RemoteDoc { doc: doc.clone(), version: *version }))
    }

    fn store(&self, book_id: &str, doc: &BookState, expected: Option<u64>) -> Result<u64, SyncError> {
        let mut inner = self.inner.lock().unwrap();

        // The contract, and nothing else: a write must carry the version it read.
        let held = inner.docs.get(book_id).map(|(_, v)| *v);
        let raced = self.conflict_next_store.swap(false, Ordering::SeqCst);
        if raced || held != expected {
            return Err(SyncError::Conflict);
        }

        inner.next_version += 1;
        let version = inner.next_version;
        inner.docs.insert(book_id.to_string(), (doc.clone(), version));
        self.counters.stores.fetch_add(1, Ordering::SeqCst);
        Ok(version)
    }

    fn versions(&self) -> Result<Vec<(String, u64)>, SyncError> {
        // Sorted like a real backend's index, so a report built from it is deterministic.
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<(String, u64)> = inner.docs.iter().map(|(id, (_, v))| (id.clone(), *v)).collect();
        out.sort();
        Ok(out)
    }
}

const BOOK: &str = "sha-of-the-file";
const OTHER: &str = "sha-of-another-file";

/// One device's library.
struct Device {
    conn: Connection,
}

impl Device {
    /// A real database, migrated the way the app migrates it, holding `books`.
    fn new(tag: &str, books: &[&str]) -> Self {
        let dir = std::env::temp_dir().join(format!("sard_sync_dev_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conn = crate::db::open_database(&dir.join("sard.db")).unwrap();
        migrations::run(&conn, None).unwrap();
        for id in books {
            conn.execute(
                "INSERT INTO books(id, file_path) VALUES(?1, ?2)",
                rusqlite::params![id, format!("{id}.epub")],
            )
            .unwrap();
        }
        Self { conn }
    }

    /// The reader turned to a page: the app's own path, which stamps the real clock.
    fn read_to(&self, id: &str, cfi: &str, fraction: f64) {
        crate::library::progress_save(&self.conn, id, cfi, fraction).unwrap();
    }

    /// A position with a CONTROLLED timestamp — how a test says "this move came first" without
    /// waiting a second for the clock.
    fn adopt_at(&self, id: &str, at: i64, cfi: &str, fraction: f64) {
        crate::library::progress_adopt(&self.conn, id, Some(cfi), Some(fraction), at).unwrap();
    }

    /// Where the book would open, and how far in.
    fn position(&self, id: &str) -> Option<(Option<String>, f64)> {
        crate::library::progress_get(&self.conn, id).unwrap().map(|p| (p.cfi, p.fraction))
    }

    /// The timestamp that travelled with the position — the value the NEXT merge will compare.
    fn position_stamp(&self, id: &str) -> Option<i64> {
        local::collect(&self.conn, id).unwrap().and_then(|s| s.progress).map(|p| p.updated_at)
    }

    fn mark_read(&self, id: &str, sections: &[i64]) {
        crate::settings::set(&self.conn, &format!("chapters_read:{id}"), &serde_json::to_string(sections).unwrap())
            .unwrap();
    }

    fn chapters_read(&self, id: &str) -> Vec<i64> {
        local::collect(&self.conn, id).unwrap().map(|s| s.chapters_read).unwrap_or_default()
    }

    fn set_furthest(&self, id: &str, cfi: &str, fraction: f64, label: &str) {
        let mark = Furthest {
            cfi: cfi.into(),
            fraction,
            label: Some(label.into()),
            href: Some("c.xhtml".into()),
            sec: 3,
        };
        crate::settings::set(&self.conn, &format!("furthest_read:{id}"), &serde_json::to_string(&mark).unwrap())
            .unwrap();
    }

    fn furthest(&self, id: &str) -> Option<Furthest> {
        local::collect(&self.conn, id).unwrap().and_then(|s| s.furthest)
    }

    /// Mark a passage the way the reader's own code does.
    fn highlight(&self, cfi: &str, excerpt: &str) {
        crate::library::highlight_create(&self.conn, BOOK, cfi, "yellow", Some(excerpt), None).unwrap();
    }

    fn highlights(&self) -> Vec<String> {
        let mut stmt = self.conn.prepare("SELECT text_excerpt FROM highlights WHERE book_id = ?1").unwrap();
        let rows = stmt.query_map([BOOK], |r| r.get::<_, Option<String>>(0)).unwrap();
        rows.map(|r| r.unwrap().unwrap_or_default()).collect()
    }

    fn forget_highlight(&self, cfi: &str) {
        let id: String = self
            .conn
            .query_row(
                "SELECT id FROM highlights WHERE book_id = ?1 AND start_cfi = ?2",
                rusqlite::params![BOOK, cfi],
                |r| r.get(0),
            )
            .unwrap();
        crate::library::highlight_delete(&self.conn, &id).unwrap();
    }
}

/// THE HEADLINE CASE: read on one device, carry on at the same place on the other.
#[test]
fn a_position_read_on_one_device_is_where_the_other_opens() {
    let phone = Device::new("basic_phone", &[BOOK]);
    let desktop = Device::new("basic_desktop", &[BOOK]);
    let account = MockBackend::new();

    phone.read_to(BOOK, "/6/4!/4/2", 0.42);
    let pushed = sync_all(&phone.conn, &account).unwrap();
    assert_eq!(pushed.books, vec![(BOOK.to_string(), Outcome::Pushed)]);

    let pulled = sync_all(&desktop.conn, &account).unwrap();
    assert_eq!(pulled.books, vec![(BOOK.to_string(), Outcome::Pulled)]);
    assert_eq!(
        desktop.position(BOOK),
        Some((Some("/6/4!/4/2".to_string()), 0.42)),
        "the desktop opens where the phone stopped"
    );
}

/// Reading on both devices without syncing in between: the chapters add up, they are not chosen
/// between. This is the case a naive "newest wins" would quietly lose.
#[test]
fn chapters_read_on_both_devices_add_up() {
    let phone = Device::new("chapters_phone", &[BOOK]);
    let desktop = Device::new("chapters_desktop", &[BOOK]);
    let account = MockBackend::new();

    phone.mark_read(BOOK, &[1, 2, 3]);
    desktop.mark_read(BOOK, &[5, 6, 7]);

    sync_all(&phone.conn, &account).unwrap();
    sync_all(&desktop.conn, &account).unwrap();
    // And back again, so both hold the union rather than each holding its own half.
    sync_all(&phone.conn, &account).unwrap();

    assert_eq!(phone.chapters_read(BOOK), vec![1, 2, 3, 5, 6, 7]);
    assert_eq!(desktop.chapters_read(BOOK), vec![1, 2, 3, 5, 6, 7]);
}

/// A DELIBERATE REWIND IS NOT OVERRULED. The reader went back to re-read on the newer device; the
/// device that happens to be further in is older, and must not win.
#[test]
fn the_newer_move_wins_even_when_it_is_earlier_in_the_book() {
    let desktop = Device::new("rewind_desktop", &[BOOK]);
    let phone = Device::new("rewind_phone", &[BOOK]);
    let account = MockBackend::new();

    desktop.adopt_at(BOOK, 1_000, "/6/40!", 0.90);
    phone.adopt_at(BOOK, 2_000, "/6/2!", 0.10);

    sync_all(&desktop.conn, &account).unwrap();
    sync_all(&phone.conn, &account).unwrap();
    sync_all(&desktop.conn, &account).unwrap();

    assert_eq!(desktop.position(BOOK).map(|(_, f)| f), Some(0.10), "the newer move is the position");
    assert_eq!(desktop.position_stamp(BOOK), Some(2_000), "and it keeps its own timestamp");
    assert_eq!(phone.position(BOOK).map(|(_, f)| f), Some(0.10));
}

/// The furthest mark and the current position are different facts, and a rewind moves only one of
/// them: "take me back to where I got to" must survive going back to re-read.
#[test]
fn a_rewind_moves_the_position_but_not_the_furthest_mark() {
    let phone = Device::new("furthest_phone", &[BOOK]);
    let other = Device::new("furthest_other", &[BOOK]);
    let account = MockBackend::new();

    phone.set_furthest(BOOK, "/6/40!", 0.90, "الفصل الأربعون");
    phone.adopt_at(BOOK, 2_000, "/6/2!", 0.10);

    sync_all(&phone.conn, &account).unwrap();
    sync_all(&other.conn, &account).unwrap();

    assert_eq!(other.position(BOOK).map(|(_, f)| f), Some(0.10), "the position followed the rewind");
    let mark = other.furthest(BOOK).expect("the mark travelled too");
    assert_eq!(mark.fraction, 0.90, "and the furthest mark did not go backwards with it");
    assert_eq!(mark.label.as_deref(), Some("الفصل الأربعون"), "with the label the reader saw");
}

/// State for a book this device has not imported is REPORTED, never written and never lost — and the
/// moment the book arrives, it applies.
#[test]
fn state_for_a_book_this_device_does_not_have_waits_for_the_book() {
    let phone = Device::new("unmatched_phone", &[BOOK]);
    let desktop = Device::new("unmatched_desktop", &[]);
    let account = MockBackend::new();

    phone.read_to(BOOK, "/6/4!", 0.4);
    sync_all(&phone.conn, &account).unwrap();

    let report = sync_all(&desktop.conn, &account).unwrap();
    assert_eq!(report.books, Vec::new(), "the desktop has no state of its own to exchange");
    assert_eq!(report.unmatched, vec![BOOK.to_string()], "and it says so rather than inventing a book");
    assert_eq!(local::remote_version(&desktop.conn, BOOK).unwrap(), None, "nothing was written");

    // The reader imports the book on the desktop.
    desktop
        .conn
        .execute("INSERT INTO books(id, file_path) VALUES(?1, ?2)", rusqlite::params![BOOK, "b.epub"])
        .unwrap();
    let report = sync_all(&desktop.conn, &account).unwrap();
    assert_eq!(report.books, vec![(BOOK.to_string(), Outcome::Pulled)]);
    assert_eq!(desktop.position(BOOK), Some((Some("/6/4!".to_string()), 0.4)), "the state was waiting");
}

/// Once both sides agree, a pass does NOTHING: no document is rewritten, no version churns, and the
/// reading tables are untouched. A sync that "succeeds" by rewriting everything every launch would
/// pass a weaker test than this one.
#[test]
fn a_second_pass_over_a_synced_library_does_no_work() {
    let phone = Device::new("quiet_phone", &[BOOK]);
    let desktop = Device::new("quiet_desktop", &[BOOK]);
    let account = MockBackend::new();

    phone.read_to(BOOK, "/6/4!", 0.4);
    phone.mark_read(BOOK, &[1, 2]);
    sync_all(&phone.conn, &account).unwrap();
    sync_all(&desktop.conn, &account).unwrap();

    let stores_before = account.counters.stores.load(std::sync::atomic::Ordering::SeqCst);
    let version_before = account.version(BOOK);

    let phone_again = sync_all(&phone.conn, &account).unwrap();
    let desktop_again = sync_all(&desktop.conn, &account).unwrap();

    assert_eq!(phone_again.books, Vec::new(), "a synced device has nothing pending");
    assert_eq!(desktop_again.books, Vec::new());
    assert_eq!(
        account.counters.stores.load(std::sync::atomic::Ordering::SeqCst),
        stores_before,
        "no document was written again"
    );
    assert_eq!(account.version(BOOK), version_before, "and no version churned");
}

/// A library of two books: each is exchanged on its own account, in a deterministic order, and one
/// book's state never becomes another's.
#[test]
fn books_are_exchanged_independently() {
    let device = Device::new("two_books", &[BOOK, OTHER]);
    let account = MockBackend::new();

    device.read_to(BOOK, "/6/4!", 0.4);
    device.read_to(OTHER, "/9/2!", 0.2);

    let report = sync_all(&device.conn, &account).unwrap();
    assert_eq!(
        report.books,
        vec![(OTHER.to_string(), Outcome::Pushed), (BOOK.to_string(), Outcome::Pushed)],
        "one outcome per book, in book order"
    );
    assert_eq!(report.pushed(), 2);
    assert_eq!(account.doc(OTHER).unwrap().progress.unwrap().cfi.as_deref(), Some("/9/2!"));
}

/// A book the reader has never opened puts NOTHING on the account. Sync must not turn an untouched
/// library into a record of it.
#[test]
fn an_unread_book_creates_no_account_state() {
    let device = Device::new("unread", &[BOOK, OTHER]);
    let account = MockBackend::new();

    let report = sync_all(&device.conn, &account).unwrap();
    assert_eq!(report.books, Vec::new());
    assert_eq!(account.book_ids(), Vec::<String>::new(), "the account knows nothing about this reader");
}

/// A DOCUMENT FROM A NEWER SARD IS LEFT ALONE. Merging it would drop fields this build does not know
/// about, so the account's copy is not overwritten and the local one is not touched.
#[test]
fn a_newer_document_is_never_merged_or_overwritten() {
    let device = Device::new("newer_format", &[BOOK]);
    let account = MockBackend::new();

    device.read_to(BOOK, "/6/4!", 0.4);
    let from_the_future = BookState {
        format: FORMAT + 1,
        progress: Some(Progress { cfi: Some("/6/9!".into()), fraction: Some(0.9), updated_at: 99_999 }),
        chapters_read: vec![9],
        ..BookState::default()
    };
    account.seed(BOOK, from_the_future.clone());

    let outcome = sync_one(&device.conn, &account, BOOK).unwrap();
    assert_eq!(outcome, Outcome::SkippedFormat);
    assert_eq!(device.position(BOOK).map(|(_, f)| f), Some(0.4), "the local position is untouched");
    assert_eq!(account.doc(BOOK), Some(from_the_future), "and the account's newer document survives");
}

/// TWO DEVICES IN THE SAME SECOND. The account refuses the store that lost the race; the answer is to
/// read again and merge again — and the retry must keep what each device had, not trade one for the
/// other.
#[test]
fn a_lost_race_is_retried_and_nothing_is_lost_in_the_retry() {
    let phone = Device::new("race_phone", &[BOOK]);
    let desktop = Device::new("race_desktop", &[BOOK]);
    let account = MockBackend::new();

    // The desktop is at a newer position; the phone is behind on position but has a chapter the
    // desktop lacks, so the phone genuinely has something to write.
    desktop.adopt_at(BOOK, 2_000, "/6/6!", 0.60);
    phone.adopt_at(BOOK, 1_000, "/6/4!", 0.40);
    phone.mark_read(BOOK, &[4]);

    sync_all(&desktop.conn, &account).unwrap();
    account.lose_next_race();
    let outcome = sync_one(&phone.conn, &account, BOOK).unwrap();
    assert_eq!(outcome, Outcome::Both, "the retry landed on top of the desktop's write");

    // The phone adopted the newer position AND the desktop gained the chapter — a retry that simply
    // overwrote would lose one of the two.
    assert_eq!(phone.position(BOOK).map(|(_, f)| f), Some(0.60), "the newer position came down");
    let account_doc = account.doc(BOOK).expect("the account holds the merged state");
    assert_eq!(account_doc.progress.unwrap().fraction, Some(0.60));
    assert_eq!(account_doc.chapters_read, vec![4], "and the chapter went up");

    sync_all(&desktop.conn, &account).unwrap();
    assert_eq!(desktop.chapters_read(BOOK), vec![4], "both devices agree once the dust settles");
    assert_eq!(desktop.position(BOOK).map(|(_, f)| f), Some(0.60));
}

/// THE HEADLINE CASE OF STAGE 2: a highlight made on one device arrives on the other, and a deletion
/// made there keeps it deleted — which is the whole reason the tombstone table exists.
#[test]
fn a_highlight_travels_and_its_deletion_sticks() {
    let phone = Device::new("marks_phone", &[BOOK]);
    let desktop = Device::new("marks_desktop", &[BOOK]);
    let account = MockBackend::new();

    phone.highlight("/6/4!", "المقتبس");
    sync_all(&phone.conn, &account).unwrap();
    sync_all(&desktop.conn, &account).unwrap();
    assert_eq!(desktop.highlights(), vec!["المقتبس".to_string()], "the mark arrived");

    // The reader deletes it on the desktop — the row goes, and only the tombstone remembers it.
    desktop.forget_highlight("/6/4!");
    assert!(desktop.highlights().is_empty(), "the row is gone from the device that deleted it");
    sync_all(&desktop.conn, &account).unwrap();
    sync_all(&phone.conn, &account).unwrap();

    assert!(
        phone.highlights().is_empty(),
        "the deletion travelled — without the tombstone the phone's copy would have come back"
    );

    // A third pass in the other direction does not resurrect it either: both devices hold the
    // deletion now, and it keeps travelling.
    sync_all(&phone.conn, &account).unwrap();
    sync_all(&desktop.conn, &account).unwrap();
    assert!(phone.highlights().is_empty());
    assert!(desktop.highlights().is_empty());
}

/// A note edited on one device wins over the copy that has not been touched, because a note is the one
/// mark whose edit carries a fresh timestamp.
#[test]
fn an_edited_note_wins_over_the_untouched_copy() {
    let phone = Device::new("note_phone", &[BOOK]);
    let desktop = Device::new("note_desktop", &[BOOK]);
    let account = MockBackend::new();

    phone
        .conn
        .execute(
            "INSERT INTO notes(id, book_id, locator_cfi, body, created_at, updated_at) \
             VALUES('n1', ?1, '/6/4!', 'ملاحظة', 100, 100)",
            [BOOK],
        )
        .unwrap();
    sync_all(&phone.conn, &account).unwrap();
    sync_all(&desktop.conn, &account).unwrap();

    desktop
        .conn
        .execute("UPDATE notes SET body = 'ملاحظة معدَّلة', updated_at = 200 WHERE id = 'n1'", [])
        .unwrap();
    crate::sync::local::touch(&desktop.conn, BOOK);
    sync_all(&desktop.conn, &account).unwrap();
    sync_all(&phone.conn, &account).unwrap();

    let body: String = phone.conn.query_row("SELECT body FROM notes WHERE id = 'n1'", [], |r| r.get(0)).unwrap();
    assert_eq!(body, "ملاحظة معدَّلة", "the later edit is what both devices hold");
}
