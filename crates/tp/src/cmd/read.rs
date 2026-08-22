#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Exempt from the workspace print lints: this module's OUTPUT IS THE
//! PRODUCT. `tp` is a CLI; a subcommand that stopped writing to stdout would
//! have no function. Narrow and permanent — see [workspace.lints.clippy] in
//! the root Cargo.toml for why the lint exists everywhere else.

//! Retrieval commands: index/reindex, search (local and fan-out), sessions,
//! turns — everything that answers "what happened".

use crate::{db_path, fmt_ts, hostname, identity, machine_id, parse_duration, parse_time_bound};
use anyhow::Result;
use std::path::PathBuf;
use tp_core::retrieval::{Query, Scope, TurnCursor};
use tp_db::Db;
use tp_search::{IndexProvider, Retrieval, ScanProvider};

/// Re-read every session whose transcript is STILL ON DISK, so columns added
/// after it was first indexed stop being empty.
///
/// Why this exists: ingest resumes from `(inode, byte_offset)` and never
/// re-reads a byte. That is right for incremental indexing and wrong after a
/// schema or descriptor change — a field added today is populated only for bytes
/// read after today. Measured on this machine before `reindex` existed:
/// provenance (`uuid`/`parent_uuid`, migration 0005) was 0% for June and July
/// turns and 35% for August; every native title sat behind a checkpoint and was
/// never seen. The index was heterogeneous in a way no query could detect.
///
/// SELECTIVE, and this is the whole design. "The index is derived data, so a
/// rebuild is free" is FALSE here: Claude Code deletes transcripts after ~30
/// days, and on this machine 14,301 of 42,629 sessions have no source file left.
/// For those the index is the ONLY copy — 133,848 turns that a blind rebuild
/// would destroy. So a session is rebuilt only if its file is still readable,
/// and everything else is left exactly as it is. Those rows keep whatever they
/// had, which for a column added later means `unknown` — the honest answer,
/// because the evidence is gone.
pub(crate) fn run_reindex(dry_run: bool, runtime: Option<String>) -> Result<()> {
    let mut db = Db::open(&db_path())?;
    let machine = machine_id()?;

    // ONE query, both numbers. Sessions this machine indexed, then narrowed in
    // Rust — `source_path IS NULL` is the pushed path (`writer::commit_pushed`
    // stores no path because there is no file) and must never be touched, and a
    // transcript that has since been deleted must not be either.
    //
    // Filtered here rather than in SQL because tp-db deliberately does not
    // re-export rusqlite, so `params!` cannot be named; and because deriving the
    // total from the same list means the two printed numbers cannot describe
    // different sets.
    let all: Vec<(String, Option<String>, String)> = {
        let conn = db.conn();
        let mut stmt =
            conn.prepare("SELECT id, source_path, runtime_id FROM session WHERE machine_id = ?1")?;
        let rows: Vec<(String, Option<String>, String)> = stmt
            .query_map([&machine], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(Result::ok)
            .collect();
        rows
    };

    let in_scope: Vec<&(String, Option<String>, String)> = all
        .iter()
        .filter(|(_, _, rt)| runtime.as_ref().is_none_or(|want| rt == want))
        .collect();
    let candidates: Vec<(String, String)> = in_scope
        .iter()
        .filter_map(|(id, path, _)| path.as_ref().map(|p| (id.clone(), p.clone())))
        .filter(|(_, p)| std::path::Path::new(p).exists())
        .collect();

    let scope = match &runtime {
        Some(r) => format!("{r} "),
        None => String::new(),
    };
    if let Some(want) = &runtime {
        if in_scope.is_empty() {
            let mut known: Vec<&str> = all.iter().map(|(_, _, rt)| rt.as_str()).collect();
            known.sort_unstable();
            known.dedup();
            // Not an error, but not silence either: `--runtime pi_` re-reading
            // nothing and reporting success reads exactly like a clean no-op.
            println!(
                "no session has runtime {want} — indexed runtimes: {}",
                known.join(", ")
            );
            return Ok(());
        }
    }
    println!(
        "{} of {} {scope}session(s) have a readable transcript; the rest keep what they have",
        candidates.len(),
        in_scope.len()
    );
    if dry_run {
        println!("--dry-run: nothing changed");
        return Ok(());
    }

    // A live daemon is not a race worth surviving, it is a race worth refusing.
    // Measured, on this machine: tpd held a write lock while reindex tried to
    // re-read a 44 MB transcript, `scan_root` downgraded the failure to
    // `[warn] skipping …: database is locked (indexing continues)`, and the
    // session was left with 12 turns of the 10,836 it had. The clear is
    // committed; the refill is best-effort. Those two facts cannot both stand.
    if let Some(d) = tp_db::query::daemon_status(db.conn())? {
        if daemon_is_live(d.pid) {
            anyhow::bail!(
                "tpd is running (pid {}) and would race this reindex, which can leave a \
                 session emptied.\n\
                 Stop it, reindex, start it again:\n\
                 \tlaunchctl bootout gui/$(id -u)/io.teleport.tpd\n\
                 \ttp reindex{}\n\
                 \tlaunchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.teleport.tpd.plist",
                d.pid,
                runtime
                    .as_ref()
                    .map(|r| format!(" --runtime {r}"))
                    .unwrap_or_default()
            );
        }
    }

    // Forget the turns and the checkpoint together, in ONE transaction per
    // session. Doing either alone is broken: dropping turns without the
    // checkpoint re-reads nothing, and retiring the checkpoint without dropping
    // turns restarts `seq` at 1 and collides with rows already there
    // (`UNIQUE(session_id, seq)`).
    let mut cleared = 0usize;
    {
        let tx = db.conn_mut().transaction()?;
        for (sid, _) in &candidates {
            tx.execute("DELETE FROM turn WHERE session_id = ?1", [sid])?;
            tx.execute("DELETE FROM ingest_state WHERE session_id = ?1", [sid])?;
            tx.execute(
                "UPDATE session SET turn_count = 0, last_turn_at = NULL WHERE id = ?1",
                [sid],
            )?;
            cleared += 1;
        }
        tx.commit()?;
    }
    println!("cleared {cleared} session(s); re-reading");

    // A refill that could not read a file is data loss, because the rows it
    // would have restored were deleted above and committed. `tp index` is right
    // to continue past a bad file; this caller is not, and until `scan_root`
    // returned its failures there was no way for the two to differ.
    let mut unreadable: Vec<(std::path::PathBuf, String)> = Vec::new();
    for r in tp_app::ingest::index_all(&mut db, &machine, &hostname())? {
        print_indexed(&r);
        if let tp_app::ingest::Indexed::Scanned { failed, .. } = &r {
            unreadable.extend(failed.iter().cloned());
        }
    }
    if !unreadable.is_empty() {
        anyhow::bail!(
            "{} file(s) could not be re-read after their rows were cleared — see the paths above.              The transcripts are still on disk, so this is recoverable: fix the cause (a running              `tpd` holding the database is the usual one) and run `tp reindex` again.",
            unreadable.len()
        );
    }

    // The belt to that pair of braces. A file can be read successfully and still
    // yield nothing — a parse that produces zero turns leaves the session empty
    // without ever raising an error, and the check above cannot see it.
    let emptied: Vec<&String> = {
        let conn = db.conn();
        let mut stmt = conn.prepare("SELECT count(*) FROM turn WHERE session_id = ?1")?;
        candidates
            .iter()
            .filter(|(sid, _)| stmt.query_row([sid], |r| r.get::<_, i64>(0)).unwrap_or(0) == 0)
            .map(|(sid, _)| sid)
            .collect()
    };
    if !emptied.is_empty() {
        for sid in emptied.iter().take(20) {
            eprintln!("[EMPTIED] {sid}");
        }
        if emptied.len() > 20 {
            eprintln!("[EMPTIED] … and {} more", emptied.len() - 20);
        }
        anyhow::bail!(
            "{} session(s) were cleared and came back with no turns — their transcripts were \
             not re-read. Look for `skipping` warnings above, fix the cause, and run \
             `tp reindex` again; the transcripts are still on disk, so this is recoverable.",
            emptied.len()
        );
    }
    Ok(())
}

/// Whether the daemon `daemon_status` last recorded is still running.
///
/// The command name is checked, not just the pid's existence: `daemon_status`
/// keeps the last recorded start forever, so a recycled pid would make `reindex`
/// refuse for no reason. Confirming it really is tpd is what lets the refusal be
/// unconditional instead of needing an override flag nobody can evaluate.
pub(crate) fn daemon_is_live(pid: i64) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim().ends_with("tpd"))
}

