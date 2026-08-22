-- Teleport Panel (docs/teleport-panel-design.md): human-readable aliases
-- for terminals, keyed by cwd (stable across /clear, restart, window
-- changes — unlike tty which macOS recycles, or session_id which is
-- per-process-incarnation). Written by the panel itself, not by tp/tpd.
CREATE TABLE terminal_alias (
  cwd        TEXT PRIMARY KEY,
  alias      TEXT NOT NULL,
  last_tty   TEXT,
  last_pid   INTEGER,
  updated_at INTEGER NOT NULL
) STRICT;
