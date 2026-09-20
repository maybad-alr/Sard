-- READING-STATE SYNC (stage 2): the deletions that must travel with the marks.
--
-- WHY A TOMBSTONE IS NOT OPTIONAL HERE. `highlights`, `notes` and `bookmarks` delete rows OUTRIGHT —
-- there is no `deleted_at`, no trash, nothing left behind. So a reader who deletes a highlight on one
-- device and syncs has told the account "this row is gone" in the only way the tables can express it:
-- by absence. And absence is exactly what the other device already looks like, for every mark it has
-- never seen. Without a record of the deletion, the next pull re-creates the mark from the other
-- device's copy, and it comes back — which is the bug this table exists to prevent.
--
-- Additive by construction: one new table, no column on any existing one, no row written until a
-- reader actually deletes something.
CREATE TABLE IF NOT EXISTS sync_tombstones (
  -- 'highlight' | 'note' | 'bookmark'. A string rather than a number so a row read by hand says what
  -- it is, which is how the rest of this database spells its kinds.
  kind       TEXT    NOT NULL,
  -- The id of the row that was deleted. Ids are UUIDs minted by the frontend, so they are the same
  -- value on every device and a deletion can be matched against the copy that still exists elsewhere.
  id         TEXT    NOT NULL,
  -- WHICH BOOK IT BELONGED TO, kept because the row itself is gone by the time anyone asks. This is
  -- what lets the deletion travel with its book instead of needing a scan of every tombstone.
  book_id    TEXT    NOT NULL,
  deleted_at INTEGER NOT NULL,
  -- One tombstone per row, ever. Re-deleting an id that was already deleted updates the time rather
  -- than adding a second row, so the table cannot grow by repeats.
  PRIMARY KEY (kind, id)
);
