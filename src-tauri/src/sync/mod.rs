//! Reading-state sync — the `SyncBackend` seam this module was reserved for, filled in far enough to
//! be tested without a network.
//!
//! This file was a two-line placeholder ("FUTURE seam only. Will define a `SyncBackend` trait so
//! optional cloud sync can be added without reshaping the core", RAWY-05). The trait below is exactly
//! that seam; what the placeholder promised is what is here.
//!
//! # What is deliberately NOT in this stage
//!
//! Stage 1 moves the state that answers "continue where I stopped": the reading position, the two
//! grow-only section sets, and the furthest mark. Three things follow, and none of them is hidden:
//!
//!   · **Annotations** (highlights, notes, bookmarks). They need two things this stage does not have:
//!     a per-row merge (their edits are not grow-only) and DELETION tombstones, because the tables
//!     here delete rows outright and a deletion that is not recorded comes back on the next pull.
//!     Both belong with the tables they describe, in the stage that ships them.
//!   · **The network.** No HTTP client is added here. The backend is a trait with an in-memory
//!     implementation used by the tests, which is what lets the merge rules be exercised against two
//!     real databases on this machine before any account exists. The account backend is a thin
//!     implementation of that trait and nothing else.
//!   · **The interface.** Signing in, a sync-now action and a status line are frontend work, and they
//!     land with the account backend so the reader is never shown a switch that cannot work.
//!
//! # The exchange, and why it is safe to stop it halfway
//!
//! One book at a time: read the local state → fetch the account's → MERGE (the rules are in
//! `merge.rs`) → store the result if the account is behind → write it back locally if this device is
//! behind → mark the book in step. The push happens BEFORE the local write, so an interruption can
//! only ever leave the account ahead of this device — never this device holding a merged state that
//! it pushed nowhere. The next pass pulls it and applies it, which is the same path a second device
//! takes, so the recovery path is the normal path rather than a repair.

pub mod account;
pub mod doc;
pub mod http;
pub mod local;
pub mod merge;
pub mod supabase;

#[cfg(test)]
mod account_tests;
#[cfg(test)]
mod supabase_tests;
#[cfg(test)]
mod tests;

use serde::Serialize;

use doc::BookState;

/// How many times one book's exchange is retried when the account moved underneath it.
///
/// A conflict is not an error: it means another device pushed this book between our fetch and our
/// store, so the answer is to fetch again and re-merge. Two devices sync in the same second by
/// accident, not by design, so three attempts is beyond generous — and a persistent conflict means
/// something else is wrong (a backend that cannot count versions) and must be reported, not looped on.
const MAX_ATTEMPTS: usize = 3;

/// The account's storage, whatever it turns out to be.
///
/// `expected` is the version the caller read: a store that does not carry that version is refused with
/// `SyncError::Conflict`, so two devices can never silently overwrite each other. This is the whole
/// concurrency contract between the two sides, and it is why the server needs no merge logic of its
/// own — the merge happens on the device, where the rules are.
pub trait SyncBackend {
    /// The account's document for this book, or `None` when it has never seen it.
    fn fetch(&self, book_id: &str) -> Result<Option<RemoteDoc>, SyncError>;
    /// Store `doc` as the new state, if the account still holds version `expected`
    /// (`None` = "this book must not exist yet"). Returns the version just written.
    fn store(&self, book_id: &str, doc: &BookState, expected: Option<u64>) -> Result<u64, SyncError>;
    /// Every book the account holds state for, WITH the version it currently holds.
    ///
    /// The version is not decoration: it is the only way a device learns that the account moved
    /// underneath it. Without it, a device that agreed at version 3 would consider the book done
    /// forever and never see version 4 — the pull side of sync would simply stop working after the
    /// first pass, which is exactly the defect this signature exists to make impossible. One call
    /// returns the whole index, so this stays a single round trip however large the library is.
    fn versions(&self) -> Result<Vec<(String, u64)>, SyncError>;
}

/// A document plus the version it was read at.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteDoc {
    pub doc: BookState,
    pub version: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    /// The account holds a different version than the caller expected. Fetch and merge again.
    Conflict,
    /// Anything else: unreachable, refused, malformed. Never silently swallowed by the caller.
    Transport(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Conflict => write!(f, "sync.conflict"),
            SyncError::Transport(msg) => write!(f, "sync.transport: {msg}"),
        }
    }
}

/// What one book's exchange did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Both sides already held the same state. Nothing was written anywhere.
    InSync,
    /// This device's state went to the account.
    Pushed,
    /// The account's state came to this device.
    Pulled,
    /// Each side had something the other lacked, and both ended up with the merged state.
    Both,
    /// The account holds state for a book this device has not imported. Nothing was written here, and
    /// nothing was lost: the state is still on the account and applies once the book is imported.
    Unmatched,
    /// The document is newer than this build understands. Left completely alone — see `doc::FORMAT`.
    SkippedFormat,
    /// The account kept moving under us. Nothing was written.
    Conflict,
}

