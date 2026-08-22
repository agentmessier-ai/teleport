//! Read paths (LLD §9: `/v1/sessions`, `/v1/sessions/:id/turns`, `/v1/search`).
//! Search returns coordinates + a snippet, never full conversations — that's
//! what scales, and it's the caller's call whether a hit is worth spending
//! context on (LLD §9, §10).

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub runtime_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub last_turn_at: Option<i64>,
    pub turn_count: i64,
}

/// Sessions with at least one turn in `[since_ms, until_ms)`, most recently
/// active first.
///
/// The time window used to be ignored here entirely — `--since` pruned by mtime
/// on the scan side and did nothing on the index side, so the same flag meant
/// different things depending on which backend answered. Conformance never
/// caught it because the fixture is entirely recent.
///
/// `last_turn_at` is recomputed WITHIN the window rather than read off the
/// session row: a session active on the 4th and again today has a stored
/// `last_turn_at` of today, which is the wrong answer to "when was this session
/// last active on the 4th" and would also sort the results wrongly.
pub fn list_sessions(
    conn: &Connection,
    cwd_filter: Option<&str>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    limit: i64,
) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        // Resolution happens HERE, not at write time: a `/rename` arriving two
        // hours into a session must win immediately without rewriting anything,
        // and the precedence is Codex's (user > ai > derived), which it uses to
        // read Claude Code's titles when importing.
        "SELECT s.id, s.runtime_id, s.cwd,
                COALESCE(s.title_user, s.title_ai, s.title_derived) AS title,
                MAX(t.ts) AS win_last, COUNT(t.id) AS win_count
         FROM session s
         JOIN turn t ON t.session_id = s.id
         WHERE (?1 IS NULL OR s.cwd LIKE '%' || ?1 || '%')
           AND (?2 IS NULL OR (t.ts IS NOT NULL AND t.ts >= ?2))
           AND (?3 IS NULL OR (t.ts IS NOT NULL AND t.ts <  ?3))
         GROUP BY s.id
         HAVING win_count > 0
         ORDER BY win_last DESC
         LIMIT ?4",
    )?;
    let rows = stmt
        .query_map(params![cwd_filter, since_ms, until_ms, limit], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                runtime_id: r.get(1)?,
                cwd: r.get(2)?,
                title: r.get(3)?,
                last_turn_at: r.get(4)?,
                turn_count: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[derive(Debug, Clone)]
pub struct TurnRow {
    pub seq: i64,
    pub role: String,
    pub ts: Option<i64>,
    pub text: String,
    /// `None` unless the caller explicitly asked for it — thinking is opt-in
    /// at read time even when it's already been indexed (LLD §9).
    pub thinking: Option<String>,
    /// Reasoning happened and is unreadable (`thinking_state = 'opaque'`).
    ///
    /// Selected unconditionally, unlike `thinking` itself: the FACT that a turn
    /// reasoned is not the reasoning, it is one flag, and withholding it would
    /// make an index read assert "no reasoning" where a scan of the same file
    /// says otherwise — the provider split LLD §16 forbids.
    pub thinking_opaque: bool,
    /// Whether this turn is still live context (`turn.surface`). Selected
    /// unconditionally for the same reason as `thinking_opaque`: an index read
    /// that stayed silent while a scan of the same file said `superseded` is the
    /// provider split LLD §16 forbids.
    pub surface: tp_core::turn::Surface,
    /// Names of the tools this turn invoked. Stored as JSON at write time and
    /// simply never selected, so an index read reported every tool-only turn as
    /// empty while the same turn from a scan carried its tools.
    pub tool_calls: Vec<tp_core::turn::ToolCallDigest>,
    /// Source identity/lineage/cost (docs/data-model-v2.md). Round-tripped so an
    /// index read carries what a scan does — otherwise the providers diverge.
    pub prov: tp_core::turn::Provenance,
}

/// The columns every turn query selects, in the order `row_to_turn` reads them.
///
/// One constant because the three queries below differ ONLY in their WHERE and
/// ORDER BY — the column lists were byte-identical, maintained by hand, and
/// read positionally. `row_to_turn` already exists because that mapping was
/// written out three times and a column reached one copy and not the others;
/// consolidating the mapping and leaving the SELECTs is half a fix, and the
/// half that was left kept costing: `sidechain` and then `surface` were each
/// added to all three by hand, in two separate commits, in one evening.
///
/// Adding a column is now two edits that the compiler ties together — this list
/// and `row_to_turn` — instead of four that nothing does.
const TURN_COLUMNS: &str = "seq, role, ts, text, thinking, tool_calls,
     uuid, parent_uuid, model, cache_read_tokens, cache_creation_tokens,
     thinking_state, sidechain, surface";

/// One `turn` row, from the column order every query in this module selects.
///
/// Positional, so it is only correct against `TURN_COLUMNS`. Any query feeding
/// it must select that list and nothing else.
fn row_to_turn(r: &rusqlite::Row, include_thinking: bool) -> rusqlite::Result<TurnRow> {
    let thinking: Option<String> = if include_thinking { r.get(4)? } else { None };
    let tool_calls: Option<String> = r.get(5)?;
    Ok(TurnRow {
        seq: r.get(0)?,
        role: r.get(1)?,
        ts: r.get(2)?,
        text: r.get(3)?,
        thinking,
        // A row written before this column carried anything, or by a future
        // writer with a different shape, must not fail the read.
        tool_calls: tool_calls
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
        thinking_opaque: r
            .get::<_, Option<String>>(11)?
            .is_some_and(|s| s == "opaque"),
        // Anything unrecognized reads as Unknown, never as Current: a value a
        // future writer invents must not be promoted to "still live context".
        surface: match r.get::<_, Option<String>>(13)?.as_deref() {
            Some("current") => tp_core::turn::Surface::Current,
            Some("superseded") => tp_core::turn::Surface::Superseded,
            _ => tp_core::turn::Surface::Unknown,
        },
        prov: tp_core::turn::Provenance {
            uuid: r.get(6)?,
            parent_uuid: r.get(7)?,
            // `Option` then `unwrap_or`: NOT NULL in the schema, but a row this
            // process did not write is not a guarantee this process gets to make.
            sidechain: r.get::<_, Option<bool>>(12)?.unwrap_or(false),
            model: r.get(8)?,
            cache_read_tokens: r.get(9)?,
            cache_creation_tokens: r.get(10)?,
        },
    })
}

/// Turns strictly after a timestamp, oldest first, with "there are more".
///
/// The cursor goes INTO the query. The index provider used to page an
/// `AfterTs` read by fetching the FIRST `limit * 4` turns of the session by
/// `seq` and filtering on `ts` in Rust afterwards — so once a caller had paged
/// past turn 800 of a session, every fetched row failed the filter and the
/// answer was zero turns with `truncated = false`. Measured on a seeded
/// 1,000-turn session: `AfterTs(ts of turn 700)` returned turns 701..800 and
/// claimed completeness, dropping 801..1000; `AfterTs(ts of turn 850)`
/// returned nothing at all. The scan backend reads the whole file, so the two
/// disagreed about the cursor this project documents for paging.
///
/// `limit + 1` rows are requested for the same reason `list_turns_window` does
/// it: the extra row is how "there is more" is known rather than guessed.
pub fn list_turns_after_ts(
    conn: &Connection,
    session_id: &str,
    after_ts: i64,
    include_thinking: bool,
    limit: i64,
) -> Result<(Vec<TurnRow>, bool)> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLUMNS}
         FROM turn
         WHERE session_id = ?1 AND ts IS NOT NULL AND ts > ?2
         ORDER BY seq ASC
         LIMIT ?3"
    ))?;
    let mut rows = stmt
        .query_map(
            params![session_id, after_ts, limit.saturating_add(1)],
            |r| row_to_turn(r, include_thinking),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = rows.len() as i64 > limit;
    if more {
        rows.pop(); // the extra row is the NEWEST — this read goes forward
    }
    Ok((rows, more))
}

