//! Merging two copies of one book's state — the whole rule, in one file, as pure functions.
//!
//! # Two of the three merges need no clock at all
//!
//! The obvious design for cross-device state is "the newest write wins", and it is a trap here: it
//! makes every merge depend on two devices' clocks agreeing, in a feature whose entire job is to be
//! right about which of two positions is the reader's. So the rule is set by the SHAPE of each value,
//! not by when it was written:
//!
//!   · `chapters_read` / `seen_start` — the reader only ever ADDS to these sets (`markChapterRead`
//!     returns early on a section it already holds), so the merge is a UNION. Read chapters 1–3 on the
//!     phone and 5–7 on the desktop and the result is 1,3,5,7 — neither device can lose what the other
//!     recorded, whatever the clocks say.
//!   · `furthest` — the reader only ever ADVANCES this mark, so the merge takes the FURTHER of the
//!     two. Same property: clock-free.
//!   · `progress` — the one value that legitimately moves BACKWARDS (the reader returns to an earlier
//!     page on purpose), so it is the one value that cannot be ordered by shape. It carries the
//!     writing device's `updated_at`, and the newer move wins — a rewind is a deliberate act and is
//!     respected rather than overruled by "furthest".
//!
//! # Ties, and why the tie-break is not a lie
//!
//! Two devices can write in the same second, and `progress` can then genuinely tie. The tie-break is
//! the CFI string, then the fraction — which is NOT reading order and is not meant to be: it only has
//! to be the SAME ANSWER ON BOTH DEVICES, because a merge that disagrees with itself never converges
//! and would ping-pong a book between two positions forever. Reading order is unavailable anyway: a
//! CFI's order is the reader engine's to define (see `features/reader/furthestRead.ts`), and no Rust
//! in this crate can compare two of them.
//!
//! The consequence is a real, documented limitation: a position written by a device whose clock is
//! wrong can win or lose against its true age. Everything that CAN be made clock-independent is.

use std::cmp::Ordering;

use super::doc::{BookState, Furthest, Progress, Record, Tombstone, FORMAT};

/// The document both devices end up holding, given what each of them has.
///
/// Total, commutative and idempotent — `merge(a, b) == merge(b, a)` and `merge(a, a) == a` — which is
/// what makes "sync twice" and "sync in either order" produce the same book. The tests below hold it
/// to those laws rather than to examples of them.
pub fn merge(local: Option<&BookState>, remote: Option<&BookState>) -> BookState {
    match (local, remote) {
        (None, None) => BookState { format: FORMAT, ..BookState::default() },
        (Some(only), None) | (None, Some(only)) => only.clone(),
        (Some(a), Some(b)) => {
            // The deletions are settled FIRST, because the marks are filtered by them below — and the
            // filter is the whole reason stage 2 needed a table of its own.
            let tombstones = merge_tombstones(&a.tombstones, &b.tombstones);
            BookState {
                // This build's format: a document claiming a higher one never reaches here (see `sync_one`).
                format: FORMAT,
                progress: merge_progress(a.progress.as_ref(), b.progress.as_ref()),
                chapters_read: union(&a.chapters_read, &b.chapters_read),
                seen_start: union(&a.seen_start, &b.seen_start),
                furthest: merge_furthest(a.furthest.as_ref(), b.furthest.as_ref()),
                // The marks are unioned by id, and then the deletions are applied — AFTER the union, so
                // a mark that one device still holds and the other has deleted comes out deleted.
                records: without_deleted(merge_records(&a.records, &b.records), &tombstones),
                tombstones,
            }
        }
    }
}

/// Every mark from both sides, one entry per id.
///
/// TWO COPIES OF ONE MARK ARE THE SAME MARK, because the id is a UUID and both devices got it from the
/// same row. Which copy wins is decided by `updated_at` — the edit that happened later — with the
/// row's own text as the tie-break, for the same reason the position has one: a merge that disagrees
/// with itself never converges, and two devices editing one note in the same second is a tie that has
/// to be broken the same way on both.
fn merge_records(a: &[Record], b: &[Record]) -> Vec<Record> {
    let mut out: std::collections::BTreeMap<(String, String), Record> = std::collections::BTreeMap::new();
    for record in a.iter().chain(b.iter()) {
        let key = (record.kind.clone(), record.id.clone());
        match out.get(&key) {
            Some(held) if record_key(held) >= record_key(record) => {}
            _ => {
                out.insert(key, record.clone());
            }
        }
    }
    out.into_values().collect()
}

fn record_key(record: &Record) -> (i64, String) {
    (record.updated_at, record.data.to_string())
}

