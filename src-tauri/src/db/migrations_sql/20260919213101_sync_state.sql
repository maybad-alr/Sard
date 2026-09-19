-- READING-STATE SYNC (stage 1): what THIS device knows about the account's copy of each book.
--
-- Additive by construction. Neither table is read or written unless sync is switched on, no existing
-- table gains a column, and no row is written by this migration — so an installation that never signs
-- in is byte-identical to one built before it.
--
-- WHY `sync_state` CARRIES NO FOREIGN KEY TO `books`, unlike every other per-book table here.
--
-- The other tables answer "what does this reader have". This one answers "what does the ACCOUNT
-- have", and those two sets are not the same: a reader who reads on a phone and then installs Sard on
-- a desktop has state on the server for books the desktop has not imported yet. A doc that arrives
-- for a book this device cannot hold (no `books` row, so no FK target) is not an error and must not
-- be dropped — the moment that book is imported, the same state is waiting for it. A FK here would
-- turn a legitimate state into a constraint violation, and the cascade would additionally erase the
-- account's version whenever a reader tidied up their library.
CREATE TABLE IF NOT EXISTS sync_state (
  -- The book's content hash — the same id on every device that holds the same file (`books.id`).
  book_id        TEXT PRIMARY KEY,
  -- The version the SERVER held when this device last agreed with it. NULL = never synced, which is
  -- what makes "push this for the first time" distinguishable from "already in step".
  remote_version INTEGER,
  -- Unix seconds of the last successful exchange, for the "last synced …" line and nothing else.
  synced_at      INTEGER,
  -- Unix seconds of the last LOCAL write that changed what this book should carry, or NULL when this
  -- device has nothing unpushed. Stamped from the two write paths (`progress_save`, `settings::set`)
  -- rather than from a SQLite trigger: this repository has no triggers, and the alternative here is
  -- not a rule in two places but one call in each of the two functions every write already goes
  -- through.
  dirty_at       INTEGER
);