pub fn list_turns(
    conn: &Connection,
    session_id: &str,
    since_seq: i64,
    include_thinking: bool,
    limit: i64,
) -> Result<Vec<TurnRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLUMNS}
         FROM turn
         WHERE session_id = ?1 AND seq > ?2
         ORDER BY seq ASC
         LIMIT ?3"
    ))?;
    let rows = stmt
        .query_map(params![session_id, since_seq, limit], |r| {
            row_to_turn(r, include_thinking)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The newest turns inside a time window, returned oldest-first.
///
/// A window read keeps the NEWEST turns, so it cannot be served by
/// `list_turns`, which walks from the oldest and caps at `limit * 4` — on a long
/// session that window would be answered entirely from turns that are not in it.
/// The bounds go into the SQL rather than being filtered in Rust so a 30-day
/// window over a huge session never materializes the rows it will discard.
///
/// `limit + 1` rows are requested: the extra one is how we can say the window
/// held more than we returned, instead of silently handing back a prefix.
pub fn list_turns_window(
    conn: &Connection,
    session_id: &str,
    since_ms: i64,
    before_ms: Option<i64>,
    include_thinking: bool,
    limit: i64,
) -> Result<(Vec<TurnRow>, bool)> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {TURN_COLUMNS}
         FROM turn
         WHERE session_id = ?1
           AND ts IS NOT NULL AND ts >= ?2
           AND (?3 IS NULL OR ts < ?3)
         ORDER BY seq DESC
         LIMIT ?4"
    ))?;
    let mut rows = stmt
        .query_map(
            params![session_id, since_ms, before_ms, limit.saturating_add(1)],
            // `row_to_turn`, not a second copy of it. This query used to inline
            // its own mapping, which is exactly the hazard that function's doc
            // warns about — and it landed: adding `thinking_state` to the SELECT
            // lists left this copy building a `TurnRow` without it, so a window
            // read would have reported every turn as having no reasoning while
            // the other two reads said otherwise.
            |r| row_to_turn(r, include_thinking),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = rows.len() as i64 > limit;
    if more {
        rows.pop(); // the extra row is the OLDEST — dropping the oldest is the rule
    }
    rows.reverse(); // chronological, like every other read
    Ok((rows, more))
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub session_id: String,
    pub machine_id: String,
    pub seq: i64,
    pub ts: Option<i64>,
    pub role: String,
    pub snippet: String,
    pub rank: f64,
    pub sidechain: bool,
    pub surface: tp_core::turn::Surface,
}

