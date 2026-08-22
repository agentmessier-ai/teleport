//! Index provider — the opt-in accelerator (LLD §16). Wraps the P0 SQLite +
//! FTS5 store behind the same `RetrievalProvider` trait the scan backend
//! implements, so switching is a config flag rather than a rewrite.

use anyhow::Result;
use std::path::PathBuf;
use tp_core::retrieval::{
    Capabilities, Coverage, Query, RawHit, RetrievalProvider, Retrieved, Scope, SessionRow,
    TurnCursor, TurnRef,
};
use tp_core::turn::{NormalizedTurn, Role};
use tp_core::SessionId;
use tp_db::Db;

pub struct IndexProvider {
    db_path: PathBuf,
}

/// What a query needs to know about the index before trusting its answer
/// (LLD §16 rule 3: "not found" must be distinguishable from "not scanned").
#[derive(Debug, Clone, Default)]
pub struct IndexStatus {
    pub exists: bool,
    pub turn_count: i64,
    /// Unix ms of the most recent ingest_state write, if any.
    pub last_ingest_at: Option<i64>,
}

impl IndexProvider {
    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }
    fn open(&self) -> Result<Db> {
        Db::open(&self.db_path)
    }

    /// The archive beside this index, if `tp archive` has made one.
    ///
    /// Every read consults it, and that is not a convenience. Splitting the
    /// corpus into two files and asking the caller to remember the second one
    /// recreates, exactly, the failure this provider spent the day fixing: an
    /// answer that reads as "never discussed" when it means "not in the half
    /// you asked". The archive is a sibling file with the same schema, so the
    /// same query runs against it unchanged.
    ///
    /// `None` when there is none, and when THIS provider is already reading one
    /// (`TP_DB=…/archive.db`), which would otherwise search it twice.
    fn archive(&self) -> Option<Db> {
        let path = self.db_path.with_file_name("archive.db");
        if path == self.db_path || !path.exists() {
            return None;
        }
        Db::open(&path).ok()
    }

    /// Inspect the index without assuming it exists. `Db::open` auto-creates
    /// the file, so existence must be checked before opening.
    pub fn status(&self) -> IndexStatus {
        if !self.db_path.exists() {
            return IndexStatus::default();
        }
        let Ok(db) = self.open() else {
            return IndexStatus::default();
        };
        let conn = db.conn();
        let turn_count = conn
            .query_row("SELECT count(*) FROM turn", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0);
        let last_ingest_at = conn
            .query_row("SELECT max(mtime_ms) FROM ingest_state", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .unwrap_or(None);
        IndexStatus {
            exists: true,
            turn_count,
            last_ingest_at,
        }
    }

    /// The freshness signal to fold into `Coverage::degraded`. A miss is
    /// reported explicitly rather than silently masked as "no matches".
    fn freshness_note(&self) -> Option<String> {
        let st = self.status();
        if !st.exists {
            return Some("index does not exist — run `tp index` to build it".to_string());
        }
        if st.turn_count == 0 {
            return Some("index is empty — run `tp index` to populate it".to_string());
        }
        if let Some(last) = st.last_ingest_at {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let lag_s = (now - last).max(0) / 1000;
            if lag_s > 3600 {
                let mins = lag_s / 60;
                return Some(format!(
                    "index last updated ~{mins} min ago — run `tp index` to refresh"
                ));
            }
        }
        None
    }
}

fn role_from_str(s: &str) -> Role {
    match s {
        "user" => Role::User,
        _ => Role::Assistant,
    }
}

