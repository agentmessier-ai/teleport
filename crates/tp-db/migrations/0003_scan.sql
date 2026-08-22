-- Active-scan discovery (follow-up to same-machine-poke-design.md): tpd now
-- reconciles live_session against a periodic tmux/iTerm2 sweep, not just
-- hook registrations. `source` distinguishes a row with a REAL session_id
-- (from Claude Code's own hook) from one the scan created with an INFERRED
-- id (matched by cwd, or a synthetic placeholder if no match exists yet).
ALTER TABLE live_session ADD COLUMN source TEXT NOT NULL DEFAULT 'hook';