impl Outcome {
    pub fn pushed(self) -> bool {
        matches!(self, Outcome::Pushed | Outcome::Both)
    }
    pub fn pulled(self) -> bool {
        matches!(self, Outcome::Pulled | Outcome::Both)
    }
}

/// What a whole pass did — the shape the interface will read, and what the tests assert on.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SyncReport {
    /// Per book, in the order the pass visited them (`local::pending` sorts).
    pub books: Vec<(String, Outcome)>,
    /// Books the account has state for that this device does not have in its library.
    pub unmatched: Vec<String>,
}

impl SyncReport {
    pub fn count(&self, want: impl Fn(Outcome) -> bool) -> usize {
        self.books.iter().filter(|(_, o)| want(*o)).count()
    }
    pub fn pushed(&self) -> usize {
        self.count(Outcome::pushed)
    }
    pub fn pulled(&self) -> usize {
        self.count(Outcome::pulled)
    }
}

/// Exchange ONE book with the account. See the module note for the order and why it is safe to stop.
pub fn sync_one(conn: &rusqlite::Connection, backend: &dyn SyncBackend, book_id: &str) -> Result<Outcome, String> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let local = local::collect(conn, book_id).map_err(|e| e.to_string())?;
        let remote = backend.fetch(book_id).map_err(|e| e.to_string())?;

        // A document from a newer Sard is not merged, not applied and not overwritten: this build
        // would drop fields it does not know about on the way back out (see `doc::FORMAT`).
        if remote.as_ref().is_some_and(|r| r.doc.format > doc::FORMAT) {
            return Ok(Outcome::SkippedFormat);
        }

        let merged = merge::merge(local.as_ref(), remote.as_ref().map(|r| &r.doc));

        // Nothing on either side: no document to create, and no state row to keep. A reader who has
        // not opened the book must not appear on the account at all.
        if merged.is_empty() && remote.is_none() {
            return Ok(Outcome::InSync);
        }

        let mut version = remote.as_ref().map(|r| r.version);
        let stored = match remote.as_ref() {
            // The account already holds exactly this. Re-storing it would churn a version for nothing.
            Some(r) if r.doc == merged => false,
            _ => match backend.store(book_id, &merged, version) {
                Ok(v) => {
                    version = Some(v);
                    true
                }
                Err(SyncError::Conflict) if attempts < MAX_ATTEMPTS => continue,
                Err(SyncError::Conflict) => return Ok(Outcome::Conflict),
                Err(e) => return Err(e.to_string()),
            },
        };

        let local_differs = local.as_ref() != Some(&merged);
        if !local_differs {
            local::mark_synced(conn, book_id, version).map_err(|e| e.to_string())?;
            return Ok(if stored { Outcome::Pushed } else { Outcome::InSync });
        }

        // Writing a merged document needs a `books` row (the foreign key on `reading_progress`), so a
        // state that arrived for a book this device has not imported is reported rather than written.
        // The in-step mark is deliberately NOT written: the pass will come back to this book.
        if !local::book_exists(conn, book_id).map_err(|e| e.to_string())? {
            return Ok(Outcome::Unmatched);
        }

        local::apply(conn, book_id, &merged)?;
        local::mark_synced(conn, book_id, version).map_err(|e| e.to_string())?;
        return Ok(if stored { Outcome::Both } else { Outcome::Pulled });
    }
}

/// Exchange every book that has something to exchange, in either direction, then report what the
/// account holds that this device cannot yet place.
///
/// Two questions decide which books a pass visits, and each side can only answer one:
///
///   · What does THIS DEVICE have to send? `local::pending_local` — books with local state that was
///     never pushed, or that changed since the last pass.
///   · What has the ACCOUNT got that this device has not seen? The account's own index
///     (`versions`), compared with the version this device last agreed with. A book whose version
///     matches is already in step and is not fetched; a book whose version moved — because another
///     device pushed it — is. This is the half that makes pull work at all after the first pass.
pub fn sync_all(conn: &rusqlite::Connection, backend: &dyn SyncBackend) -> Result<SyncReport, String> {
    let mut report = SyncReport::default();

    let mut targets = local::pending_local(conn).map_err(|e| e.to_string())?;
    for (remote_id, remote_version) in backend.versions().map_err(|e| e.to_string())? {
        if !local::book_exists(conn, &remote_id).map_err(|e| e.to_string())? {
            // State on the account for a book this library does not contain — the honest "three books
            // have progress on your phone and are not here" line. Nothing is written, nothing is lost:
            // it applies the moment the book is imported. Not an error.
            report.unmatched.push(remote_id);
            continue;
        }
        if local::remote_version(conn, &remote_id).map_err(|e| e.to_string())? != Some(remote_version) {
            targets.push(remote_id);
        }
    }
    report.unmatched.sort();

    targets.sort();
    targets.dedup();
    for book_id in targets {
        let outcome = sync_one(conn, backend, &book_id)?;
        report.books.push((book_id, outcome));
    }

    Ok(report)
}
