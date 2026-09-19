//! The document that travels — ONE PER BOOK.
//!
//! # Why a book, and not a row
//!
//! Everything Sard records about reading a book belongs to that book: where the reader is, which
//! chapters they have read, how far they ever reached, and — from the annotations stage on — their
//! highlights, notes and bookmarks. A book is also the only unit the reader thinks in ("continue on my
//! phone where I stopped"), and the only unit the two devices can agree on without a shared row id:
//! `books.id` is the SHA-256 of the file, so the same file is the same book on every device. A
//! per-row protocol would need a shared key for every row in five tables; a per-book document needs
//! exactly one key, and it already exists.
//!
//! # The fields are the app's own, not a new vocabulary
//!
//! `progress` mirrors the `reading_progress` row. The two sets and the furthest mark are the per-book
//! `settings` rows the reader already writes (`chapters_read:<id>`, `seen_start:<id>`,
//! `furthest_read:<id>`), carried with their own shapes so that what arrives is what the reader's own
//! code already knows how to parse — no translation layer to drift.

use serde::{Deserialize, Serialize};

/// The document format this build writes and is willing to apply.
///
/// A document claiming a HIGHER format is not merged, not applied and not overwritten: a newer Sard
/// may have added a field this build would drop on the way back out, and silently halving a reader's
/// state is worse than declining to touch it. The reader is told instead (see `Outcome::SkippedFormat`).
pub const FORMAT: u32 = 1;

fn default_format() -> u32 {
    FORMAT
}

fn minus_one() -> i64 {
    -1
}

/// The current reading position — `reading_progress`, as it is stored.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// The locator the reader's engine navigates by. `None` on a row written before one existed.
    #[serde(default)]
    pub cfi: Option<String>,
    /// How far through the book, 0..1, when the engine reported one.
    #[serde(default)]
    pub fraction: Option<f64>,
    /// Unix seconds **on the device that made the move** — see the tie-break note in `merge`.
    pub updated_at: i64,
}

/// The furthest point the reader ever reached — `furthest_read:<id>`, field for field.
///
/// Every field but `cfi` and `fraction` is for DISPLAY (see `features/reader/furthestRead.ts`), and
/// they travel unchanged so the destination device names the chapter exactly as the source did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Furthest {
    /// Where it is — the only field navigation uses.
    pub cfi: String,
    /// How far through the book, 0..1. The ordering fallback the merge uses, because a CFI's reading
    /// order is the engine's to define and no Rust in this crate can compare two of them.
    pub fraction: f64,
    /// The chapter label as it read when the mark was set.
    #[serde(default)]
    pub label: Option<String>,
    /// The contents href the mark sits in, for the Contents panel's own naming.
    #[serde(default)]
    pub href: Option<String>,
    /// The spine section the mark lives in, or -1 when it was not known.
    #[serde(default = "minus_one")]
    pub sec: i64,
}

/// One book's synced reading state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookState {
    /// Absent in a doc written before the field existed, and read as `FORMAT` — so the first version
    /// of this document is also the one that never had to announce itself.
    #[serde(default = "default_format")]
    pub format: u32,
    #[serde(default)]
    pub progress: Option<Progress>,
    /// Spine-section indices the reader has read. GROW-ONLY in the reader (`markChapterRead` returns
    /// early on a section it already holds), which is what lets the merge be a union.
    #[serde(default)]
    pub chapters_read: Vec<i64>,
    /// Sections whose beginning the reader has seen. Same grow-only set, same union.
    #[serde(default)]
    pub seen_start: Vec<i64>,
    /// The furthest point reached. Monotonic by construction in the reader, which is what lets the
    /// merge take the further of the two without a clock.
    #[serde(default)]
    pub furthest: Option<Furthest>,
}

impl Default for BookState {
    /// An empty document of THIS build's format. Written out rather than derived because a derived
    /// `Default` would leave `format` at 0, and a document that understates its own format is one a
    /// future build would mistake for something older than it is.
    fn default() -> Self {
        Self { format: FORMAT, progress: None, chapters_read: vec![], seen_start: vec![], furthest: None }
    }
}

impl BookState {
    /// Does this carry anything at all? A book the reader has never opened produces this, and an empty
    /// document is not worth storing on either side.
    pub fn is_empty(&self) -> bool {
        self.progress.is_none()
            && self.chapters_read.is_empty()
            && self.seen_start.is_empty()
            && self.furthest.is_none()
    }
}