/// `include_thinking=false` restricts the MATCH itself to the `text`/`tool_calls`
/// FTS columns via FTS5's `{col1 col2}:` column-filter syntax — thinking is not
/// just hidden from the result, it isn't searched (LLD §9 gate).
fn build_match_expr(user_query: &str, include_thinking: bool) -> String {
    // Whole-query phrase match: safest default for a CLI — avoids user input
    // colliding with FTS5 query-syntax tokens (AND/OR/NOT/-/:/*).
    let escaped = user_query.replace('"', "\"\"");
    let phrase = format!("\"{escaped}\"");
    if include_thinking {
        phrase
    } else {
        format!("{{text tool_calls}} : {phrase}")
    }
}

/// `since_ms` is applied **in SQL**, not by the caller. Filtering after the
/// query would let `LIMIT` fill with the best-ranked matches across all
/// history and then discard them for being out of window — turning a corpus
/// with thousands of in-window matches into a confident "no matches" whenever
/// old rows outrank recent ones. Rows with a NULL `ts` are kept: an unknown
/// timestamp is not evidence of being too old.
/// `cwd_filter` is applied IN the query, not to the result set, for the same
/// reason the time window is: filtering afterwards means `LIMIT` has already
/// been spent on rows the caller excluded.
///
/// Matched two ways, because a folder needle arrives in two forms and both are
/// natural. `s.cwd` is the real path (`/Users/me/dev/devops`), but everything
/// that prints a transcript location prints the ENCODED form
/// (`-Users-me-dev-devops`), and pasting that back is the obvious thing to do.
/// The scan provider widened to accept both after the literal-only comparison
/// silently returned nothing; this is the same widening, expressed in SQL.
pub fn search(
    conn: &Connection,
    user_query: &str,
    include_thinking: bool,
    limit: i64,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    cwd_filter: Option<&str>,
) -> Result<Vec<SearchHit>> {
    let match_expr = build_match_expr(user_query, include_thinking);
    // snippet() takes ONE column index, so a turn whose only content is a tool
    // call (text = '') previewed as an empty string: a coordinate with no
    // evidence of why it matched, which is the one thing a search result must
    // always carry. Fall back to the tool_calls column when the text snippet
    // is empty. `thinking` (column 1) is deliberately never previewed even
    // when searched — it is fetched per-turn via list_turns, opted in.
    let sql = "SELECT t.session_id, s.machine_id, t.seq, t.ts, t.role,
                CASE WHEN snippet(turn_fts, 0, '[', ']', '…', 10) <> ''
                     THEN snippet(turn_fts, 0, '[', ']', '…', 10)
                     ELSE snippet(turn_fts, 2, '[', ']', '…', 10) END AS snip,
                bm25(turn_fts) AS rank, t.sidechain, t.surface
         FROM turn_fts
         JOIN turn t ON t.id = turn_fts.rowid
         JOIN session s ON s.id = t.session_id
         WHERE turn_fts MATCH ?1
           AND (?3 IS NULL OR t.ts IS NULL OR t.ts >= ?3)
           AND (?4 IS NULL OR (t.ts IS NOT NULL AND t.ts < ?4))
           AND (?5 IS NULL
                OR LOWER(s.cwd) LIKE '%' || LOWER(?5) || '%'
                OR REPLACE(LOWER(s.cwd), '/', '-')
                     LIKE '%' || REPLACE(LOWER(?5), '/', '-') || '%')
         ORDER BY rank
         LIMIT ?2";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(
            params![match_expr, limit, since_ms, until_ms, cwd_filter],
            |r| {
                Ok(SearchHit {
                    session_id: r.get(0)?,
                    machine_id: r.get(1)?,
                    seq: r.get(2)?,
                    ts: r.get(3)?,
                    role: r.get(4)?,
                    snippet: r.get(5)?,
                    rank: r.get(6)?,
                    sidechain: r.get::<_, Option<bool>>(7)?.unwrap_or(false),
                    // Unrecognized reads as Unknown, never Current — same rule
                    // as `row_to_turn`.
                    surface: match r.get::<_, Option<String>>(8)?.as_deref() {
                        Some("current") => tp_core::turn::Surface::Current,
                        Some("superseded") => tp_core::turn::Surface::Superseded,
                        _ => tp_core::turn::Surface::Unknown,
                    },
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ── Federation / pairing (LLD §8.2) ─────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MachineRow {
    pub id: String,
    pub name: String,
    pub trust: String,
    pub pubkey: Option<Vec<u8>>,
    pub addr: Option<String>,
    pub last_seen_at: Option<i64>,
}

/// Where a session's transcript lives on disk, if teleport indexed it from a
/// file at all. `None` for a pushed session — there is no file (see
/// `writer::commit_pushed`).
/// Every session in the window that claims a transcript on disk, with how many
/// turns it holds — the input to "how much of this window can a scan even see".
///
/// Returns the CLAIM, not the answer: whether each path still exists is a
/// filesystem question, and the caller stats them. Kept that way so storage does
/// not reach into the filesystem, and so the cost is visible where it is paid.
pub fn sessions_claiming_a_file(
    conn: &Connection,
    since_ms: i64,
    until_ms: Option<i64>,
) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT source_path, turn_count FROM session
          WHERE source_path IS NOT NULL
            AND last_turn_at IS NOT NULL
            AND last_turn_at >= ?1
            AND (?2 IS NULL OR last_turn_at < ?2)",
    )?;
    let rows = stmt
        .query_map(params![since_ms, until_ms], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// The other half of the same question: sessions in the window that never had a
/// transcript at all.
///
/// `sessions_claiming_a_file` answers "the scan could have read this once" —
/// its first condition is `source_path IS NOT NULL`, so a push-ingested runtime
/// is invisible to it. Those sessions arrive through `tp ingest` (dsh does
/// this), and no file is ever written, so there is nothing to stat and nothing
/// for a scan to find, ever.
///
/// Counted rather than listed: there are no paths to check, so the caller has
/// no filesystem work to do and no reason to see the rows.
pub fn sessions_without_a_file(
    conn: &Connection,
    since_ms: i64,
    until_ms: Option<i64>,
) -> Result<(usize, i64)> {
    let (n, turns): (i64, Option<i64>) = conn.query_row(
        "SELECT COUNT(*), SUM(turn_count) FROM session
          WHERE source_path IS NULL
            AND last_turn_at IS NOT NULL
            AND last_turn_at >= ?1
            AND (?2 IS NULL OR last_turn_at < ?2)",
        params![since_ms, until_ms],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((n as usize, turns.unwrap_or(0)))
}

pub fn source_path(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT source_path FROM session WHERE id = ?1",
            [session_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// The most recently active indexed session with this exact `cwd`, scoped to one
/// runtime — how the process scan puts a real session id on a process it found.
///
/// The runtime scope is load-bearing: a pi process sharing a directory with an
/// indexed Claude Code session must not be mistaken for it.
pub fn latest_session_for_cwd(
    conn: &Connection,
    machine_id: &str,
    runtime_id: &str,
    cwd: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT id FROM session
          WHERE machine_id = ?1 AND runtime_id = ?2 AND cwd = ?3
          ORDER BY last_turn_at DESC LIMIT 1",
        params![machine_id, runtime_id, cwd],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// What the resident daemon published about itself when it started.
///
/// `None` means no daemon has started since migration 0011 — NOT that none is
/// running. The two are different and a caller must not report the second.
#[derive(Debug, Clone)]
pub struct DaemonStatus {
    pub version: String,
    pub pid: i64,
    pub started_at: i64,
}

pub fn daemon_status(conn: &Connection) -> Result<Option<DaemonStatus>> {
    conn.query_row(
        "SELECT version, pid, started_at FROM daemon_status WHERE id = 1",
        [],
        |r| {
            Ok(DaemonStatus {
                version: r.get(0)?,
                pid: r.get(1)?,
                started_at: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// The last backup, if one was ever taken.
#[derive(Debug, Clone)]
pub struct BackupStatus {
    pub taken_at: i64,
    pub dest: String,
    pub turn_count: i64,
    pub bytes: i64,
}

/// `None` means no backup has EVER been recorded — which for an index holding
/// the only copy of a quarter of its turns is the answer that matters most, and
/// the one the caller must not render as "0 days ago".
pub fn backup_status(conn: &Connection) -> Result<Option<BackupStatus>> {
    conn.query_row(
        "SELECT taken_at, dest, turn_count, bytes FROM backup_status WHERE id = 1",
        [],
        |r| {
            Ok(BackupStatus {
                taken_at: r.get(0)?,
                dest: r.get(1)?,
                turn_count: r.get(2)?,
                bytes: r.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn machine(conn: &Connection, id: &str) -> Result<Option<MachineRow>> {
    conn.query_row(
        "SELECT id, name, trust, pubkey, addr, last_seen_at FROM machine WHERE id = ?1",
        [id],
        |r| {
            Ok(MachineRow {
                id: r.get(0)?,
                name: r.get(1)?,
                trust: r.get(2)?,
                pubkey: r.get(3)?,
                addr: r.get(4)?,
                last_seen_at: r.get(5)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Record where a peer was last reached. Discovery is NOT trust: this only
/// updates a machine we already have a relationship with, and never inserts.
/// An unknown device seen on the LAN is a scan result the user may act on, not
/// state to persist — otherwise anyone broadcasting on the network could write
/// rows into every listener's `machine` table.
pub fn touch_peer(conn: &Connection, device_id: &str, addr: &str) -> Result<bool> {
    // Seconds, matching `created_at` / `paired_at` written by the pairing path.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let n = conn.execute(
        "UPDATE machine SET addr = ?1, last_seen_at = ?2 WHERE id = ?3 AND is_self = 0",
        params![addr, now, device_id],
    )?;
    Ok(n > 0)
}

/// Every machine we have a relationship with, in any trust state — what
/// `tp peers` shows. `trusted_peers` is the query-time subset.
pub fn all_peers(conn: &Connection) -> Result<Vec<MachineRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, trust, pubkey, addr, last_seen_at FROM machine
         WHERE is_self = 0 ORDER BY trust, name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MachineRow {
                id: r.get(0)?,
                name: r.get(1)?,
                trust: r.get(2)?,
                pubkey: r.get(3)?,
                addr: r.get(4)?,
                last_seen_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Peers we can query (trusted). Never includes self.
pub fn trusted_peers(conn: &Connection) -> Result<Vec<MachineRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, trust, pubkey, addr, last_seen_at FROM machine
         WHERE trust = 'trusted' AND is_self = 0",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(MachineRow {
                id: r.get(0)?,
                name: r.get(1)?,
                trust: r.get(2)?,
                pubkey: r.get(3)?,
                addr: r.get(4)?,
                last_seen_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod turn_columns_tests {
    use super::*;

    /// `TURN_COLUMNS` and `row_to_turn` must agree on how many columns there
    /// are, because the mapping is POSITIONAL — `r.get(13)` is only `surface`
    /// while the list has exactly fourteen entries in exactly that order.
    ///
    /// Consolidating the three SELECTs removed the "added it to one query and
    /// not the others" failure. It did NOT remove this one: a column appended
    /// to the list without a line in `row_to_turn` compiles, runs, and is
    /// silently never read. Verified by trying it — nothing else in the suite
    /// noticed.
    #[test]
    fn the_column_list_and_the_row_mapping_agree_on_arity() {
        // The highest index `row_to_turn` reads, plus one. Update BOTH when a
        // column is added; that is the whole contract this test enforces.
        const READ_BY_ROW_TO_TURN: usize = 14;

        let db = crate::Db::open_in_memory().expect("in-memory db");
        let sql = format!("SELECT {TURN_COLUMNS} FROM turn");
        let stmt = db.conn().prepare(&sql).expect("the column list must parse");
        assert_eq!(
            stmt.column_count(),
            READ_BY_ROW_TO_TURN,
            "TURN_COLUMNS has {} columns and row_to_turn reads {READ_BY_ROW_TO_TURN}",
            stmt.column_count()
        );
    }

    /// Every column named actually exists on `turn`. A typo would otherwise
    /// surface as a runtime error on the first read of a real session, which is
    /// after shipping rather than before.
    #[test]
    fn every_named_column_exists() {
        let db = crate::Db::open_in_memory().expect("in-memory db");
        db.conn()
            .prepare(&format!("SELECT {TURN_COLUMNS} FROM turn LIMIT 0"))
            .expect("TURN_COLUMNS must name real columns");
    }
}
