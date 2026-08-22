//! On-demand scan provider — the shipping default (LLD §16).
//!
//! Ports the three-layer filter RestWalker's `node/teleport.ts:searchConversations`
//! has run in production:
//!   1. mtime window prunes the file set before anything is opened
//!   2. raw-substring test on the un-parsed line (the fast path — no JSON parse)
//!   3. `Adapter::parse_line` only on lines that survived (2)
//!
//! Layer 2 is what makes this viable: a full JSON parse of every line of a
//! 2.4 GB corpus is what you are avoiding. A regex query cannot use it (there
//! is no cheap prefilter), so regex queries parse every line in the window —
//! bounded by layer 1, and reported via `Coverage`.

use anyhow::Result;
use regex::Regex;
use std::path::PathBuf;
use tp_core::retrieval::{
    Capabilities, Coverage, Query, RawHit, RetrievalProvider, Retrieved, Scope, SessionRow,
    TurnCursor, TurnRef,
};
use tp_core::turn::NormalizedTurn;
use tp_core::SessionId;
use tp_ingest::adapter::{Adapter, SourceFile};

/// Cap on files opened per query. Exceeding it sets `Coverage::degraded`
/// rather than silently returning a partial answer.
const SCAN_FILE_BUDGET: usize = 5_000;
const EXCERPT_RADIUS: usize = 60;

pub struct ScanProvider {
    machine_id: String,
    adapters: Vec<Box<dyn Adapter>>,
    roots: Vec<(String, PathBuf)>,
}

impl ScanProvider {
    pub fn new(
        machine_id: impl Into<String>,
        adapters: Vec<Box<dyn Adapter>>,
        roots: Vec<(String, PathBuf)>,
    ) -> Self {
        Self {
            machine_id: machine_id.into(),
            adapters,
            roots,
        }
    }

    fn adapter_for(&self, runtime_id: &str) -> Option<&dyn Adapter> {
        self.adapters
            .iter()
            .find(|a| a.id() == runtime_id)
            .map(|a| a.as_ref())
    }

    /// Layer 1: prune by mtime + runtime + folder before opening anything.
    ///
    /// Only the LOWER bound can prune here. `mtime` is the last write, so a file
    /// older than `since` cannot hold a turn inside the window — but a session
    /// that was active on the 4th and again today has today's mtime, and pruning
    /// it against an upper bound would drop exactly the file being asked for.
    /// The upper bound is therefore applied per-turn, not per-file.
    fn candidates(&self, scope: &Scope) -> Result<Vec<(String, SourceFile)>> {
        let since_ms = tp_core::now_ms().saturating_sub(scope.since.as_millis() as i64);
        let folder = scope.folder_needle();
        let mut out = Vec::new();
        for (runtime_id, root) in &self.roots {
            if !scope.runtimes.is_empty() && !scope.runtimes.iter().any(|r| r == runtime_id) {
                continue;
            }
            let Some(adapter) = self.adapter_for(runtime_id) else {
                continue;
            };
            for src in adapter.discover(root)? {
                if src.mtime_ms < since_ms {
                    continue;
                }
                if let Some(folder) = &folder {
                    if !path_matches_folder(&src.path, folder) {
                        continue;
                    }
                }
                out.push((runtime_id.clone(), src));
            }
        }
        // Most-recent first: with a limit, the useful hits come back first.
        out.sort_by_key(|(_, src)| std::cmp::Reverse(src.mtime_ms));
        Ok(out)
    }
}

/// Every separator-ish character collapsed to a single `-`, lowercased.
///
/// Both runtimes encode a session's cwd into its transcript DIRECTORY name by
/// replacing the separators — `/Users/me/dev/teleport` becomes
/// `-Users-me-dev-teleport` (Claude Code) or `--Users-me-dev--` (pi). Comparing
/// a user-supplied path against that raw string can never match, so normalizing
/// both sides is what lets a real path be used as the needle.
fn folder_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.trim().to_lowercase().chars() {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out
}