/// One line per runtime. Shared by `index` and `reindex` so the two cannot drift
/// into describing the same work differently.
pub(crate) fn print_indexed(r: &tp_app::ingest::Indexed) {
    match r {
        tp_app::ingest::Indexed::NoRoot { runtime_id, root } => {
            println!("{runtime_id}: no root at {root} — skipped")
        }
        tp_app::ingest::Indexed::Scanned {
            runtime_id,
            files_touched,
            turns_written,
            sources_seen,
            failed,
        } => {
            println!(
                "{runtime_id}: indexed {turns_written} new turn(s) across {files_touched} file(s) ({sources_seen} known source file(s) scanned)"
            );
            // Surfaced here, for `index` as well as `reindex`. Continuing past a
            // bad file is right for both; leaving the fact only in tpd's log is
            // what let a reindex report success after losing 10,836 turns.
            if !failed.is_empty() {
                let shown: Vec<String> = failed
                    .iter()
                    .take(3)
                    .map(|(p, e)| format!("  {}: {e}", p.display()))
                    .collect();
                eprintln!("{runtime_id}: {} file(s) could not be read:", failed.len());
                eprintln!("{}", shown.join("\n"));
                if failed.len() > shown.len() {
                    eprintln!("  … and {} more", failed.len() - shown.len());
                }
            }
        }
    }
}

