-- Teleport schema v1 (LLD §4). Full schema is created up front — federation
-- tables (machine/message/live_session) are unused by P0's CLI but cost
-- nothing to have in place, and avoids a P1 migration for the join surface.

CREATE TABLE machine (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  pubkey        BLOB,
  is_self       INTEGER NOT NULL DEFAULT 0,
  addr          TEXT,
  trust         TEXT NOT NULL DEFAULT 'self',
  paired_at     INTEGER,
  last_seen_at  INTEGER,
  created_at    INTEGER NOT NULL
) STRICT;
CREATE INDEX machine_trust_idx ON machine(trust);

CREATE TABLE runtime (
  id       TEXT PRIMARY KEY,
  root     TEXT NOT NULL,
  version  TEXT,
  enabled  INTEGER NOT NULL DEFAULT 1
) STRICT;

CREATE TABLE session (
  id            TEXT PRIMARY KEY,
  machine_id    TEXT NOT NULL REFERENCES machine(id) ON DELETE CASCADE,
  runtime_id    TEXT NOT NULL REFERENCES runtime(id),
  native_id     TEXT NOT NULL,
  cwd           TEXT,
  title         TEXT,
  source_path   TEXT,
  started_at    INTEGER,
  last_turn_at  INTEGER,
  turn_count    INTEGER NOT NULL DEFAULT 0,
  UNIQUE(machine_id, runtime_id, native_id)
) STRICT;
CREATE INDEX session_recent_idx ON session(last_turn_at DESC);
CREATE INDEX session_cwd_idx    ON session(cwd);

CREATE TABLE turn (
  id          INTEGER PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL,
  role        TEXT NOT NULL,
  ts          INTEGER,
  text        TEXT,
  thinking    TEXT,
  tool_calls  TEXT,
  tokens_in   INTEGER,
  tokens_out  INTEGER,
  UNIQUE(session_id, seq)
) STRICT;

-- Column names MUST match `turn`'s column names exactly (text/thinking/tool_calls,
-- not e.g. tool_names) — SQLite's external-content snippet()/highlight() resolve
-- the source column by NAME when re-fetching un-tokenized text for a hit, and a
-- name mismatch fails at query time with an opaque "SQL logic error", but only
-- once a query actually matches a row (bm25()/MATCH alone never trip it, which
-- is what made this easy to miss until an integration test exercised a real hit).
CREATE VIRTUAL TABLE turn_fts USING fts5(
  text, thinking, tool_calls,
  content='turn', content_rowid='id',
  tokenize='unicode61 remove_diacritics 2'
);

-- turns are immutable (append-only ingest): only insert/delete need to stay in sync.
CREATE TRIGGER turn_ai AFTER INSERT ON turn BEGIN
  INSERT INTO turn_fts(rowid, text, thinking, tool_calls)
  VALUES (new.id, new.text, new.thinking, new.tool_calls);
END;
CREATE TRIGGER turn_ad AFTER DELETE ON turn BEGIN
  INSERT INTO turn_fts(turn_fts, rowid, text, thinking, tool_calls)
  VALUES ('delete', old.id, old.text, old.thinking, old.tool_calls);
END;

-- Keyed by INODE, not path (Pattern8: fluentd's named anti-pattern is tracking
-- position by filename — it breaks on rotation). See LLD §6.2 / §15 #1.
CREATE TABLE ingest_state (
  inode       INTEGER PRIMARY KEY,
  source_path TEXT NOT NULL,
  session_id  TEXT REFERENCES session(id) ON DELETE CASCADE,
  byte_offset INTEGER NOT NULL DEFAULT 0,
  last_seq    INTEGER NOT NULL DEFAULT 0,
  mtime_ms    INTEGER,
  retired_at  INTEGER
) STRICT;
CREATE INDEX ingest_state_path_idx ON ingest_state(source_path);

-- Like `message`, `live_session` has NO FK to session(id): the SessionStart
-- hook registers a session BEFORE it has been indexed, so the row must be
-- addressable without a session row existing yet.
CREATE TABLE live_session (
  session_id    TEXT PRIMARY KEY,
  pid           INTEGER NOT NULL,
  tty           TEXT,
  registered_at INTEGER NOT NULL,
  last_seen_at  INTEGER NOT NULL
) STRICT;

-- `to_session` deliberately has NO FK to session(id): a mailbox accepts
-- messages for sessions that haven't been indexed yet (the session id is an
-- address, not a row that must already exist). Same for `from_machine`.
CREATE TABLE message (
  id           TEXT PRIMARY KEY,
  to_session   TEXT NOT NULL,
  from_session TEXT,
  from_machine TEXT NOT NULL,
  kind         TEXT NOT NULL,
  body         TEXT NOT NULL,
  reply_to     TEXT REFERENCES message(id),
  created_at   INTEGER NOT NULL,
  delivered_at INTEGER,
  read_at      INTEGER,
  attempts     INTEGER NOT NULL DEFAULT 0,
  dead_at      INTEGER
) STRICT;
CREATE INDEX message_inbox_idx ON message(to_session, read_at, dead_at);
