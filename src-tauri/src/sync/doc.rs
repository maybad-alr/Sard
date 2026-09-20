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
///
/// **2 CARRIES THE MARKS.** Version 1 was the reading state alone — the position, the two section sets
/// and the furthest mark. Version 2 adds the annotations (highlights, notes, bookmarks) and the
/// tombstones that make their deletions travel. The bump is what keeps the two honest in both
/// directions: a v1 document is still read here (its `records`/`tombstones` are simply empty), and a
/// v2 document is refused by a v1 build rather than being accepted with the marks dropped.
/// **3 CARRIES THE NAME OF THE BOOK.** Version 2 had the marks; version 3 adds the card — the title,
/// the author and the format — so that a device can say WHICH book has reading on it that this library
/// does not contain, instead of showing a reader a SHA-256 and calling it a report. Bumping the format
/// is what keeps that honest: a v2 build, which would drop the card on the way back out, refuses the
/// document instead.
pub const FORMAT: u32 = 3;

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

/// One annotation, carried as the database holds it.
///
/// WHY A WHOLE ROW RATHER THAN A TYPED STRUCT PER TABLE. The three tables differ in their columns, and
/// they have grown columns repeatedly over this repository's history (`alpha`, `title`,
/// `chapter_label`, `fraction`). A struct per table would be three more places to update at the next
/// migration, and forgetting one fails SILENTLY: a column that quietly stops travelling. A row copied
/// column for column carries whatever the table has, and `FORMAT` is what makes that safe — a document
/// from a newer build, whose columns this one does not know, is refused rather than half-understood.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// `highlight` | `note` | `bookmark`.
    pub kind: String,
    /// The row's primary key — a UUID minted by the frontend, and therefore the same value on every
    /// device, which is what lets two copies of one mark be recognised as the same mark.
    pub id: String,
    /// The row's columns, exactly as stored.
    pub data: serde_json::Value,
    /// WHEN THIS COPY WAS WRITTEN, for choosing between two of them: `updated_at` where the table has
    /// one and `created_at` where it does not. A row with neither reads as 0, which loses to any real
    /// timestamp — the direction that cannot steal a mark from a device that has edited it.
    pub updated_at: i64,
}

/// What the book IS, for a device that does not have it yet.
///
/// The document is keyed by the file's SHA-256, which is the right identity and a useless report: a
/// reader told that "eb1f…c9 has progress on your phone" has learned nothing they can act on. The card
/// is what turns that into a book they can recognise and go and copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookCard {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    /// `epub` | `pdf` — what the file is.
    #[serde(default)]
    pub format: Option<String>,
}

/// A deletion, which the annotation tables themselves cannot express — they delete rows outright, and
/// absence is indistinguishable from "never seen" on the other device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    /// `highlight` | `note` | `bookmark`.
    pub kind: String,
    pub id: String,
    pub deleted_at: i64,
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
    /// The reader's marks: highlights, notes and bookmarks, each as the row it is.
    #[serde(default)]
    pub records: Vec<Record>,
    /// Deletions of those marks, which the rows themselves cannot carry. See `Tombstone`.
    #[serde(default)]
    pub tombstones: Vec<Tombstone>,
    /// What the book is called, so another device can name it. NOT counted by `is_empty`: a card on
    /// its own is a book nobody has read, and such a book has no business on the account at all.
    #[serde(default)]
    pub book: Option<BookCard>,
}

impl Default for BookState {
    /// An empty document of THIS build's format. Written out rather than derived because a derived
    /// `Default` would leave `format` at 0, and a document that understates its own format is one a
    /// future build would mistake for something older than it is.
    fn default() -> Self {
        Self {
            format: FORMAT,
            progress: None,
            chapters_read: vec![],
            seen_start: vec![],
            furthest: None,
            records: vec![],
            tombstones: vec![],
            book: None,
        }
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
            && self.records.is_empty()
            && self.tombstones.is_empty()
    }
}