pub(crate) fn run_index() -> Result<()> {
    let mut db = Db::open(&db_path())?;
    for r in tp_app::ingest::index_all(&mut db, &machine_id()?, &hostname())? {
        print_indexed(&r);
    }
    Ok(())
}

/// Strategy selection lives here and nowhere else — every command below talks
/// only to `Retrieval` (LLD §16).
pub(crate) fn retrieval(use_index: bool) -> Result<Retrieval> {
    if use_index {
        return Ok(Retrieval::new(Box::new(IndexProvider::new(db_path()))));
    }
    let machine_id = machine_id()?;
    Ok(Retrieval::new(Box::new(ScanProvider::new(
        machine_id,
        tp_ingest::adapter::all_adapters(),
        tp_ingest::adapter::all_roots(),
    ))))
}

/// Coverage is part of the answer, not a footer — a truncated or degraded
/// result must never read as "there is nothing there" (LLD §16 rule 3).
/// What a scan cannot read in this window, printed next to coverage because it
/// is the same kind of fact: this answer is incomplete, and here is why. Opens
/// the index read-only and stays silent if there is none — a machine that never
/// ran `tp index` has nothing to be missing.
pub(crate) fn print_unscannable(r: &Retrieval, scope: &Scope) {
    let Ok(db) = Db::open(&db_path()) else {
        return;
    };
    if let Some(note) = tp_app::read::unscannable_note(r.provider_name(), scope, &db) {
        eprintln!("[coverage] {note}");
    }
}

