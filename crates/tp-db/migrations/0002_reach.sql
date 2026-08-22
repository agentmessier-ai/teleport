-- Same-machine poke (docs/same-machine-poke-design.md):
--   cwd          — needed for the LLD §7.3 `readonly` cwd-allowlist guard,
--                  which cannot be implemented without storing it.
--   last_wake_at — 10s wake coalescing (LLD §7.3), previously unimplemented.
ALTER TABLE live_session ADD COLUMN cwd TEXT;
ALTER TABLE live_session ADD COLUMN last_wake_at INTEGER;
