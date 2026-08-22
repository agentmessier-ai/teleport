//! The retrieval seam (LLD §16). Both the scan backend and the index backend
//! satisfy `RetrievalProvider`, and the *only* way to build a public `Hit` is
//! through `Hit::redacted_from`, so no backend can emit unscrubbed content.

use crate::id::SessionId;
use crate::turn::Role;
use std::time::Duration;

pub const DEFAULT_WINDOW: Duration = Duration::from_secs(6 * 3600);

/// What a caller is allowed to ask for. `since` always has a value — an
/// unbounded query must be requested explicitly by widening it, never by
/// omission (LLD §16 rule 3).
#[derive(Debug, Clone)]
pub struct Scope {
    /// Match against session cwd — name, basename, or substring. `None` = every folder.
    pub folder: Option<String>,
    pub since: Duration,
    /// Empty = all known runtimes.
    pub runtimes: Vec<String>,
    /// Exclusive upper bound, unix ms. `None` = up to now.
    ///
    /// `since` alone can only express "the last N" — its window always ends at
    /// now. Asking about a specific past day therefore had no answer here at
    /// all: `sessions --since 7d` on the 11th cannot mean the 4th.
    pub until: Option<i64>,
}

impl Scope {
    /// The folder filter as every provider should see it.
    ///
    /// Defined once because the two providers matched DIFFERENT broken subsets
    /// of the same input: scan compared against the encoded transcript path so
    /// no real path matched, and the index's `cwd LIKE '%x%'` missed a trailing
    /// separator — so `--folder /a/b/` and `--folder /a/b` disagreed depending
    /// on which backend answered. Each returned an empty list, which reads as
    /// "you never worked there" rather than "the filter didn't understand you".
    /// `(lower, upper)` in unix ms — the window as both providers must read it.
    pub fn range_ms(&self, now_ms: i64) -> (i64, Option<i64>) {
        (
            now_ms.saturating_sub(self.since.as_millis() as i64),
            self.until,
        )
    }

    pub fn folder_needle(&self) -> Option<String> {
        let f = self.folder.as_ref()?;
        let t = f.trim().trim_end_matches(['/', '\\']);
        (!t.is_empty()).then(|| t.to_string())
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            folder: None,
            since: DEFAULT_WINDOW,
            runtimes: Vec::new(),
            until: None,
        }
    }
}

impl Scope {
    /// True when this scope forces a provider without `unscoped_ok` into its
    /// slow path — the CLI/MCP layer warns rather than silently blocking.
    pub fn is_broad(&self) -> bool {
        self.folder.is_none() && self.since > Duration::from_secs(24 * 3600)
    }
}

#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    pub regex: bool,
    /// Off by default at every layer: `thinking` is opt-in to search *and* to
    /// return (LLD §9 gate).
    pub include_thinking: bool,
    pub limit: usize,
}

/// Not telemetry — a correctness contract. A caller concluding "I never
/// discussed X" from a truncated scan is the failure mode being designed
/// against (LLD §16 rule 3, mirroring §8.3).
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub sessions_scanned: usize,
    pub truncated: bool,
    /// Set when the provider could not fully honour the request (e.g. a scan
    /// hit its scan budget, a peer timed out).
    pub degraded: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Retrieved<T> {
    pub items: Vec<T>,
    pub coverage: Coverage,
}