impl RetrievalProvider for IndexProvider {
    fn name(&self) -> &'static str {
        "index"
    }

    fn capabilities(&self) -> Capabilities {
        // regex: false — FTS5 matches tokenized phrases, not regexes.
        Capabilities {
            ranked: true,
            unscoped_ok: true,
            search_thinking: true,
            regex: false,
        }
    }

    fn search(&self, q: &Query, scope: &Scope) -> Result<Retrieved<RawHit>> {
        // Freshness first: `Db::open` auto-creates the file, so checking after
        // opening would mask "does not exist" as "empty".
        let mut coverage = Coverage {
            degraded: self.freshness_note(),
            ..Default::default()
        };
        let db = self.open()?;
        let (since_ms, until_ms) = scope.range_ms(tp_core::now_ms());
        // The window AND the folder go INTO the query. Applying either to the
        // result set here would mean `LIMIT` had already been spent on rows the
        // caller excluded.
        //
        // The folder used to be dropped on this line while `sessions()` below
        // applied it correctly — the same provider disagreeing with itself. The
        // effect was worse than an error: `--index --folder anything-at-all`,
        // including a directory that does not exist, returned the full unfiltered
        // result set, which reads as "I searched only there, and this is all
        // there is." Reported from a real session that lost several rounds to it,
        // and reached by following teleport's OWN advice — the scan provider
        // suggests `--folder` and building an index in the same breath.
        let mut hits = tp_db::query::search(
            db.conn(),
            &q.text,
            q.include_thinking,
            q.limit as i64,
            Some(since_ms),
            until_ms,
            scope.folder_needle().as_deref(),
        )?;
        // The archive answers the same query. Both sides rank with bm25 over the
        // same schema, so the scores are comparable enough to interleave; the
        // merged list is re-sorted and re-cut to `limit` so the caller gets the
        // best `limit` of BOTH halves rather than the best of one plus whatever
        // the other happened to add.
        if let Some(arch) = self.archive() {
            hits.extend(tp_db::query::search(
                arch.conn(),
                &q.text,
                q.include_thinking,
                q.limit as i64,
                Some(since_ms),
                until_ms,
                scope.folder_needle().as_deref(),
            )?);
            hits.sort_by(|a, b| a.rank.total_cmp(&b.rank));
            hits.truncate(q.limit);
        }
        let mut out = Vec::new();
        for h in hits {
            out.push(RawHit {
                at: TurnRef {
                    session_id: h.session_id,
                    ts: h.ts,
                    seq: Some(h.seq),
                },
                machine_id: h.machine_id,
                cwd: None,
                role: role_from_str(&h.role),
                excerpt: h.snippet,
                rank: Some(h.rank),
                sidechain: h.sidechain,
                surface: h.surface,
            });
        }
        coverage.truncated = out.len() >= q.limit;
        Ok(Retrieved::new(out, coverage))
    }

    fn sessions(&self, scope: &Scope, limit: usize) -> Result<Retrieved<SessionRow>> {
        let degraded = self.freshness_note();
        let db = self.open()?;
        let folder = scope.folder_needle();
        let (since_ms, until_ms) = scope.range_ms(tp_core::now_ms());
        let mut rows = tp_db::query::list_sessions(
            db.conn(),
            folder.as_deref(),
            Some(since_ms),
            until_ms,
            limit as i64,
        )?;
        // Same window, both halves — a session list that stops at the archive
        // boundary is a list of the recent ones wearing the name of all of them.
        if let Some(arch) = self.archive() {
            rows.extend(tp_db::query::list_sessions(
                arch.conn(),
                folder.as_deref(),
                Some(since_ms),
                until_ms,
                limit as i64,
            )?);
            rows.sort_by_key(|s| std::cmp::Reverse(s.last_turn_at.unwrap_or(0)));
            rows.truncate(limit);
        }
        let items = rows
            .into_iter()
            .map(|s| SessionRow {
                id: s.id,
                runtime_id: s.runtime_id,
                cwd: s.cwd,
                title: s.title,
                last_turn_at: s.last_turn_at,
                turn_count: Some(s.turn_count),
            })
            .collect::<Vec<_>>();
        let coverage = Coverage {
            sessions_scanned: items.len(),
            truncated: items.len() >= limit,
            degraded,
        };
        Ok(Retrieved::new(items, coverage))
    }

    fn turns(
        &self,
        session: &SessionId,
        at: TurnCursor,
        include_thinking: bool,
        limit: usize,
        budget_bytes: usize,
    ) -> Result<Retrieved<NormalizedTurn>> {
        let db = self.open()?;
        // `limit as i64` was two bugs at once, and both produced the SAME wrong
        // answer: nothing.
        //
        // A limit above i64::MAX wrapped negative, `limit + 1` in
        // `list_turns_window` then made it 0, and SQLite's `LIMIT 0` returns no
        // rows — so asking for "as many as you can" got zero, and the CLI
        // printed "no turns since 365d" for a session holding 6,756 of them.
        // A limit of ZERO — which a person types — did the same thing.
        //
        // The scan backend does neither, because `WindowBuffer` keeps one turn
        // no matter what (retrieval.rs:271: "returning nothing would read as
        // 'this window is empty'"). That rule is the house rule; this backend
        // simply did not implement it, and the conformance suite drove both
        // backends everywhere except here. Measured before the fix, same
        // session, same window: index 0 turns and scan 1 at `--limit 0`; index
        // 0 and scan 361 at `--limit 18446744073709551615`.
        let limit = limit.max(1);
        let sql_limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let read = |db: &Db| -> Result<(Vec<tp_db::TurnRow>, bool)> {
            Ok(match at {
                TurnCursor::Window {
                    since_ms,
                    before_ms,
                } => tp_db::query::list_turns_window(
                    db.conn(),
                    &session.to_string(),
                    since_ms,
                    before_ms,
                    include_thinking,
                    sql_limit,
                )?,
                // The cursor goes INTO the query. This arm used to fetch the FIRST
                // `limit * 4` turns by `seq` and filter on `ts` in Rust afterwards,
                // for both cursors, with no comment explaining the 4. So paging an
                // `AfterTs` read past the start of a session ran out of fetched
                // rows and reported an EMPTY result as complete: on a seeded
                // 1,000-turn session, `AfterTs(ts of turn 700)` returned 701..800
                // with `truncated = false`, silently dropping 801..1000, and
                // `AfterTs(ts of turn 850)` returned nothing at all. The scan
                // backend reads the whole file and paged correctly, so the two
                // disagreed about the cursor the README documents for paging.
                TurnCursor::AfterTs(ts) => tp_db::query::list_turns_after_ts(
                    db.conn(),
                    &session.to_string(),
                    ts,
                    include_thinking,
                    sql_limit,
                )?,
                TurnCursor::Start => {
                    let mut rows = tp_db::query::list_turns(
                        db.conn(),
                        &session.to_string(),
                        0,
                        include_thinking,
                        sql_limit.saturating_add(1),
                    )?;
                    let more = rows.len() as i64 > sql_limit;
                    if more {
                        rows.pop();
                    }
                    (rows, more)
                }
            })
        };

        // Read the archive when the main index has nothing for this session.
        // Reading a session BY ID must not depend on which half it landed in —
        // that is the same "not found means not here" confusion the search merge
        // above avoids, and by id it is worse: the caller already knows the
        // session exists.
        let (rows, more_in_window) = match read(&db)? {
            (rows, more) if !rows.is_empty() => (rows, more),
            empty => match self.archive() {
                Some(arch) => read(&arch)?,
                None => empty,
            },
        };
        let to_turn = |r: tp_db::TurnRow| NormalizedTurn {
            role: role_from_str(&r.role),
            ts: r.ts,
            text: r.text,
            thinking: r.thinking.unwrap_or_default(),
            // Carried through rather than defaulted to false: an index read that
            // said "not opaque" while a scan of the same file said otherwise is
            // the provider split LLD §16 forbids.
            thinking_opaque: r.thinking_opaque,
            // Was hardcoded empty, so every tool-only turn read from the index
            // looked like a turn with nothing in it — while the same turn from a
            // scan carried its tool names. The column was written all along.
            tool_calls: r.tool_calls,
            // Carried through like `thinking_opaque` — an index answer of
            // "current" where the scan says "superseded" is the LLD §16 split.
            surface: r.surface,
            tokens_in: None,
            tokens_out: None,
            prov: r.prov,
        };

        // Newest-first for a window, oldest-first for a forward read — the same
        // split the scan provider makes, via the same two shared primitives.
        let (out, truncated) = if let TurnCursor::Window { .. } = at {
            let mut buf = tp_core::retrieval::WindowBuffer::new(limit, budget_bytes);
            for r in rows {
                if !at.admits_ts(r.ts) {
                    continue;
                }
                buf.push(to_turn(r));
            }
            let (items, dropped) = buf.finish();
            (items, dropped || more_in_window)
        } else {
            let mut out = Vec::new();
            let mut used = 0usize;
            let mut truncated = false;
            for r in rows {
                if !at.admits_ts(r.ts) {
                    continue;
                }
                // Same shared primitive the scan provider uses, so the two cut at
                // exactly the same place — the conformance suite checks this.
                if !tp_core::retrieval::admit_turn(&mut out, to_turn(r), &mut used, budget_bytes) {
                    truncated = true;
                    break;
                }
                if out.len() >= limit {
                    break;
                }
            }
            // Reaching `limit` is not itself truncation — `more_in_window` is
            // the extra row the query fetched to find out, so a session with
            // exactly `limit` turns left is reported as complete instead of
            // being guessed at.
            (out, truncated || more_in_window)
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
