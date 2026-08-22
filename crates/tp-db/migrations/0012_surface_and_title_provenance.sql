-- Three facts teleport was told by every runtime and stored from none of them.
--
-- NOT included, because 0005_provenance already added them: turn.uuid and
-- turn.parent_uuid. The source DAG is already captured where an adapter fills
-- it — the gap is that nothing else here was.

-- ── 1. Supersession ─────────────────────────────────────────────────────────
--
-- Three of four runtimes publish a way to tell context that is still live from
-- content that has been compacted away, and each does it differently: dsh folds
-- `surfaceOp` into {nodes, replacements{shadowedSeqs}}; codex writes a
-- `CompactedItem.replacement_history` that replaces an entire prefix; pi walks
-- leaf→root to a `CompactionEntry.firstKeptEntryId`. teleport stored neither the
-- marker nor the relation, so a search hit could not say whether the text it
-- matched is still part of the conversation or was replaced an hour ago.
--
-- Stored rather than derived at read time because the derivation needs the WHOLE
-- session — a reverse scan to the newest compaction — which a windowed query
-- does not have. It is a cache of a computable fact: wrong values are repairable
-- by re-ingest, which is why `Unknown` is the honest default rather than
-- `Current`.
--
--   'current'    still part of the model's context
--   'superseded' replaced by a compaction/rollback; kept for search, not context
--   'log_only'   never was context (dsh's third surface class)
--   'unknown'    ingested before this column existed, or by an adapter that
--                does not implement its runtime's fold. NOT a synonym for
--                'current' — the distinction is the point.
ALTER TABLE turn ADD COLUMN surface TEXT NOT NULL DEFAULT 'unknown';

CREATE INDEX turn_surface_idx ON turn(session_id, surface);

-- ── 2. Reasoning that exists but cannot be read ─────────────────────────────
--
-- `thinking` is TEXT and an empty string has meant two irreconcilable things:
-- "this turn had no reasoning" and "this turn reasoned and the payload is
-- opaque to us". Codex is the case that forces the split — every one of the 15
-- reasoning items on this machine carries `summary: []` with a ~1.4 KB
-- `encrypted_content` blob, so storing "" asserts something false. pi's
-- `redacted: true` and Claude Code's `redacted_thinking` blocks are the same
-- shape.
--
--   'none'   no reasoning in the source
--   'text'   `thinking` holds readable text
--   'opaque' reasoning happened; the payload is encrypted/redacted and is NOT
--            in `thinking`
ALTER TABLE turn ADD COLUMN thinking_state TEXT NOT NULL DEFAULT 'none';

-- Backfill what is knowable from the rows already here: a non-empty `thinking`
-- IS readable text, so defaulting those to 'none' would assert the opposite of
-- what the column plainly contains.
--
-- 'opaque' is NOT backfillable — nothing in the existing rows records that
-- reasoning happened but was encrypted, which is precisely the fact the column
-- was added to hold. Those turns stay 'none' until re-ingested, and that is
-- the honest state: we do not know, and 'none' is what the old schema was
-- already claiming about them.
UPDATE turn SET thinking_state = 'text' WHERE thinking IS NOT NULL AND thinking != '';

-- ── 3. Sidechains ───────────────────────────────────────────────────────────
--
-- Claude Code marks `isSidechain` on transcript messages and writes subagent
-- transcripts to SEPARATE files (`<sessionId>/subagents/agent-<id>.jsonl`),
-- which teleport currently indexes as unrelated sessions. This column claims
-- the concept; joining the separate files to their parent session is a distinct
-- change and is deliberately not attempted here.
ALTER TABLE turn ADD COLUMN sidechain INTEGER NOT NULL DEFAULT 0;

-- ── 4. Title provenance ─────────────────────────────────────────────────────
--
-- Every runtime surveyed has a native title, in a different place each time: a
-- log event plus a projection (dsh), three columns in a SQLite side-store the
-- rollout never mentions (codex), a versioned `session_info` entry in the
-- session tree (pi), two entry types with a documented precedence (Claude Code).
-- teleport read none of them and derived its own from the first user message,
-- into the SAME column — so "this runtime has no title" and "teleport did not
-- look" were indistinguishable, and a real title could be silently outranked by
-- a truncated first message.
--
-- Codex already ships the shape, for exactly this problem, because it reads
-- Claude Code's titles when importing sessions
-- (external-agent-migration/src/sessions/title.rs:18-28):
--
--     pub(super) struct SessionTitleCandidates { custom_title, ai_title, fallback_title }
--     pub fn select(self) -> Option<String> {
--         self.custom_title.or(self.ai_title).or(self.fallback_title)
--     }
--
-- Same precedence here, one column per source, resolved at READ time:
--   COALESCE(title_user, title_ai, title_derived)
ALTER TABLE session ADD COLUMN title_user    TEXT;  -- /rename, /name, session_info
ALTER TABLE session ADD COLUMN title_ai      TEXT;  -- model-generated (Claude Code ai-title)
ALTER TABLE session ADD COLUMN title_derived TEXT;  -- teleport's fallback, marked as such

-- The existing `title` column held a mix of both kinds with no way to tell them
-- apart. Everything in it today was produced by teleport's own derivation
-- (`adapter::jsonl` for the disk path; nothing populated it on the push path),
-- so it moves wholesale into `title_derived` — which is what it always was.
--
-- `title` is left in place, unread, rather than dropped: a rebuild repopulates
-- the new columns from source, and keeping the old value costs one TEXT column
-- while making a bad rebuild recoverable rather than destructive.
UPDATE session SET title_derived = title WHERE title IS NOT NULL AND title != '';

-- ── Note on turn.seq ────────────────────────────────────────────────────────
--
-- `seq` is documented as "ordinal within session, gapless" and is populated from
-- FILE ORDER. For dsh and codex that is conversation order. For pi and Claude
-- Code it is not: both are trees, and pi re-parents branch entries onto older
-- ids, so file order and conversation order genuinely differ on a branched
-- session.
--
-- Renaming it (to `ingest_seq`, say) is the honest fix and is NOT done here.
-- It is referenced by UNIQUE(session_id, seq), by every adapter, by the resume
-- checkpoint logic and by `tp turns` paging; changing it is a separate change
-- with its own tests. Recorded here so the next reader knows the column lies
-- rather than discovering it from a wrongly-ordered pi transcript.