pub(crate) fn print_coverage(c: &tp_core::Coverage) {
    let mut notes = Vec::new();
    if c.sessions_scanned > 0 {
        notes.push(format!("{} session(s) examined", c.sessions_scanned));
    }
    if c.truncated {
        notes.push("results truncated by --limit".to_string());
    }
    if let Some(d) = &c.degraded {
        notes.push(d.clone());
    }
    if !notes.is_empty() {
        eprintln!("[coverage] {}", notes.join(" · "));
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the clap command's fields 1:1
pub(crate) fn run_search(
    use_index: bool,
    query: &str,
    include_thinking: bool,
    folder: Option<String>,
    since: &str,
    until: Option<&str>,
    regex: bool,
    limit: usize,
    all: bool,
    only: &[String],
) -> Result<()> {
    let r = retrieval(use_index)?;
    let scope = window_scope(folder, since, until)?;
    if let Some(w) = r.scope_warning(&scope) {
        eprintln!("[warn] {w}");
    }
    let q = Query {
        text: query.to_string(),
        regex,
        include_thinking,
        limit,
    };
    let got = tp_app::read::search(&r, &q, &scope)?;

    if got.items.is_empty() && !all {
        // "window: 2026-08-06" alone is ambiguous once `since` can be a date —
        // it reads as "since that day", which is the opposite of what was asked.
        let window = match until {
            Some(u) => format!("{since} … {u}"),
            None => format!("{since} … now"),
        };
        println!(
            "no matches for {query:?} (provider: {}, window: {window})",
            r.provider_name()
        );
        // A window the caller never chose has to announce itself when it comes
        // up empty. `--all`'s peer-failure note already reasons this way — "a
        // silent partial result would read as 'this was never discussed
        // anywhere'" — and a default time bound is the same trap by a different
        // axis, reached without passing any flag at all.
        //
        // Reported from a real session that concluded a feature had never been
        // built. It had been, six days earlier; only the operator's memory
        // caught it. The line below is what would have prevented that.
        // Before the window note, because it is the more specific answer: if
        // the phrasing is what excluded everything, widening the window will
        // not help and the caller should not be sent to do it first.
        if let Some(note) = tp_app::read::empty_note(&q, got.items.len()) {
            println!("  {note}");
        }
        if since == DEFAULT_SEARCH_SINCE && until.is_none() {
            println!(
                "  This is the DEFAULT {DEFAULT_SEARCH_SINCE} window, not an exhaustive search — \
                 anything older was never looked at.\n  \
                 Widen before concluding it was never discussed: --since 30d, or --since <date>."
            );
        }
        // The budget is the same trap on a different axis, and widening the
        // window walks straight into it: `--since 30d` reached the scan budget
        // with 80% of candidate files never opened, and still printed a clean
        // "no matches". Coverage already reported the shortfall on the line
        // below — but a number in a footer does not undo a negative conclusion
        // stated above it. Reported by the session that verified the window fix
        // and immediately hit this.
        if let Some(d) = &got.coverage.degraded {
            println!(
                "  NOT an exhaustive search — {d}. The rest was never read.\n  \
                 Narrow with --folder, or build an index with `tp index`, before \
                 concluding it is not there."
            );
        }
    }
    for h in &got.items {
        // The same two marks `tp turns` prints, for the same reasons: whose
        // words matched, and whether they are still context.
        let side = if h.sidechain { " [subagent]" } else { "" };
        let dead = if h.surface == tp_core::turn::Surface::Superseded {
            " [superseded]"
        } else {
            ""
        };
        println!(
            "{}  [{:?}]{side}{dead}  {}",
            fmt_ts(h.at.ts),
            h.role,
            h.at.session_id
        );
        println!("    {}", h.excerpt().replace('\n', " "));
    }
    print_coverage(&got.coverage);
    print_unscannable(&r, &scope);
    if all || !only.is_empty() {
        run_search_all(query, since, limit, &got, only)?;
    }
    Ok(())
}

/// `--since`/`--until` as a `Scope`.
///
/// `Scope::since` is a DURATION back from now, so an absolute `--since` has to
/// be converted into "how long ago that was" — one place, because a date meaning
/// different instants to `search` and `sessions` is exactly the class of bug
/// this pair of flags exists to fix.
pub(crate) fn window_scope(
    folder: Option<String>,
    since: &str,
    until: Option<&str>,
) -> Result<Scope> {
    let now = tp_core::now_ms();
    let since_ms = parse_time_bound(since, now)?;
    Ok(Scope {
        folder,
        since: std::time::Duration::from_millis(now.saturating_sub(since_ms).max(0) as u64),
        runtimes: vec![],
        until: until.map(|u| parse_time_bound(u, now)).transpose()?,
    })
}

pub(crate) fn run_sessions(
    use_index: bool,
    folder: Option<String>,
    since: &str,
    until: Option<&str>,
    limit: usize,
) -> Result<()> {
    let r = retrieval(use_index)?;
    let scope = window_scope(folder, since, until)?;
    let got = tp_app::read::sessions(&r, &scope, limit)?;
    if got.items.is_empty() {
        println!(
            "no sessions in the last {since} (provider: {})",
            r.provider_name()
        );
    }
    for s in &got.items {
        let turns = s
            .turn_count
            .map(|n| format!("{n:>5} turns"))
            .unwrap_or_else(|| "     ? turns".to_string());
        println!(
            "{}  {}  {}  {}",
            fmt_ts(s.last_turn_at),
            turns,
            s.cwd.clone().unwrap_or_else(|| "-".to_string()),
            s.id
        );
        if let Some(title) = &s.title {
            println!("    {}", title.replace('\n', " "));
        }
    }
    print_coverage(&got.coverage);
    print_unscannable(&r, &scope);
    Ok(())
}

/// One turn's body line.
///
/// Printing `text` alone rendered most of a coding session as blank lines: in
/// one real session 1,115 of 1,791 message records were `tool_use`/`tool_result`
/// and carry no text. They still consumed `--limit` slots and the byte budget,
/// so a 400-turn read surfaced ~68 informative turns and then truncated — a read
/// that correctly reported truncation, having spent its budget on nothing. Worse,
/// a blank line is indistinguishable from an adapter that failed to parse the
/// record, and those two want opposite responses from whoever is reading.
///
/// Every branch here therefore emits SOMETHING, and names which case it is. The
/// tool names were in `tool_calls` the whole time; only the renderer dropped them.
pub(crate) fn turn_body(t: &tp_core::turn::NormalizedTurn, include_thinking: bool) -> String {
    if !t.text.is_empty() {
        return t.text.replace('\n', " ");
    }
    if !t.tool_calls.is_empty() {
        let names: Vec<&str> = t.tool_calls.iter().map(|c| c.name.as_str()).collect();
        return format!("({})", names.join(", "));
    }
    if !t.thinking.is_empty() && !include_thinking {
        return "(thinking — pass --include-thinking to show)".to_string();
    }
    // Not gated on `include_thinking`: unlike the branch above there is no
    // payload the flag would reveal, and the fallthrough line would call this
    // turn "no indexed content", which for codex's encrypted reasoning is
    // exactly the "no reasoning happened" claim `thinking_state = 'opaque'`
    // exists to prevent.
    if t.thinking_opaque {
        return "(reasoning happened but is encrypted by the runtime — nothing to show)"
            .to_string();
    }
    // Nothing was indexed. Usually a `tool_result`, whose body is deliberately
    // not stored (LLD §4: tool payloads are large and carry secrets), or a
    // signature-only `thinking` block.
    "(no indexed content — tool result or non-text record)".to_string()
}

/// Pick the session to read when the caller didn't name one.
///
/// Naming a session is the wrong thing to require for "what happened in the last
/// few hours" — you don't know the id yet; finding it is the question. So this
/// resolves the most recently active session in the window, and SAYS which one it
/// picked and how many others matched, because a silent pick among several is
/// indistinguishable from there having been only one.
pub(crate) fn resolve_session(
    r: &Retrieval,
    folder: Option<String>,
    since: &str,
    until: Option<&str>,
) -> Result<String> {
    // The SAME window the turns will be read with. Two bugs lived here: this
    // used `parse_duration`, so an absolute `--since 2026-08-04` was rejected
    // outright — and it dropped `until`, so the candidate set was "sessions
    // since the 4th", i.e. today's. It would then read today's session through
    // the 4th's window and find nothing: a silently wrong answer to a question
    // that had a right one.
    let scope = window_scope(folder.clone(), since, until)?;
    let got = tp_app::read::sessions(r, &scope, 20)?;
    let where_ = folder
        .as_ref()
        .map(|f| format!(" under {f:?}"))
        .unwrap_or_default();
    let window = match until {
        Some(u) => format!("between {since} and {u}"),
        None => format!("in the last {since}"),
    };
    let Some(first) = got.items.first() else {
        anyhow::bail!("no sessions active {window}{where_} — widen --since, or name a session id");
    };

    // Auto-pick needs a narrowing signal. Reading ONE arbitrary session and
    // presenting it as the answer to "what happened that day" is the failure
    // this whole command was added to fix, so with no folder and no session id
    // it refuses and shows what to choose from instead of guessing.
    if got.items.len() > 1 && folder.is_none() {
        let mut msg = format!(
            "{} sessions were active {window} — reading one of them would answer a \
             different question than you asked.\nNarrow with --folder, or name one:\n",
            got.items.len()
        );
        for s in got.items.iter().take(8) {
            msg.push_str(&format!(
                "  {}  {}\n",
                s.id,
                s.cwd.as_deref().unwrap_or("(unknown cwd)")
            ));
        }
        if got.items.len() > 8 {
            msg.push_str(&format!("  … and {} more\n", got.items.len() - 8));
        }
        anyhow::bail!(msg);
    }
    if got.items.len() > 1 {
        eprintln!(
            "[note] {} sessions matched; reading the most recent. Others:",
            got.items.len()
        );
        for s in got.items.iter().skip(1).take(4) {
            eprintln!(
                "         {}  {}",
                s.id,
                s.cwd.as_deref().unwrap_or("(unknown cwd)")
            );
        }
    }
    Ok(first.id.clone())
}

#[allow(clippy::too_many_arguments)] // mirrors the clap command's fields 1:1
pub(crate) fn run_turns(
    use_index: bool,
    session_id: Option<String>,
    after_ts: Option<i64>,
    since: Option<String>,
    until: Option<String>,
    folder: Option<String>,
    include_thinking: bool,
    limit: usize,
) -> Result<()> {
    let r = retrieval(use_index)?;
    let session_id = match session_id {
        Some(s) => s,
        None => {
            let Some(since) = since.as_deref() else {
                anyhow::bail!(
                    "give a session id, or --since (e.g. --since 4h) to read the most recent session"
                );
            };
            resolve_session(&r, folder, since, until.as_deref())?
        }
    };
    let session_id = session_id.as_str();
    let now = tp_core::now_ms();
    let cursor = match (&since, after_ts) {
        (Some(d), _) => TurnCursor::Window {
            since_ms: parse_time_bound(d, now)?,
            before_ms: until
                .as_deref()
                .map(|u| parse_time_bound(u, now))
                .transpose()?,
        },
        (None, Some(ts)) => TurnCursor::AfterTs(ts),
        (None, None) => TurnCursor::Start,
    };
    let got = tp_app::read::turns(&r, session_id, cursor, include_thinking, limit, None)?;
    if got.items.is_empty() {
        // WHY the answer is empty comes before the answer, and only here: an
        // empty read is the one case where the reason changes what it means.
        // `tp turns` handled `truncated` and never looked at `degraded`, so a
        // provider that said "I cannot read this at all" was rendered as "there
        // is nothing here" — which is how a dsh session holding 50 turns
        // reported itself as empty while `--index` printed it.
        //
        // stderr, matching `print_coverage`: the note is about the answer, not
        // part of it, and must not land in whatever is parsing stdout.
        if let Some(d) = &got.coverage.degraded {
            eprintln!("[coverage] {d}");
        }
        // "in the last 2026-08-06" is nonsense — the phrasing has to survive an
        // absolute bound, since that is now a normal thing to pass.
        match (&since, &until) {
            (Some(d), Some(u)) => println!("no turns between {d} and {u} for {session_id:?}"),
            (Some(d), None) => println!("no turns since {d} for {session_id:?}"),
            _ => println!("no turns found for {session_id:?}"),
        }
    }
    let last_ts = got.items.last().and_then(|t| t.ts);
    for t in &got.items {
        // A subagent's turn is not the operator's, and reading it as one is how
        // 23,485 turns on this machine misattributed themselves. Marked rather
        // than filtered: the content is real work worth reading, it just was not
        // said by the person whose session this is.
        let side = if t.prov.sidechain { " [subagent]" } else { "" };
        // Superseded turns stay in the output — they are real history, and for
        // pi the compaction summary that REPLACED them sits in the same list —
        // but reading one as live context is the lie the surface column exists
        // to stop. `Unknown` prints nothing: for a human, flagging every turn
        // of every old or pushed session as unknown is noise, and the MCP
        // surface (where an agent acts on the difference) does carry it.
        let dead = if t.surface == tp_core::turn::Surface::Superseded {
            " [superseded]"
        } else {
            ""
        };
        println!(
            "{} [{:?}]{side}{dead} {}",
            fmt_ts(t.ts),
            t.role,
            turn_body(t, include_thinking)
        );
        if include_thinking && !t.thinking.is_empty() {
            println!("    thinking: {}", t.thinking.replace('\n', " "));
        }
    }
    // A truncated read that looks complete is the failure this whole contract
    // exists to prevent (LLD §16 rule 3) — print the resume cursor, not just
    // the fact.
    if got.coverage.truncated {
        // The resume hint has to match the direction actually read. A window
        // read kept the NEWEST turns, so what's missing is OLDER than the first
        // one returned — telling that caller to resume with `--after-ts` would
        // page them away from the part that was dropped.
        let first_ts = got.items.first().and_then(|t| t.ts);
        match (&since, first_ts, last_ts) {
            (Some(d), Some(ts), _) => println!(
                "[truncated] kept the newest turns in the window — page BACK with: tp turns {session_id} --since {d} --until {ts}"
            ),
            (None, _, Some(ts)) => println!(
                "[truncated] stopped at the turn/byte budget — resume with: tp turns {session_id} --after-ts {ts}"
            ),
            _ => println!("[truncated] stopped at the turn/byte budget"),
        }
    }
    Ok(())
}

/// Local hits plus the selected peers', with failures always surfaced.
///
/// `only` empty means `--all`: every trusted peer. Non-empty means the caller
/// named them, which always works regardless of how many there are.
pub(crate) fn run_search_all(
    query: &str,
    since: &str,
    limit: usize,
    local: &tp_core::Retrieved<tp_core::Hit>,
    only: &[String],
) -> Result<()> {
    let me = identity()?;
    let db = Db::open(&db_path())?;

    let peers = match tp_app::fanout::select(&db, only)? {
        tp_app::Fanout::Ready { peers, no_address } => {
            for name in &no_address {
                eprintln!("[warn] {name} was not queried — no address; `tp discover` or re-pair");
            }
            peers
        }
        tp_app::Fanout::NothingReachable { no_address } => {
            for name in &no_address {
                eprintln!("[warn] {name} is trusted but has no address — `tp discover` or re-pair");
            }
            eprintln!("[warn] no reachable trusted peers; showing local results only");
            return Ok(());
        }
        // Naming a peer and getting local-only results back used to exit 0
        // here, with the name never looked at. The caller asked one machine a
        // question; silence is not an answer from it.
        tp_app::Fanout::NoneUsable {
            unmatched,
            without_address,
        } => {
            let mut msg = String::from("none of the peers you named can be searched.");
            for name in &without_address {
                msg.push_str(&format!(
                    "\n  {name} is trusted but has no address — run `tp discover` or re-pair. \
                     Retyping the name will not help."
                ));
            }
            for want in &unmatched {
                msg.push_str(&format!(
                    "\n  no trusted peer matches {want:?} — `tp peers` lists them."
                ));
            }
            anyhow::bail!(msg);
        }
        tp_app::Fanout::Ambiguous { want, matched } => anyhow::bail!(
            "{want:?} matches {} peers ({}) — use more of the id",
            matched.len(),
            matched.join(", ")
        ),
        tp_app::Fanout::TooMany { reachable } => anyhow::bail!(
            "--all would query {reachable} trusted peers, and each answers by scanning its whole \
             corpus.\nName the ones you mean instead — `tp peers` lists them, and `--peer <id>` \
             is repeatable and works at any number."
        ),
    };

    let since_ms = parse_duration(since)?.as_millis() as i64;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let fan = rt.block_on(tp_net::query_peers(&me, &peers, query, since_ms, limit))?;
    let merged = tp_app::fanout::merge(&me.device_id, &peers, local, fan);

    println!("\n── peers ──");
    for r in &merged.remote {
        let side = if r.hit.sidechain { " [subagent]" } else { "" };
        let dead = if r.hit.surface == tp_core::turn::Surface::Superseded {
            " [superseded]"
        } else {
            ""
        };
        println!(
            "{}  [{}]{side}{dead}  {}  {}",
            fmt_ts(r.hit.ts),
            r.hit.role,
            r.machine,
            r.hit.session_id
        );
        println!("    {}", r.hit.excerpt.replace('\n', " "));
    }
    if let Some(d) = merged.degraded {
        eprintln!("[coverage] {d}");
    }
    Ok(())
}

/// The `--since` a caller gets without asking. Named because the no-match path
/// must be able to tell "you chose this window" from "we chose it for you" —
/// only the second needs an apology.
pub(crate) const DEFAULT_SEARCH_SINCE: &str = "6h";

/// Move sessions older than a cutoff into an archive database.
///
/// MOVED, not deleted — which is what makes the rule "everything older than N
/// days" safe to state so simply. A quarter of this index has no transcript
/// left on disk, and age is no guide to which quarter (a runtime deletes its
/// files long before teleport would), so a policy that DELETED by age would
/// destroy the only copies first. Copying them out costs nothing but a second
/// file and leaves that question unasked.
///
/// The archive is an ordinary teleport database, same schema, its own FTS. Read
/// it with `TP_DB=<path> tp search --index`, back it up, or move it to slower
/// storage — it is not a private format.
pub(crate) fn run_archive(before: String, to: Option<PathBuf>, dry_run: bool) -> Result<()> {
    let cutoff = parse_time_bound(&before, tp_core::now_ms())?;
    let dest = to.unwrap_or_else(|| db_path().with_file_name("archive.db"));

    let mut db = Db::open(&db_path())?;
    let (sessions, turns): (i64, i64) = db.conn().query_row(
        "SELECT count(*), coalesce(sum(turn_count), 0) FROM session
          WHERE last_turn_at IS NOT NULL AND last_turn_at < ?1",
        [cutoff],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let before_bytes = std::fs::metadata(db_path()).map(|m| m.len()).unwrap_or(0);
    println!(
        "{sessions} session(s) / {turns} turn(s) last active before {before} → {}",
        dest.display()
    );
    if dry_run || sessions == 0 {
        println!("--dry-run: nothing changed");
        return Ok(());
    }
    if db_path() == tp_db::daemon_db_path() {
        if let Some(d) = tp_db::query::daemon_status(db.conn())? {
            if daemon_is_live(d.pid) {
                anyhow::bail!(
                        "tpd is running (pid {}) and would race this.\n\
                     \tlaunchctl bootout gui/$(id -u)/io.teleport.tpd\n\
                     \ttp archive --before {before}\n\
                     \tlaunchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.teleport.tpd.plist",
                    d.pid
                );
            }
        }
    }

    // Opened through the normal path so the archive gets the current schema and
    // migrations — it is a teleport database, not a dump — then closed before
    // being attached, since one process holding two handles to it is a lock
    // waiting to happen.
    Db::open(&dest)?;
    let conn = db.conn_mut();
    conn.execute(
        "ATTACH DATABASE ?1 AS arch",
        [dest.to_string_lossy().as_ref()],
    )?;
    let moved = {
        let tx = conn.transaction()?;
        // machine and runtime first: the session rows reference them, and the
        // archive's foreign keys are real.
        tx.execute_batch(
            "INSERT OR IGNORE INTO arch.machine SELECT * FROM main.machine;
             INSERT OR IGNORE INTO arch.runtime SELECT * FROM main.runtime;",
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO arch.session
             SELECT * FROM main.session WHERE last_turn_at IS NOT NULL AND last_turn_at < ?1",
            [cutoff],
        )?;
        // The archive's own FTS triggers fire on this insert, so the copy is
        // searchable there without a rebuild step.
        let n = tx.execute(
            "INSERT OR REPLACE INTO arch.turn
             SELECT t.* FROM main.turn t JOIN main.session s ON s.id = t.session_id
              WHERE s.last_turn_at IS NOT NULL AND s.last_turn_at < ?1",
            [cutoff],
        )?;
        // Turns and ingest_state go with the session row (ON DELETE CASCADE).
        tx.execute(
            "DELETE FROM main.session WHERE last_turn_at IS NOT NULL AND last_turn_at < ?1",
            [cutoff],
        )?;
        tx.commit()?;
        n
    };
    conn.execute_batch("DETACH DATABASE arch")?;
    // Deleted pages are only marked free; the file does not shrink without this,
    // and shrinking it was the point.
    conn.execute_batch("VACUUM")?;

    let after = std::fs::metadata(db_path()).map(|m| m.len()).unwrap_or(0);
    let arch_size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    println!(
        "moved {moved} turn(s); index {:.2} GB → {:.2} GB, archive {:.2} GB\n\
         read it with:  TP_DB={} tp search <query> --index",
        before_bytes as f64 / 1e9,
        after as f64 / 1e9,
        arch_size as f64 / 1e9,
        dest.display()
    );
    Ok(())
}
