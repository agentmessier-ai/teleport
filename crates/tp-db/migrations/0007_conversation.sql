-- A stable address for a CONVERSATION, distinct from the id of one transcript
-- segment (docs/reach-provider.md).
--
-- Claude Code mints a NEW session id at every compaction. teleport published
-- that id as an address — `tp live` prints it, senders address it — while
-- guaranteeing nothing about how long it stays one. Measured on this machine in
-- a single afternoon: one conversation answered to four ids in turn, and eight
-- messages sat undeliverable in the mailboxes of ids nothing would ever drain
-- again. Both directions of one exchange had to be resent by hand.
--
-- Deliberately ADDITIVE. `message` is untouched, `session` is untouched, turns
-- are untouched: a session id remains exactly what it always was — the key of a
-- transcript segment, which is the right key for turns. What is added is a
-- second name, one that survives compaction, and a record of which segments
-- answered to it. Addressing resolves through it at the edge; nothing
-- downstream of enqueue changes.

-- One correspondent. `pid` + `cwd` + `runtime_id` is how a rotation is
-- RECOGNIZED, not what identifies the row — a compaction re-registers a new
-- session id from the same process, in the same directory, immediately.
CREATE TABLE conversation (
  id           TEXT PRIMARY KEY,   -- <machine>/<runtime>/conv-<uuid>
  machine_id   TEXT NOT NULL,
  runtime_id   TEXT NOT NULL,
  -- Host process of the most recent member. Updated on every join, so a
  -- conversation that outlives one pid (it cannot today, but a runtime that
  -- reconnects could) still points at where it currently lives.
  pid          INTEGER,
  cwd          TEXT,
  created_at   INTEGER NOT NULL,
  last_seen_at INTEGER NOT NULL
) STRICT;

CREATE INDEX conversation_host ON conversation(runtime_id, pid, last_seen_at);

-- Which transcript segments have answered to this conversation.
--
-- Kept in its OWN table rather than as a column on `live_session` because
-- `live_session` rows are pruned — that is the whole point of the scan — and
-- the membership must outlive them. Draining a conversation's inbox means
-- reading the mailboxes of every id it ever had, including ids whose live rows
-- were pruned hours ago. That is what rescues mail addressed before a rotation.
CREATE TABLE conversation_member (
  session_id      TEXT PRIMARY KEY,
  conversation_id TEXT NOT NULL REFERENCES conversation(id) ON DELETE CASCADE,
  joined_at       INTEGER NOT NULL
) STRICT;

CREATE INDEX conversation_member_conv ON conversation_member(conversation_id, joined_at);
