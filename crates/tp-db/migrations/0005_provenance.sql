-- Source identity / lineage / cost per turn (docs/data-model-v2.md).
--
-- Additive: every column is nullable and old rows keep NULL. New ingests
-- populate them from the source JSONL; a re-ingest backfills any session whose
-- file still exists on disk. The fields are unrecoverable once Claude Code
-- deletes the transcript (~30 days), which is why they are captured now rather
-- than with the retrieval changes that will consume them.
--
-- `uuid` is the sound turn coordinate the format carries (the (session_id, ts)
-- coordinate collides — 9,692 groups measured locally). It is stored here now;
-- flipping the public contract to use it (LLD §16 rule 1) is a separate change.
ALTER TABLE turn ADD COLUMN uuid TEXT;
ALTER TABLE turn ADD COLUMN parent_uuid TEXT;
ALTER TABLE turn ADD COLUMN model TEXT;
ALTER TABLE turn ADD COLUMN cache_read_tokens INTEGER;
ALTER TABLE turn ADD COLUMN cache_creation_tokens INTEGER;

-- Look up a turn by its source uuid within a session — the coordinate the
-- retrieval layer will move to. Partial: only turns that have one.
CREATE INDEX turn_uuid ON turn(session_id, uuid) WHERE uuid IS NOT NULL;