impl<T> Retrieved<T> {
    pub fn new(items: Vec<T>, coverage: Coverage) -> Self {
        Self { items, coverage }
    }
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Retrieved<U> {
        Retrieved {
            items: self.items.into_iter().map(f).collect(),
            coverage: self.coverage,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    /// bm25 relevance ordering. Index only; a scan returns file order.
    pub ranked: bool,
    /// Whether an unscoped query is cheap. False for scan.
    pub unscoped_ok: bool,
    pub search_thinking: bool,
    /// Whether `Query::regex` is honored. The FTS5 index tokenizes and matches
    /// phrases — it cannot evaluate a regex — so a regex query there would
    /// silently degrade to a literal-string search and report a false "no
    /// matches". Declared here so the caller can refuse instead.
    pub regex: bool,
}

/// The universal coordinate is `(session_id, ts)` — NOT `seq`. Deriving a turn
/// ordinal in a scan requires parsing every line, which destroys the raw
/// substring fast path. `seq` is an optional index-provided convenience and is
/// never an identity anyone persists (LLD §16 rule 1).
#[derive(Debug, Clone)]
pub struct TurnRef {
    pub session_id: String,
    pub ts: Option<i64>,
    pub seq: Option<i64>,
}

/// Provider-internal, pre-redaction. Deliberately not re-exported for public use.
#[derive(Debug, Clone)]
pub struct RawHit {
    pub at: TurnRef,
    pub machine_id: String,
    pub cwd: Option<String>,
    pub role: Role,
    pub excerpt: String,
    /// `Some` only from a ranked provider.
    pub rank: Option<f64>,
    /// A subagent said this, not the operator (`Provenance::sidechain`).
    pub sidechain: bool,
    /// Whether the matched turn is still live context. A hit is a coordinate
    /// plus evidence; "this evidence was compacted out of context" changes what
    /// the caller does with it, so it travels WITH the hit rather than costing
    /// a second read to find out.
    pub surface: crate::turn::Surface,
}

/// The public search result. Constructible only via `redacted_from`.
#[derive(Debug, Clone)]
pub struct Hit {
    pub at: TurnRef,
    pub machine_id: String,
    pub cwd: Option<String>,
    pub role: Role,
    excerpt: String,
    pub rank: Option<f64>,
    pub sidechain: bool,
    pub surface: crate::turn::Surface,
}

impl Hit {
    /// The single funnel every backend's output passes through (LLD §16 rule 2).
    /// `scrub` is injected rather than imported so `tp-core` stays dependency-free.
    pub fn redacted_from(raw: RawHit, scrub: &dyn Fn(&str) -> String) -> Self {
        Self {
            excerpt: scrub(&raw.excerpt),
            at: raw.at,
            machine_id: raw.machine_id,
            cwd: raw.cwd,
            role: raw.role,
            rank: raw.rank,
            sidechain: raw.sidechain,
            surface: raw.surface,
        }
    }

    pub fn excerpt(&self) -> &str {
        &self.excerpt
    }
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub runtime_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub last_turn_at: Option<i64>,
    /// `None` from a scan provider — counting turns means parsing the whole file.
    pub turn_count: Option<i64>,
}

/// Where to read from in a session. Time-based so both backends agree.
#[derive(Debug, Clone, Copy)]
pub enum TurnCursor {
    Start,
    AfterTs(i64),
    /// A time WINDOW, kept newest-first when it doesn't fit.
    ///
    /// `AfterTs` pages FORWARD from a point and, on overflow, keeps the OLDEST
    /// turns — correct for "replay this session from the beginning", which is
    /// what it was built for. It is the wrong shape for the far more common
    /// question, "what happened in the last few hours": that read starts at now
    /// and wants the NEWEST turns kept when it overflows.
    ///
    /// RestWalker's `getRawConversation` had this (`windowMs` + an exclusive
    /// `before` for paging backwards) and teleport lost it in the rewrite, which
    /// is why reading four hours back took a session id, hand-computed epoch
    /// milliseconds, and one call per session.
    Window {
        /// Inclusive lower bound.
        since_ms: i64,
        /// Exclusive upper bound; `None` = now. Paging backwards means passing
        /// the earliest `ts` of the previous page, and exclusivity is what keeps
        /// that turn from being returned twice.
        before_ms: Option<i64>,
    },
}

impl TurnCursor {
    /// Whether a turn's timestamp falls in this cursor's range.
    ///
    /// One definition for both providers: the scan side filters raw lines and
    /// the index side filters rows, and they used to spell the same comparison
    /// separately.
    pub fn admits_ts(&self, ts: Option<i64>) -> bool {
        match (self, ts) {
            (TurnCursor::Start, _) => true,
            // A turn with no parseable timestamp can't be placed in a window, and
            // guessing would put it in every page. Only the unbounded read keeps it.
            (_, None) => false,
            (TurnCursor::AfterTs(cut), Some(ts)) => ts > *cut,
            (
                TurnCursor::Window {
                    since_ms,
                    before_ms,
                },
                Some(ts),
            ) => ts >= *since_ms && before_ms.is_none_or(|b| ts < b),
        }
    }
}

/// Accumulates a windowed read, dropping the OLDEST turns when it overflows.
///
/// The counterpart to `admit_turn`, and it exists for the same reason: the two
/// providers must cut in exactly the same place, so the rule lives here rather
/// than being spelled twice. Memory stays bounded — turns are evicted as they
/// go over, so a 30-day window over a huge session never materializes.
pub struct WindowBuffer {
    out: std::collections::VecDeque<crate::turn::NormalizedTurn>,
    used: usize,
    limit: usize,
    budget: usize,
    dropped: bool,
}

impl WindowBuffer {
    pub fn new(limit: usize, budget: usize) -> Self {
        Self {
            out: std::collections::VecDeque::new(),
            used: 0,
            limit,
            budget,
            dropped: false,
        }
    }

    pub fn push(&mut self, mut turn: crate::turn::NormalizedTurn) {
        truncate_field(&mut turn.text, "… [turn truncated]");
        truncate_field(&mut turn.thinking, "… [thinking truncated]");
        self.used += turn.text.len() + turn.thinking.len();
        self.out.push_back(turn);
        // `len() > 1` for the same reason `admit_turn` admits an oversized first
        // turn: returning nothing would read as "this window is empty".
        while (self.out.len() > self.limit || self.used > self.budget) && self.out.len() > 1 {
            if let Some(old) = self.out.pop_front() {
                self.used -= old.text.len() + old.thinking.len();
                self.dropped = true;
            }
        }
    }

    /// `(turns oldest-first, dropped anything)`.
    pub fn finish(self) -> (Vec<crate::turn::NormalizedTurn>, bool) {
        (self.out.into(), self.dropped)
    }
}

/// Byte ceiling for one `turns` call. `limit` bounds the number of turns, which
/// bounds nothing that matters: a turn's size is whatever the other session
/// happened to write, so 200 turns is anywhere from a few hundred bytes to more
/// context than the caller has. Measured on a real session, `limit=200` returned
/// ~3.7k tokens against ~13 for the equivalent `search` — and the caller had no
/// way to know that in advance.
///
/// ~40 KB ≈ 10k tokens: enough to carry a working conversation over (the point
/// of `turns`), bounded enough that one call can't evict the caller's context.
pub const DEFAULT_TURN_BUDGET_BYTES: usize = 40_000;

/// Per-turn ceiling, so a single enormous message can't consume the whole
/// budget by itself and starve the turns around it — which is what makes a
/// conversation readable rather than one wall of text.
pub const MAX_TURN_BYTES: usize = 4_000;

fn truncate_field(s: &mut String, marker: &str) {
    if s.len() <= MAX_TURN_BYTES {
        return;
    }
    let mut cut = MAX_TURN_BYTES;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(marker);
}

/// Trim a turn to `MAX_TURN_BYTES` per field and charge it against `used`.
/// Returns `false` when the budget is spent and the caller must stop reading.
///
/// Lives here, not in either provider, so scan and index cannot drift on where
/// they cut — the conformance suite asserts they agree, and sharing the
/// primitive makes that true by construction rather than by discipline.
pub fn admit_turn(
    out: &mut Vec<crate::turn::NormalizedTurn>,
    mut turn: crate::turn::NormalizedTurn,
    used: &mut usize,
    budget: usize,
) -> bool {
    truncate_field(&mut turn.text, "… [turn truncated]");
    truncate_field(&mut turn.thinking, "… [thinking truncated]");
    let cost = turn.text.len() + turn.thinking.len();
    // Never return an empty result just because the FIRST turn is oversized:
    // it has already been capped, so admitting it is bounded, and returning
    // nothing would look like "this session is empty".
    if !out.is_empty() && *used + cost > budget {
        return false;
    }
    *used += cost;
    out.push(turn);
    true
}

pub trait RetrievalProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn search(&self, q: &Query, scope: &Scope) -> anyhow::Result<Retrieved<RawHit>>;
    fn sessions(&self, scope: &Scope, limit: usize) -> anyhow::Result<Retrieved<SessionRow>>;
    /// Stops at `limit` turns OR `budget_bytes`, whichever comes first, and
    /// reports which via `Coverage::truncated`. A truncated read is resumable:
    /// the caller passes the last returned turn's `ts` back as
    /// `TurnCursor::AfterTs` (LLD §16 rule 1 — the cursor is `ts`, never `seq`).
    fn turns(
        &self,
        session: &SessionId,
        at: TurnCursor,
        include_thinking: bool,
        limit: usize,
        budget_bytes: usize,
    ) -> anyhow::Result<Retrieved<crate::turn::NormalizedTurn>>;
}
