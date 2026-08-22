-- Presence regime and delivery channel per live session (docs/reach-provider.md).
--
-- Until now `reconcile` treated the process scan as authoritative for EVERY
-- row: anything whose pid the scan did not find this cycle was deleted. That is
-- right for a runtime the scan can see, and fatal for one it cannot — a
-- correctly registered session of an unscannable harness is pruned within one
-- SCAN_INTERVAL_SECS. It already happened once, to pi, before `recognize_runtime`
-- learned about it (see a_hook_registered_pi_session_survives_a_scan_cycle).
--
-- 'scan' preserves exactly that behaviour and is the default for every existing
-- row, so this migration changes nothing for Claude Code or pi. 'declared' means
-- the runtime owns its own liveness: the scan may neither create nor prune the
-- row, and it expires on a heartbeat timeout instead.
ALTER TABLE live_session ADD COLUMN presence TEXT NOT NULL DEFAULT 'scan';

-- Where to send the wake for this session. NULL keeps today's path: infer a
-- tmux pane or iTerm2 tty from pid/tty. A harness with no tty (a web GUI, a
-- multiplexed host) declares `exec:<argv>` or a loopback `http://…` instead.
ALTER TABLE live_session ADD COLUMN deliver TEXT;

-- Set when a declared session's heartbeat lapses; cleared by the next heartbeat.
-- Expiry is deliberately two-stage — mark, then evict much later — so a laptop
-- waking from sleep does not lose every declared registration to a brief stall.
-- Kubernetes splits these the same way: ~40s of silence only marks a node
-- (Ready=Unknown + unreachable taint), and pods are not evicted until
-- tolerationSeconds (300s, 7.5x longer) has also passed.
ALTER TABLE live_session ADD COLUMN stale_at INTEGER;

-- The runtime this row belongs to. Derivable from session_id's middle segment,
-- but stored so the presence sweep and `tp live` can filter without parsing a
-- composite id in SQL.
ALTER TABLE live_session ADD COLUMN runtime_id TEXT;

-- The sweep and the scan both filter on presence; the scan additionally filters
-- on pid. Partial index: 'declared' rows are the minority and the only ones the
-- heartbeat sweep touches.
CREATE INDEX live_session_declared ON live_session(stale_at, last_seen_at)
  WHERE presence = 'declared';