/// Folder match: case-insensitive substring, against the transcript path both
/// literally and separator-normalized.
///
/// The literal comparison alone was a silent-empty-result bug. This layer prunes
/// on the file PATH — it runs before any file is opened, which is the point — but
/// the path is the ENCODED form of the cwd, while `tp sessions` DISPLAYS the real
/// cwd read from the records. So the most natural input, pasting back the path
/// the tool just printed, matched nothing and reported "no sessions", which reads
/// as "you never worked there" rather than "the filter didn't understand you".
///
/// It was also a provider divergence, the thing the conformance suite exists to
/// prevent: the index filters `session.cwd`, the real path, so the SAME query
/// answered correctly there and emptily here. Every conformance case passed
/// `folder: None`, so nothing ever compared them.
fn path_matches_folder(path: &std::path::Path, folder: &str) -> bool {
    let hay = path.to_string_lossy().to_lowercase();
    let needle = folder.trim().to_lowercase();
    // Kept so anything that matched before still matches — this widens, never narrows.
    hay.contains(&needle) || folder_slug(&hay).contains(&folder_slug(&needle))
}

/// Case-insensitive substring search returning a byte offset **into `hay`**.
///
/// `hay.to_lowercase().find(..)` is NOT usable for this: `to_lowercase` is not
/// byte-length preserving (U+212A 'K' 3→1 bytes, U+0130 'İ' 2→3), so its index
/// can land mid-codepoint in the original string and panic on slicing.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let needle_lower = needle.to_lowercase();
    // How much of `hay` can possibly be consumed by the needle, in CHARACTERS.
    // Lowercasing never turns several characters into one, so the needle cannot
    // span more than this many of them. One extra is taken because
    // `str::to_lowercase` is context-sensitive — a Greek Σ lowercases to ς only
    // when it is word-final — so the character after the span has to be present
    // for that decision to come out the same as it would on the whole string.
    let span = needle_lower.chars().count() + 1;

    // Walk real char boundaries of `hay`, lowercasing only that bounded span at
    // each one. The offset stays anchored to `hay`, which is what the caller
    // slices with.
    //
    // This used to lowercase `hay[*i..]` — the ENTIRE remaining suffix — at
    // every position, which is quadratic and was measured as exactly that:
    // 10 KB 1.7ms, 20 KB 6.2ms, 40 KB 25.1ms, 100 KB 149.6ms, 200 KB 602.3ms
    // for a single non-matching field. `matches()` calls this on `text`, then
    // `thinking`, then every tool name, for every turn whose raw line contained
    // the needle, and the search path applies no per-field size cap — so one
    // turn holding a pasted build log cost most of a second by itself.
    hay.char_indices()
        .find(|(i, _)| {
            let rest = &hay[*i..];
            let end = rest
                .char_indices()
                .nth(span)
                .map_or(rest.len(), |(byte, _)| byte);
            rest[..end].to_lowercase().starts_with(&needle_lower)
        })
        .map(|(i, _)| i)
}

fn excerpt_around(text: &str, query: &str, re: Option<&Regex>) -> String {
    let found = match re {
        Some(r) => r.find(text).map(|m| m.start()),
        None => find_ci(text, query),
    };
    let Some(idx) = found else {
        return text.chars().take(EXCERPT_RADIUS * 2).collect();
    };
    // `idx` is a real boundary of `text`, so these slices are safe.
    let start = text[..idx]
        .char_indices()
        .rev()
        .nth(EXCERPT_RADIUS)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = text[idx..]
        .char_indices()
        .nth(EXCERPT_RADIUS * 2)
        .map(|(i, _)| idx + i)
        .unwrap_or(text.len());
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.push_str(text[start..end].trim());
    if end < text.len() {
        s.push('…');
    }
    s
}

fn matches(hay: &str, query: &str, re: Option<&Regex>) -> bool {
    match re {
        Some(r) => r.is_match(hay),
        // Same predicate as `find_ci`, so "matched" and "can locate it for the
        // excerpt" never disagree.
        None => find_ci(hay, query).is_some(),
    }
}

