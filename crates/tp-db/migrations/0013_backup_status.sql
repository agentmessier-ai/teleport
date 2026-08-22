-- When a backup was last taken, and of what.
--
-- The index is the only copy of 140,321 turns on the machine this was written
-- on — a quarter of the corpus, because Claude Code deletes transcripts at
-- around 30 days. `tp verify` says so and suggests `tp backup`. Nothing knew
-- whether that suggestion had ever been taken, which makes the advice a
-- reminder rather than a fact.
--
-- WHY NOT SCAN THE FILESYSTEM. `tp backup <dest>` writes wherever the caller
-- says — an external disk, a synced folder, /tmp by mistake. There is no
-- directory to look in, so the only way to know is to have written it down.
--
-- ONE ROW, like daemon_status. The interesting question is "how long since the
-- last one", not the history; a log of every backup ever taken would be a
-- second thing to prune.
--
-- NOTE ON RESTORE, which is the one confusing case: this row lives IN the
-- database, so a snapshot carries the timestamp of the backup that produced it.
-- Restore that snapshot and `tp version` reports the backup as older than it
-- is — the row describes the file it was copied from, not the copy. That is
-- the honest reading either way: a restored database has not been backed up
-- since it became this database.
CREATE TABLE backup_status (
  id          INTEGER PRIMARY KEY CHECK (id = 1),
  taken_at    INTEGER NOT NULL,   -- unix ms
  dest        TEXT    NOT NULL,   -- where it went, so "which copy" is answerable
  turn_count  INTEGER NOT NULL,   -- what was in it, to see drift since
  bytes       INTEGER NOT NULL
) STRICT;