/// Every deletion from both sides, keeping the LATEST time for an id deleted twice.
fn merge_tombstones(a: &[Tombstone], b: &[Tombstone]) -> Vec<Tombstone> {
    let mut out: std::collections::BTreeMap<(String, String), Tombstone> = std::collections::BTreeMap::new();
    for stone in a.iter().chain(b.iter()) {
        let key = (stone.kind.clone(), stone.id.clone());
        match out.get(&key) {
            Some(held) if held.deleted_at >= stone.deleted_at => {}
            _ => {
                out.insert(key, stone.clone());
            }
        }
    }
    out.into_values().collect()
}

/// Drop the marks that were deleted, and only those.
///
/// A deletion is a fact about a ROW, not about a device: the mark is gone, and the copy still sitting
/// on the other machine is a copy of something that no longer exists. Without this the next pull
/// re-creates it, which is the defect the tombstone table was added for.
///
/// The comparison is `deleted_at >= updated_at`, so a deletion recorded at the same second as the last
/// edit wins — the deletion is the later intent. A mark whose edit is genuinely NEWER than a deletion
/// survives, which is what keeps this from being a rule that can delete something a reader just wrote.
fn without_deleted(records: Vec<Record>, tombstones: &[Tombstone]) -> Vec<Record> {
    if tombstones.is_empty() {
        return records;
    }
    records
        .into_iter()
        .filter(|record| {
            !tombstones.iter().any(|stone| {
                stone.kind == record.kind && stone.id == record.id && stone.deleted_at >= record.updated_at
            })
        })
        .collect()
}

/// The newer move wins; an exact tie falls back to the CFI, then the fraction — see the module note.
fn merge_progress(a: Option<&Progress>, b: Option<&Progress>) -> Option<Progress> {
    match (a, b) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(x), Some(y)) => Some(if progress_cmp(x, y) == Ordering::Less { y.clone() } else { x.clone() }),
    }
}

/// The total order `merge_progress` compares, written out step by step rather than as a tuple key
/// because a fraction is an `f64`: `total_cmp` is a real total order (NaN included), while `>=` on a
/// tuple containing NaN is false in BOTH directions, which would make the answer depend on which
/// device asked and break convergence.
///
/// A missing locator sorts below any string, so a row with no CFI cannot beat one that has one when
/// both were written in the same second.
fn progress_cmp(x: &Progress, y: &Progress) -> Ordering {
    x.updated_at
        .cmp(&y.updated_at)
        .then_with(|| x.cfi.as_deref().unwrap_or("").cmp(y.cfi.as_deref().unwrap_or("")))
        .then_with(|| x.fraction.unwrap_or(f64::NEG_INFINITY).total_cmp(&y.fraction.unwrap_or(f64::NEG_INFINITY)))
}

/// The further mark wins; an exact tie falls back to the CFI, for the same convergence reason.
fn merge_furthest(a: Option<&Furthest>, b: Option<&Furthest>) -> Option<Furthest> {
    match (a, b) {
        (None, None) => None,
        (Some(only), None) | (None, Some(only)) => Some(only.clone()),
        (Some(x), Some(y)) => Some(if furthest_cmp(x, y) == Ordering::Less { y.clone() } else { x.clone() }),
    }
}

fn furthest_cmp(x: &Furthest, y: &Furthest) -> Ordering {
    // The fraction is the ordering the reader's own fallback uses (`isBeyond`), and it orders
    // positions correctly in both directions: the fraction advances in READING order, so an RTL book
    // still has a larger fraction further in.
    x.fraction.total_cmp(&y.fraction).then_with(|| x.cfi.cmp(&y.cfi))
}