impl RetrievalProvider for ScanProvider {
    fn name(&self) -> &'static str {
        "scan"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            ranked: false,      // file order, most-recent-first; no bm25 without an index
            unscoped_ok: false, // an unscoped scan is the 27 s case — caller must be warned
            search_thinking: true,
            regex: true, // evaluated per-line against the real text
        }
    }

    fn search(&self, q: &Query, scope: &Scope) -> Result<Retrieved<RawHit>> {
        let re = if q.regex {
            Some(Regex::new(&q.text)?)
        } else {
            None
        };
        let candidates = self.candidates(scope)?;
        let window = scope.range_ms(tp_core::now_ms());
        let mut coverage = Coverage::default();
        if candidates.len() > SCAN_FILE_BUDGET {
            coverage.degraded = Some(format!(
                "scan budget: {} of {} candidate files examined",
                SCAN_FILE_BUDGET,
                candidates.len()
            ));
        }

        let mut hits = Vec::new();
        'outer: for (runtime_id, src) in candidates.iter().take(SCAN_FILE_BUDGET) {
            let Some(adapter) = self.adapter_for(runtime_id) else {
                continue;
            };
            let Ok(content) = std::fs::read_to_string(&src.path) else {
                continue;
            };
            coverage.sessions_scanned += 1;

            let session_id =
                SessionId::new(&self.machine_id, runtime_id, &src.native_id).to_string();
            let mut cwd: Option<String> = None;
            // Hits from THIS file, held back until the file has been fully
            // seen — their `surface` cannot be known any earlier.
            let mut file_hits: Vec<(RawHit, Option<String>)> = Vec::new();

            for line in content.lines() {
                if line.is_empty() {
                    continue;
                }
                // Layer 2 — the fast path. Only valid for substring queries:
                // if the raw line doesn't contain the needle anywhere, no
                // parsed field of it can either.
                if re.is_none() && !line.to_lowercase().contains(&q.text.to_lowercase()) {
                    continue;
                }
                if cwd.is_none() {
                    cwd = adapter.cwd_of_line(line);
                }
                // Layer 3 — parse only survivors, via the SHARED primitive so
                // scan and index never drift on format quirks.
                let Some(turn) = adapter.parse_line(line) else {
                    continue;
                };
                // Per-TURN window. The mtime prune is per file, so a long-lived
                // session survives it and would otherwise contribute hits from
                // outside the window entirely.
                if !in_window(turn.ts, window) {
                    continue;
                }

                let mut hit_field: Option<String> = None;
                if matches(&turn.text, &q.text, re.as_ref()) {
                    hit_field = Some(turn.text.clone());
                } else if q.include_thinking && matches(&turn.thinking, &q.text, re.as_ref()) {
                    hit_field = Some(turn.thinking.clone());
                } else if let Some(tc) = turn
                    .tool_calls
                    .iter()
                    .find(|t| matches(&t.name, &q.text, re.as_ref()))
                {
                    hit_field = Some(format!("[tool] {}", tc.name));
                }

                // Scrub the WHOLE field before cutting the excerpt. Several
                // redaction rules are anchored on a closing delimiter (the
                // `"` of "accessToken":"…", the `-----END` of a private key);
                // truncating first strands the opener outside the window and
                // the rule silently no-ops, emitting raw credential material.
                // `Retrieval` scrubs again downstream — this is the pass that
                // has to see the full text.
                let hit_field = hit_field.map(|f| tp_ingest::redact::scrub(&f));

                if let Some(field) = hit_field {
                    file_hits.push((
                        RawHit {
                            at: TurnRef {
                                session_id: session_id.clone(),
                                ts: turn.ts,
                                seq: None,
                            },
                            machine_id: self.machine_id.clone(),
                            cwd: cwd.clone(),
                            role: turn.role,
                            excerpt: excerpt_around(&field, &q.text, re.as_ref()),
                            rank: None,
                            sidechain: turn.prov.sidechain,
                            // Resolved below, once the whole file has been seen.
                            surface: tp_core::turn::Surface::Unknown,
                        },
                        turn.prov.uuid.clone(),
                    ));
                    if hits.len() + file_hits.len() >= q.limit {
                        coverage.truncated = true;
                        break;
                    }
                }
            }

            // Resolve `surface` for this file's hits — and ONLY for files that
            // produced hits, so the fast path stays fast. It cannot be answered
            // inside the line loop twice over: a compaction marker is not a
            // turn (it flows past `parse_line`), and a marker near the END of
            // the file supersedes turns already matched near the start — the
            // answer for line 10 depends on line 400. So hit-bearing files get
            // the same whole-file parse ingest does, folded through the same
            // `apply_compaction` the turns path uses, and each hit is matched
            // back by uuid, falling back to timestamp for runtimes without one.
            // At most `q.limit` files ever pay this.
            if !file_hits.is_empty() {
                if let Ok(mut chunk) = adapter.parse_from(&src.path, 0) {
                    tp_core::turn::apply_compaction(
                        &mut chunk.turns,
                        &chunk.compaction,
                        chunk.tracks_compaction,
                    );
                    for (hit, uuid) in &mut file_hits {
                        let found = match uuid {
                            Some(u) => chunk
                                .turns
                                .iter()
                                .find(|t| t.prov.uuid.as_deref() == Some(u.as_str())),
                            None => chunk.turns.iter().find(|t| t.ts == hit.at.ts),
                        };
                        if let Some(t) = found {
                            hit.surface = t.surface;
                        }
                        // Unmatched stays Unknown — never promoted to Current.
                    }
                }
                hits.extend(file_hits.into_iter().map(|(h, _)| h));
            }
            if coverage.truncated {
                break 'outer;
            }
        }
        Ok(Retrieved::new(hits, coverage))
    }

    fn sessions(&self, scope: &Scope, limit: usize) -> Result<Retrieved<SessionRow>> {
        let candidates = self.candidates(scope)?;
        let window = scope.range_ms(tp_core::now_ms());
        let mut coverage = Coverage::default();
        let mut rows = Vec::new();
        let mut examined = 0usize;
        for (runtime_id, src) in candidates.iter() {
            if rows.len() >= limit {
                break;
            }
            let Some(adapter) = self.adapter_for(runtime_id) else {
                continue;
            };
            examined += 1;
            coverage.sessions_scanned += 1;

            // mtime is the FILE's last write, not its last turn's timestamp, and
            // the index answers this from turn timestamps. Using mtime here made
            // "active in the window" mean two different things per backend —
            // CI caught exactly that, on the lower bound, where the fixture's
            // turns are seconds older than the file that was just written.
            //
            // Affordable because the loop stops at `limit`: candidates arrive
            // mtime-ordered, so this parses on the order of `limit` files, not
            // the whole corpus.
            let Some(last_turn_at) = last_ts_in_window(adapter, &src.path, window) else {
                continue; // no turns in the window — not an active session for this query
            };
            let last_turn_at = Some(last_turn_at);

            // Read only enough to recover cwd + title; never the whole file.
            let (cwd, title) = head_meta(adapter, &src.path);
            rows.push(SessionRow {
                id: SessionId::new(&self.machine_id, runtime_id, &src.native_id).to_string(),
                runtime_id: runtime_id.clone(),
                cwd,
                title,
                last_turn_at,
                turn_count: None, // counting means parsing the whole file — index-only
            });
        }
        // Most-recently-active first is the contract; a windowed read reorders
        // because mtime is no longer the sort key.
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_turn_at));
        coverage.truncated = candidates.len() > examined;
        Ok(Retrieved::new(rows, coverage))
    }

    fn turns(
        &self,
        session: &SessionId,
        at: TurnCursor,
        _include_thinking: bool,
        limit: usize,
        budget_bytes: usize,
    ) -> Result<Retrieved<NormalizedTurn>> {
        let empty = || Retrieved {
            items: Vec::new(),
            coverage: Coverage {
                sessions_scanned: 0,
                truncated: false,
                degraded: None,
            },
        };
        // "I looked and there is nothing" and "I cannot look here at all" are
        // the same empty vector and must not be the same ANSWER. Three returns
        // below produced an undifferentiated `empty()`, and the middle one is
        // reachable for an entire runtime: a push-ingested harness (dsh calls
        // `tp ingest`; nothing writes a transcript) has no file to read and no
        // `source_path` to notice is missing, so `unscannable_note` — which
        // exists for this exact class — cannot see it either, its first
        // condition being `source_path IS NOT NULL`.
        //
        // Measured before this was written: `tp turns <dsh-session>` said "no
        // turns found" for a session holding 50 turns that `--index` printed
        // without complaint. That is a fact about the PROVIDER rendered as a
        // fact about the CORPUS (LLD §16 rule 3), and a bug rather than a
        // declared capability, because `Capabilities` has no flag able to say
        // "this backend cannot see that runtime".
        let unreadable = |why: String| Retrieved {
            items: Vec::new(),
            coverage: Coverage {
                sessions_scanned: 0,
                truncated: false,
                degraded: Some(why),
            },
        };
        let Some(adapter) = self.adapter_for(&session.runtime_id) else {
            return Ok(unreadable(format!(
                "runtime '{}' has no adapter here, so a scan cannot read it — \
                 this is not an empty session. If the index holds it, --index \
                 will show it.",
                session.runtime_id
            )));
        };
        let Some((_, root)) = self.roots.iter().find(|(r, _)| *r == session.runtime_id) else {
            return Ok(unreadable(format!(
                "runtime '{}' writes no transcript for a scan to read (its turns \
                 arrive through `tp ingest`), so this answer is empty for a \
                 reason that has nothing to do with the session: re-run with \
                 --index.",
                session.runtime_id
            )));
        };
        // `locate`, not `discover().find()`: reading one session must not cost
        // an enumeration of every session on the machine.
        let Some(src) = adapter.locate(root, &session.native_id)? else {
            return Ok(empty());
        };
        // `parse_from`, not a per-line `parse_line` loop. The line loop parsed
        // each turn correctly and could never answer `surface`: a compaction
        // marker is not a turn, so it flowed past `parse_line` unseen, and every
        // turn came back without the one fact the index provider stores per row.
        // Parsing the whole session first costs nothing new — ingest already
        // does exactly this over the same file — and hands over the boundary
        // list plus `tracks_compaction`, which `apply_compaction` folds into a
        // per-turn answer the writer would have computed in SQL. Conformance
        // pins the two turn by turn.
        let mut chunk = adapter.parse_from(&src.path, 0)?;
        tp_core::turn::apply_compaction(
            &mut chunk.turns,
            &chunk.compaction,
            chunk.tracks_compaction,
        );
        // A windowed read keeps the NEWEST turns, so it cannot stop early — the
        // last turn of the file is the one most likely to be kept. A forward
        // read keeps the oldest and stops as soon as the budget is spent.
        let (out, truncated) = if let TurnCursor::Window { .. } = at {
            let mut buf = tp_core::retrieval::WindowBuffer::new(limit, budget_bytes);
            for turn in chunk.turns {
                if !at.admits_ts(turn.ts) {
                    continue;
                }
                buf.push(turn);
            }
            buf.finish()
        } else {
            let mut out = Vec::new();
            let mut used = 0usize;
            let mut truncated = false;
            for turn in chunk.turns {
                if !at.admits_ts(turn.ts) {
                    continue;
                }
                // Budget first: `admit_turn` caps the turn and reports whether it
                // fit.
                if !tp_core::retrieval::admit_turn(&mut out, turn, &mut used, budget_bytes) {
                    truncated = true;
                    break;
                }
                if out.len() >= limit {
                    truncated = true;
                    break;
                }
            }
            (out, truncated)
        };
        Ok(Retrieved {
            items: out,
            coverage: Coverage {
                sessions_scanned: 1,
                truncated,
                degraded: None,
            },
        })
    }
}

/// `true` when `ts` falls inside `(since, until)`; `until` is exclusive.
///
/// A turn with no timestamp cannot be placed in a window. It is kept only when
/// there is no upper bound, matching the old behaviour where `since` alone was
/// enforced per FILE and such turns came back with everything else.
fn in_window(ts: Option<i64>, (since, until): (i64, Option<i64>)) -> bool {
    match ts {
        Some(ts) => ts >= since && until.is_none_or(|u| ts < u),
        None => until.is_none(),
    }
}

/// The latest turn timestamp inside the window, or `None` if the session has no
/// turns there at all. Costs a full parse, which is why it runs only when an
/// upper bound makes mtime an unusable proxy.
fn last_ts_in_window(
    adapter: &dyn Adapter,
    path: &std::path::Path,
    window: (i64, Option<i64>),
) -> Option<i64> {
    let content = std::fs::read_to_string(path).ok()?;
    content
        .lines()
        .filter_map(|l| adapter.parse_line(l))
        .filter(|t| in_window(t.ts, window))
        .filter_map(|t| t.ts)
        .max()
}

/// First `cwd` and first user-turn text, reading only the head of the file.
/// `(cwd, title)` from the head of a transcript.
///
/// Two different kinds of title can appear, and they are NOT interchangeable:
///
///   * one the runtime STATES, as its own entry (`adapter::title_of_line`);
///   * teleport's fallback, the first user message truncated.
///
/// The stated one wins, and it is not bounded by the head window the way the
/// fallback is — a `/rename` happens whenever the user runs it, so the whole
/// file is scanned for one. The index side does the same in
/// `jsonl::read_chunk`; the two backends printing different titles for one
/// session is the split LLD §16 forbids, and it happened for exactly as long as
/// only the index knew how to read them.
fn head_meta(adapter: &dyn Adapter, path: &std::path::Path) -> (Option<String>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let mut cwd = None;
    let mut derived: Option<String> = None;
    let mut stated: Option<(tp_core::turn::TitleSource, String)> = None;

    for (i, line) in content.lines().enumerate() {
        // A stated title may be appended at any point, so this pass is not
        // bounded — but it is the only reason to keep reading past the head.
        if let Some((source, t)) = adapter.title_of_line(line) {
            let better = match (&stated, source) {
                // A person's title outranks a model's, whatever the order they
                // were appended in — Claude Code's observed behaviour, which
                // is not a documented contract.
                (Some((tp_core::turn::TitleSource::User, _)), tp_core::turn::TitleSource::Ai) => {
                    false
                }
                // Otherwise the LAST statement wins: a rename replaces itself.
                _ => true,
            };
            if better && !t.trim().is_empty() {
                stated = Some((source, tp_ingest::redact::scrub(&t)));
            }
        }

        if i >= HEAD_LINES {
            continue;
        }
        if cwd.is_none() {
            cwd = adapter.cwd_of_line(line);
        }
        if derived.is_none() {
            if let Some(t) = adapter.parse_line(line) {
                if matches!(t.role, tp_core::turn::Role::User) && !t.text.is_empty() {
                    // Scrub before truncating — same delimiter-anchored
                    // rule hazard as the search excerpt above.
                    derived = Some(
                        tp_ingest::redact::scrub(&t.text)
                            .chars()
                            .take(tp_ingest::adapter::jsonl::TITLE_CHARS)
                            .collect(),
                    );
                }
            }
        }
    }

    (cwd, stated.map(|(_, t)| t).or(derived))
}