/// Union of two grow-only sets, sorted and deduplicated — so the stored value is a function of the
/// two inputs and not of the order they were combined in.
fn union(a: &[i64], b: &[i64]) -> Vec<i64> {
    let mut out: Vec<i64> = a.iter().chain(b.iter()).copied().filter(|n| *n >= 0).collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(updated_at: i64, cfi: &str, fraction: f64) -> Option<Progress> {
        Some(Progress { cfi: Some(cfi.into()), fraction: Some(fraction), updated_at })
    }

    fn state(p: Option<Progress>, chapters: &[i64], furthest: Option<Furthest>) -> BookState {
        BookState {
            progress: p,
            chapters_read: chapters.to_vec(),
            furthest,
            ..BookState::default()
        }
    }

    /// The property the whole feature rests on, checked on the same inputs the device tests use:
    /// the merge does not depend on which side is "local".
    #[test]
    fn merge_is_commutative_and_idempotent() {
        let phone = state(progress(200, "/6/4!", 0.6), &[1, 2, 3], None);
        let desktop = state(progress(100, "/6/10!", 0.9), &[5, 6, 7], None);

        assert_eq!(merge(Some(&phone), Some(&desktop)), merge(Some(&desktop), Some(&phone)));
        assert_eq!(merge(Some(&phone), Some(&phone)), phone);
        assert_eq!(merge(Some(&phone), None), phone);
        assert_eq!(merge(None, Some(&phone)), phone);
    }

    /// THE REWIND. The desktop is further in but older; the phone's newer move is the reader's actual
    /// position. "Furthest" must not win here — that is the whole reason progress is not merged the
    /// way `furthest` is.
    #[test]
    fn a_deliberate_rewind_beats_a_further_but_older_position() {
        let rewound = state(progress(200, "/6/2!", 0.10), &[], None);
        let further = state(progress(100, "/6/40!", 0.90), &[], None);

        let merged = merge(Some(&rewound), Some(&further));
        let position = merged.progress.expect("a position survives");
        assert_eq!(position.updated_at, 200, "the newer move is the position");
        assert_eq!(position.fraction, Some(0.10));
    }

    /// Two devices in the same second still converge on ONE answer.
    #[test]
    fn a_tie_in_the_same_second_converges_on_one_position() {
        let a = state(progress(500, "/6/2!", 0.2), &[], None);
        let b = state(progress(500, "/6/8!", 0.8), &[], None);

        let ab = merge(Some(&a), Some(&b));
        let ba = merge(Some(&b), Some(&a));
        assert_eq!(ab, ba, "a merge that disagrees with itself never converges");
    }

    /// Chapters read on two devices are all kept — the case the union exists for.
    #[test]
    fn chapters_read_on_two_devices_are_unioned_not_chosen_between() {
        let phone = state(None, &[1, 2, 3], None);
        let desktop = state(None, &[5, 6, 7], None);

        let merged = merge(Some(&phone), Some(&desktop));
        assert_eq!(merged.chapters_read, vec![1, 2, 3, 5, 6, 7]);
    }

    /// Overlapping sets do not duplicate, and an already-merged pair is stable.
    #[test]
    fn overlapping_chapters_deduplicate_and_the_result_is_stable() {
        let a = state(None, &[3, 1, 2], None);
        let b = state(None, &[2, 3, 4], None);

        let merged = merge(Some(&a), Some(&b));
        assert_eq!(merged.chapters_read, vec![1, 2, 3, 4], "sorted and deduplicated");
        assert_eq!(merge(Some(&merged), Some(&a)), merged, "merging again changes nothing");
        assert_eq!(merge(Some(&merged), Some(&b)), merged);
        assert_eq!(merge(Some(&merged), Some(&merged)), merged);
    }

    /// The furthest mark advances to the further of the two, display fields and all.
    #[test]
    fn furthest_takes_the_further_mark() {
        let mk = |cfi: &str, fraction: f64, label: &str| Furthest {
            cfi: cfi.into(),
            fraction,
            label: Some(label.into()),
            href: None,
            sec: 3,
        };
        let phone = state(None, &[], Some(mk("/6/4!", 0.4, "الفصل الرابع")));
        let desktop = state(None, &[], Some(mk("/6/9!", 0.9, "الفصل التاسع")));

        let merged = merge(Some(&phone), Some(&desktop));
        let f = merged.furthest.expect("a mark survives");
        assert_eq!(f.cfi, "/6/9!", "the further mark wins");
        assert_eq!(f.label.as_deref(), Some("الفصل التاسع"), "and its display fields travel with it");
    }

    /// Two documents with nothing in them merge to nothing — not to a phantom book.
    #[test]
    fn merging_two_empty_documents_is_empty() {
        let merged = merge(Some(&BookState::default()), Some(&BookState::default()));
        assert!(merged.is_empty());
        assert_eq!(merged.format, FORMAT);
    }

    /// A corrupt value from an older build (a NaN fraction, a negative section) cannot make the merge
    /// order-dependent or put junk into the sets.
    #[test]
    fn junk_values_neither_change_the_order_nor_enter_the_sets() {
        let nan = Some(Progress { cfi: Some("/6/1!".into()), fraction: Some(f64::NAN), updated_at: 7 });
        let normal = progress(7, "/6/1!", 0.5);
        let a = state(nan, &[-1, 4], None);
        let b = state(normal, &[4, 9], None);

        // Compared as the DOCUMENT THAT WOULD BE WRITTEN, not as Rust values: a NaN fraction is
        // unequal to itself under `==`, so a value comparison would fail here for a reason that has
        // nothing to do with convergence. What both devices store is the claim that matters.
        let ab = serde_json::to_value(merge(Some(&a), Some(&b))).unwrap();
        let ba = serde_json::to_value(merge(Some(&b), Some(&a))).unwrap();
        assert_eq!(ab, ba, "both devices must write the same document");
        assert_eq!(merge(Some(&a), Some(&b)).chapters_read, vec![4, 9], "negative indices never travel");
    }

    // ---- the marks (stage 2) ----------------------------------------------------------------------

    fn highlight(id: &str, excerpt: &str, updated_at: i64) -> Record {
        Record {
            kind: "highlight".into(),
            id: id.into(),
            data: serde_json::json!({ "id": id, "book_id": "book", "text_excerpt": excerpt }),
            updated_at,
        }
    }

    fn deleted(kind: &str, id: &str, deleted_at: i64) -> Tombstone {
        Tombstone { kind: kind.into(), id: id.into(), deleted_at }
    }

    fn with_marks(records: Vec<Record>, tombstones: Vec<Tombstone>) -> BookState {
        BookState { records, tombstones, ..BookState::default() }
    }

    /// A mark made on one device travels, and the same mark on both is one mark.
    #[test]
    fn a_mark_from_one_device_arrives_once_and_the_newer_copy_wins() {
        let phone = with_marks(vec![highlight("h1", "المقتبس", 100)], vec![]);
        let desktop = with_marks(vec![highlight("h1", "المقتبس المعدَّل", 200)], vec![]);

        let merged = merge(Some(&phone), Some(&desktop));
        assert_eq!(merged.records, vec![highlight("h1", "المقتبس المعدَّل", 200)], "the later edit wins");
        assert_eq!(merge(Some(&desktop), Some(&phone)).records, merged.records, "and the order does not matter");
    }

    /// THE DEFECT THE TOMBSTONE TABLE EXISTS FOR. One device deletes a highlight; the other still has
    /// it. The deletion has to win, or the mark comes back on the next pull.
    #[test]
    fn a_deleted_mark_does_not_come_back_from_the_copy_that_still_has_it() {
        let deleter = with_marks(vec![], vec![deleted("highlight", "h1", 300)]);
        let holder = with_marks(vec![highlight("h1", "المقتبس", 100)], vec![]);

        let merged = merge(Some(&deleter), Some(&holder));
        assert!(merged.records.is_empty(), "the mark stays deleted");
        assert_eq!(merged.tombstones.len(), 1, "and the deletion is kept, so it keeps travelling");
        assert_eq!(merge(Some(&holder), Some(&deleter)).records, Vec::new(), "in either order");
    }

    /// A deletion recorded at the same second as the last edit still wins: the deletion is the later
    /// intent, and a tie that went the other way would resurrect a mark the reader removed.
    #[test]
    fn a_deletion_at_the_same_second_as_the_edit_still_wins() {
        let deleter = with_marks(vec![], vec![deleted("note", "n1", 500)]);
        let editor = with_marks(
            vec![Record {
                kind: "note".into(),
                id: "n1".into(),
                data: serde_json::json!({ "id": "n1", "body": "ملاحظة" }),
                updated_at: 500,
            }],
            vec![],
        );
        assert!(merge(Some(&deleter), Some(&editor)).records.is_empty());
    }

    /// A mark whose edit is genuinely newer than a deletion survives — the rule must not be able to
    /// delete something a reader has written since.
    #[test]
    fn an_edit_newer_than_a_deletion_survives() {
        let deleter = with_marks(vec![], vec![deleted("bookmark", "b1", 400)]);
        let editor = with_marks(
            vec![Record { kind: "bookmark".into(), id: "b1".into(), data: serde_json::json!({}), updated_at: 600 }],
            vec![],
        );
        assert_eq!(merge(Some(&deleter), Some(&editor)).records.len(), 1);
    }

    /// Marks and deletions do not interfere across kinds: deleting a note leaves a highlight that
    /// happens to share nothing alone, and a tombstone for an id never removes another kind's row.
    #[test]
    fn a_tombstone_only_removes_its_own_kind() {
        let a = with_marks(vec![highlight("x", "مقتبس", 100)], vec![]);
        let b = with_marks(vec![], vec![deleted("note", "x", 200)]);

        assert_eq!(merge(Some(&a), Some(&b)).records.len(), 1, "the highlight is untouched");
    }
}