/// How far into a file the CWD and the fallback title are looked for. Both come
/// from the opening of a session by definition, so reading further would cost a
/// full file walk to find something that is either in the head or absent.
const HEAD_LINES: usize = 200;

#[cfg(test)]
mod find_ci_tests {
    use super::find_ci;

    /// `find_ci` used to lowercase the whole remaining suffix at every char
    /// boundary — quadratic, measured at 10 KB 1.7ms / 40 KB 25.1ms / 200 KB
    /// 602.3ms for one non-matching field. The lazy version must agree with the
    /// old definition EXACTLY, because the caller slices `hay` at the offset it
    /// returns and case folding can change a character's byte length.
    #[test]
    fn find_ci_agrees_with_the_naive_definition() {
        fn naive(hay: &str, needle: &str) -> Option<usize> {
            if needle.is_empty() {
                return Some(0);
            }
            let n = needle.to_lowercase();
            hay.char_indices()
                .find(|(i, _)| hay[*i..].to_lowercase().starts_with(&n))
                .map(|(i, _)| i)
        }
        for (hay, needle) in [
            ("hello world", "WORLD"),
            ("HELLO world", "hello"),
            ("", "x"),
            ("abc", ""),
            ("abc", "abcd"),
            ("aaa", "aa"),
            ("→ → → teleport ←", "TELEPORT"),
            ("STRASSE", "strasse"),
            ("İstanbul", "istanbul"),
            ("no match here", "zzq"),
            ("émile ÉMILE", "ÉMILE"),
            ("ΣΟΦΟΣ", "σοφος"),
        ] {
            assert_eq!(
                find_ci(hay, needle),
                naive(hay, needle),
                "find_ci disagreed on ({hay:?}, {needle:?})"
            );
            if let Some(i) = find_ci(hay, needle) {
                assert!(hay.is_char_boundary(i), "offset {i} is not a char boundary");
            }
        }
    }

    /// The property the rewrite exists for: doubling the input must not
    /// quadruple the work. Generous bound — the point is to catch a return to
    /// quadratic, not to pin a number to this machine.
    #[test]
    fn a_non_matching_field_does_not_cost_quadratic_time() {
        let make = |kb: usize| -> String { "build log line noise ".repeat(kb * 1024 / 21) };
        let time = |s: &str| {
            let t = std::time::Instant::now();
            assert_eq!(find_ci(s, "zzq-no-such-token"), None);
            t.elapsed()
        };
        let small = time(&make(50));
        let big = time(&make(200)); // 4x the input
        assert!(
            big < small * 20,
            "4x input cost {big:?} vs {small:?} — that is the quadratic shape returning"
        );
    }
}
