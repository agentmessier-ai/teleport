//! Active-scan discovery, run periodically by `tpd`: find every live agent
//! process on this machine (Claude Code, pi) and reconcile `live_session`
//! against exactly what's found RIGHT NOW.
//!
//! Liveness comes from `ps` and nothing else. It deliberately says nothing
//! about whether a session can be WOKEN — that is asked and answered at wake
//! time, per session, by `wake`. See `scan_all` for why conflating the two
//! deleted every session running outside tmux and iTerm2.
//!
//! Recognizing every hook-registerable runtime here, not just Claude Code, is
//! load-bearing, not cosmetic: `reconcile` PRUNES any row whose pid it
//! doesn't find this cycle (see its doc) — a runtime the scan doesn't know
//! about would have its hook-registered rows silently deleted within one
//! scan interval, no matter how correctly that runtime's own hooks behaved.
//!
//! This exists alongside — not instead of — hook-based registration
//! (`resolve::register`). Passive registration alone has two real gaps:
//! (1) a session opened before the hooks were installed, or in some other
//! terminal Claude Code doesn't hook into, is invisible forever; (2) a
//! crashed/killed process whose `SessionEnd` hook never got to run stays
//! registered as live forever. The active scan is authoritative for BOTH: it
//! adds what hooks missed and removes what hooks never got to clean up — see
//! `reconcile`'s doc.
//!
//! A process the scan finds that has no existing `live_session` row gets a
//! best-effort session_id: matched by cwd against the most recently active
//! KNOWN session for that directory (same idea as LLD §7.1's "runtimes
//! without hooks fall back to inference... confidence = inferred"), or a
//! synthetic placeholder if nothing matches yet. Rows created this way are
//! tagged `source = 'scan'` and are NEVER allowed to overwrite a `'hook'`
//! row's session_id — the real id always wins (see `resolve::register`).

use crate::mailbox::now_ms;
use anyhow::Result;
use std::collections::HashMap;
use tp_db::reach;
use tp_db::DbConnection as Connection;

pub const SCAN_INTERVAL_SECS: u64 = 60;

/// A live agent process the scan found. Says nothing about reachability —
/// `tty` is where it runs, not a promise that anything can write there.
#[derive(Debug, Clone)]
pub struct ScannedProcess {
    pub pid: i32,
    pub tty: String,
    /// Teleport runtime id this process belongs to — "claude_code" or "pi"
    /// today (see `recognize_runtime`).
    pub runtime: String,
    /// Best-effort — `lsof` per matched pid; `None` if that failed or the
    /// process exited between the tty sweep and this lookup.
    pub cwd: Option<String>,
}

/// Every LIVE agent process on this machine, whatever terminal it sits in.
///
/// This used to intersect `ps` with the ttys tmux and iTerm2 reported, and
/// return only what was left — "every currently-injectable pane/session". That
/// conflated two different questions, and `reconcile` consumes the answer as if
/// it were the first:
///
/// * is this process ALIVE? — `ps` answers it, for every terminal that exists
///   or ever will.
/// * can I WAKE it? — only tmux/iTerm2 can answer, and only for the terminals
///   teleport has integrated.
///
/// Intersecting them made an UNINTEGRATED TERMINAL indistinguishable from a
/// DEAD PROCESS. Observed live: a `pi` started from Terminal.app registered
/// correctly via its hook, appeared in the panel, and was deleted within one
/// scan interval — its tty was in neither set, so the scan reported it as not
/// found and `reconcile` pruned the row. Every non-tmux, non-iTerm2 terminal
/// had the same fate: Warp, Ghostty, kitty, VS Code's terminal, a bare ssh
/// session.
///
/// Nothing needed the intersection. Waking does its own lookup by tty at the
/// moment it wakes — `wake::iterm_write_text` walks iTerm2 itself and reports
/// `not-found` if the window is gone — as do `resolve::iterm_session_exists_for_tty`
/// and the panel's focus. The enumeration here was the only copy whose result
/// fed liveness, and dropping it costs no reachability that was ever real:
/// an unreachable session now stays visible and fails honestly AT WAKE TIME,
/// instead of silently ceasing to exist a minute after it starts.
///
/// It also leaves `tpd` with no AppleScript at all, which is what its own
/// header comment has always claimed (LLD §7.4: a LaunchAgent is the wrong
/// place to depend on Automation).
pub fn scan_all(sigs: &[ProcessSignature]) -> Vec<ScannedProcess> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-eo", "pid=,tty=,comm="])
        .output()
    else {
        return Vec::new();
    };
    let found = agents_from_ps(&String::from_utf8_lossy(&out.stdout), sigs);
    found
        .into_iter()
        .map(|(tty, pid, runtime)| ScannedProcess {
            cwd: cwd_of_pid(pid),
            pid,
            tty,
            runtime,
        })
        .collect()
}

/// The pure half of `scan_all`: `ps` output in, recognized agents out.
///
/// Split out so the selection rule is testable without spawning anything —
/// the rule is the part that was wrong, and the part a future change could
/// quietly narrow again.
///
/// Keyed by normalized tty (`ttys007`, no `/dev/` prefix) because that is the
/// form `wake` and `resolve` match against.
fn agents_from_ps(ps_output: &str, sigs: &[ProcessSignature]) -> Vec<(String, i32, String)> {
    let mut seen: HashMap<String, (i32, String)> = HashMap::new();
    for line in ps_output.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(tty), Some(comm)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let Some(runtime) = recognize_runtime(comm, sigs) else {
            continue;
        };
        // No controlling terminal: nothing to wake, and nothing `wake` could
        // ever address. A harness with no tty registers itself through the
        // `exec:`/loopback delivery path instead (`resolve::register_with`),
        // which the scan is not authoritative for.
        if tty == "??" || tty == "?" {
            continue;
        }
        let Ok(pid) = pid.parse::<i32>() else {
            continue;
        };
        seen.insert(
            tty.trim_start_matches("/dev/").to_string(),
            (pid, runtime.to_string()),
        );
    }
    seen.into_iter()
        .map(|(tty, (pid, runtime))| (tty, pid, runtime))
        .collect()
}

/// How a scannable harness is recognized in `ps` output.
///
/// Supplied by the caller from each harness descriptor's
/// `capabilities.process_match`, not hardcoded here: teleport used to decide
/// which runtimes exist by an `if lower.contains("claude")` chain, which made
/// adding a scannable runtime a Rust change and a release. A harness that
/// registers itself needs no signature at all — this is only how one can be
/// found WITHOUT cooperation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSignature {
    pub runtime_id: String,
    /// `comm` pattern. A leading `=` anchors an exact match; otherwise it is a
    /// case-insensitive substring. Both forms are load-bearing and were each
    /// verified live: `claude` must be substring (a dev build named
    /// `claude-local` is missed by equality, silently excluding it from
    /// `tp live`), and `pi` must be exact (a substring hits `pip`,
    /// `gpio-tool`, and anything else containing "pi").
    pub pattern: String,
}

impl ProcessSignature {
    fn matches(&self, comm: &str) -> bool {
        let comm = comm.to_lowercase();
        match self.pattern.strip_prefix('=') {
            Some(exact) => comm == exact.to_lowercase(),
            None => comm.contains(&self.pattern.to_lowercase()),
        }
    }
}

/// Map a process's `comm` to a runtime id using the supplied signatures.
/// First match wins, so caller order is the precedence.
fn recognize_runtime<'a>(comm: &str, sigs: &'a [ProcessSignature]) -> Option<&'a str> {
    sigs.iter()
        .find(|s| s.matches(comm))
        .map(|s| s.runtime_id.as_str())
}

/// `lsof -a -d cwd -p <pid> -Fn` → the `n`-prefixed line is the path. macOS
/// has no `/proc`, so this is the standard way to get another process's cwd.
fn cwd_of_pid(pid: i32) -> Option<String> {
    let out = std::process::Command::new("lsof")
        .args(["-a", "-d", "cwd", "-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(str::to_string)
}

/// Reconcile `live_session` against exactly what `scan_all()` found this
/// cycle: refresh or create a row for every process found, and DELETE any
/// row — hook- or scan-sourced — whose pid ISN'T among them. The scan is
/// authoritative for liveness: a hook-registered row whose process the scan
/// can no longer find (crashed, killed -9, machine slept through its
/// SessionEnd) is exactly as stale as an unmatched scan row, and gets pruned
/// the same way, closing the gap passive registration alone leaves open.
/// What an EMPTY scan result is allowed to mean.
///
/// `scan_all` degrades silently to an empty list on failure: `ps` not
/// executing returns one. An empty result is therefore two completely
/// different facts wearing one shape: "nothing is running" and "I could not
/// look".
///
/// `reconcile` used to read it only as the first, and act on it: an empty scan
/// deletes EVERY `presence = 'scan'` row, which is every Claude Code and pi
/// session on the machine, hook-registered or not. One AppleScript hiccup made
/// them all unaddressable — `tp ask` answering PARKED, NOT DELIVERED for a
/// session whose process never stopped running — until a later cycle rebuilt
/// them, and permanently downgraded their `source` from `hook` to `scan` on the
/// way, since only a hook firing writes that column back.
///
/// The rest of this module already refuses to make this mistake for `declared`
/// rows: "pruning it here would delete a correct registration the scan was never
/// able to observe". The scan's own blindness deserves the same treatment.
///
/// This guard only ever covered TOTAL blindness, and the scan used to have a
/// PARTIAL kind it could not express: with tmux answering and iTerm2 not, the
/// result was non-empty, so `Authoritative` applied and every session in the
/// unreadable terminal was pruned as dead. `scan_all` no longer intersects
/// with any terminal, so that shape is gone — the only remaining way to see
/// nothing is for `ps` itself to fail, which this handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyScan {
    /// The caller has established that finding nothing means nothing is there.
    /// `tpd` requires two consecutive empty cycles before claiming this — the
    /// same grace-then-act shape `sweep_declared` uses, and for the same reason:
    /// a transient failure must cost a delay, never state.
    Authoritative,
    /// Finding nothing may just mean the scan could not see. Refresh what was
    /// found and prune nothing.
    Unverified,
}

pub fn reconcile(
    conn: &Connection,
    machine_id: &str,
    found: &[ScannedProcess],
    empty: EmptyScan,
) -> Result<()> {
    let now = now_ms();
    let mut live_pids = Vec::with_capacity(found.len());

    for p in found {
        live_pids.push(p.pid);
        // `session_id` is the primary key, not `pid` — a pid CAN transiently
        // own more than one row: the scan creates a `scan-pid-N` placeholder
        // before that process's own hook (if it has one) gets a chance to
        // register the real id (verified live: `/reload`-ing a hook-having
        // runtime after its process had already been scan-discovered left
        // both the placeholder AND the real row, both keyed to the same
        // pid). Fetch every row for this pid, keep exactly one — a real
        // `hook` row always wins over any `scan` row; among ties, the most
        // recently registered — and delete the rest before doing anything else.
        // `presence = 'scan'` only: a declared row may legitimately share a pid
        // with many others (one dsh host process serves many sessions), so the
        // keep-exactly-one rule below would collapse N registrations into 1.
        let mut existing = reach::scan_rows_for_pid(conn, p.pid)?;
        existing.sort_by(|a, b| {
            let hook_rank = |source: &str| if source == "hook" { 0 } else { 1 };
            hook_rank(&a.source)
                .cmp(&hook_rank(&b.source))
                .then(b.registered_at.cmp(&a.registered_at)) // hook first, then newest first
        });
        for dupe in existing.iter().skip(1) {
            reach::delete_session(conn, &dupe.session_id)?;
        }

        match existing.first() {
            // A row (hook- or scan-sourced) already tracks this pid — just
            // refresh its liveness/location, NEVER its session_id (that
            // would silently reassign a real, hook-provided id).
            Some(row) => {
                reach::touch_location(conn, &row.session_id, Some(&p.tty), p.cwd.as_deref(), now)?;
            }
            None => {
                let sid = infer_session_id(conn, machine_id, p)?;
                reach::insert_scanned(conn, &sid, p.pid, Some(&p.tty), p.cwd.as_deref(), now)?;
                bind_scanned_to_conversation(conn, machine_id, &sid, p, now);
            }
        }
        // Also for the refresh branch above: a row the scan is merely touching
        // may still predate its process's conversation, which is precisely the
        // case that lost a message — an id resurrected by cwd inference, live
        // and wakeable, belonging to no conversation and therefore drained by
        // nobody.
        if let Some(row) = existing.first() {
            bind_scanned_to_conversation(conn, machine_id, &row.session_id, p, now);
        }
    }

    // The scan's delete authority is scoped to rows that declared themselves
    // scannable (docs/reach-provider.md). The rule used to be "the scan is
    // authoritative for liveness"; it is now "the scan is authoritative only for
    // sessions that said it could see them". A `declared` row is owned by its
    // runtime and expires on a heartbeat timeout instead — pruning it here would
    // delete a correct registration the scan was never able to observe, which is
    // exactly the bug pi hit before `recognize_runtime` learned about it.
    if live_pids.is_empty() && empty == EmptyScan::Unverified {
        return Ok(());
    }
    reach::prune_scan_rows(conn, &live_pids)
}

/// Attach a scanned row to whatever conversation already owns its process.
///
/// Never fatal: discovery's job is to keep `live_session` true, and a session
/// that is reachable but not yet grouped is strictly better than one the scan
/// dropped over a bookkeeping error.
fn bind_scanned_to_conversation(
    conn: &Connection,
    machine_id: &str,
    session_id: &str,
    p: &ScannedProcess,
    now: i64,
) {
    let start = crate::resolve::process_start(p.pid);
    let key = reach::ConversationKey {
        machine_id,
        runtime_id: &p.runtime,
        pid: p.pid,
        pid_start: start.as_deref(),
        cwd: p.cwd.as_deref(),
    };
    if let Err(e) = reach::join_existing_conversation(conn, session_id, key, now) {
        tp_core::log_warn!("scan could not bind {session_id} to a conversation: {e:#}");
    }
}

/// The most recently active KNOWN session whose `cwd` exactly matches — its
/// `id` is already the composite form, so it's returned as-is. Falls back to
/// a synthetic, still-unique id (`scan-pid-<pid>`) when nothing matches: a
/// process teleport genuinely can't identify yet still gets tracked for
/// presence/counting purposes, just not usefully targetable by a human until
/// its conversation gets ingested and a later scan cycle re-resolves it.
fn infer_session_id(conn: &Connection, machine_id: &str, p: &ScannedProcess) -> Result<String> {
    if let Some(cwd) = &p.cwd {
        // Scoped to the SAME runtime — a pi process sharing a cwd with an
        // indexed Claude Code session must not be mistaken for it.
        if let Some(id) = tp_db::query::latest_session_for_cwd(conn, machine_id, &p.runtime, cwd)? {
            return Ok(id);
        }
    }
    Ok(format!("{machine_id}/{}/scan-pid-{}", p.runtime, p.pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_db::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();
        db
    }

    fn scanned(pid: i32, tty: &str, runtime: &str, cwd: Option<&str>) -> ScannedProcess {
        ScannedProcess {
            pid,
            tty: tty.to_string(),
            runtime: runtime.to_string(),
            cwd: cwd.map(str::to_string),
        }
    }

    fn sigs() -> Vec<ProcessSignature> {
        vec![
            ProcessSignature {
                runtime_id: "claude_code".into(),
                pattern: "claude".into(),
            },
            ProcessSignature {
                runtime_id: "pi".into(),
                pattern: "=pi".into(),
            },
        ]
    }

    /// THE regression this refactor exists for.
    ///
    /// `scan_all` used to return only processes whose tty tmux or iTerm2
    /// claimed, and `reconcile` deletes any registered session the scan does
    /// not return — so an agent in any other terminal was registered by its
    /// hook, shown in the panel, and pruned within one 60s cycle. Observed on
    /// a real machine with a `pi` started from Terminal.app on ttys013.
    ///
    /// Liveness must come from `ps` alone. A terminal teleport cannot inject
    /// into is a session it cannot WAKE — never a session that is not RUNNING.
    #[test]
    fn an_agent_in_an_unintegrated_terminal_is_still_alive() {
        // ttys013 is Terminal.app's; neither tmux nor iTerm2 would report it.
        let ps = "77810 ttys013  pi\n98275 ttys002  claude\n";
        let found = agents_from_ps(ps, &sigs());

        let ttys: Vec<&str> = found.iter().map(|(t, _, _)| t.as_str()).collect();
        assert!(
            ttys.contains(&"ttys013"),
            "an agent in a terminal teleport has no integration for must still \
             count as alive — it is unwakeable, not dead: {found:?}"
        );
        assert!(ttys.contains(&"ttys002"), "{found:?}");
        assert_eq!(found.len(), 2);
    }

    /// A process with no controlling terminal has nothing `wake` could ever
    /// address, and registers through the `exec:`/loopback path instead.
    #[test]
    fn a_process_with_no_tty_is_not_scanned() {
        let ps = "999 ??  claude\n1000 ?  pi\n77810 ttys013  pi\n";
        let found = agents_from_ps(ps, &sigs());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "ttys013");
    }

    #[test]
    fn unrecognized_processes_are_ignored_and_dev_ttys_are_normalized() {
        let ps = "1 ttys001  bash\n2 /dev/ttys004  claude\n";
        let found = agents_from_ps(ps, &sigs());
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, "ttys004", "the /dev/ prefix must be stripped");
        assert_eq!(found[0].2, "claude_code");
    }

    #[test]
    fn recognize_runtime_matches_dev_builds_by_substring_but_pi_by_exact_name() {
        // The two match modes are load-bearing and each was verified live. They
        // now come from descriptors rather than a hardcoded chain, so this
        // asserts the MECHANISM honours both — the patterns themselves are
        // asserted against the shipped TOMLs in tp-ingest.
        let sigs = vec![
            ProcessSignature {
                runtime_id: "claude_code".into(),
                pattern: "claude".into(), // substring
            },
            ProcessSignature {
                runtime_id: "pi".into(),
                pattern: "=pi".into(), // exact
            },
        ];
        let r = |comm: &str| recognize_runtime(comm, &sigs);

        // Regression: an exact `== "claude"` check misses a locally-built
        // binary named `claude-local`, silently excluding it from `tp live`
        // with no error — substring closes that.
        assert_eq!(r("claude"), Some("claude_code"));
        assert_eq!(r("claude-local"), Some("claude_code"));
        assert_eq!(r("Claude"), Some("claude_code"));

        assert_eq!(r("pi"), Some("pi"));
        // "pi" is deliberately EXACT — too short to substring-match without
        // false positives.
        assert_eq!(r("pip"), None);
        assert_eq!(r("gpio-tool"), None);
        assert_eq!(
            r("node"),
            None,
            "the interpreter alone must not match — comm is agent-named specifically"
        );
    }

    /// A harness that declares no signature is simply not discoverable without
    /// cooperation — it must not accidentally match everything or nothing-by-panic.
    #[test]
    fn an_empty_signature_table_recognizes_nothing() {
        assert_eq!(recognize_runtime("claude", &[]), None);
        assert_eq!(recognize_runtime("anything", &[]), None);
    }

    /// A single empty scan must NOT be believed.
    ///
    /// `scan_all` degrades silently to an empty list whenever one of its three
    /// subprocesses misbehaves — `ps` failing returns an empty map, tmux and
    /// osascript each return an empty vec. Before this, `reconcile` read that as
    /// "nothing is running" and executed `DELETE FROM live_session WHERE
    /// presence = 'scan'`, taking out every Claude Code and pi session on the
    /// machine, hook-registered ones included, for a transient AppleScript
    /// hiccup. They came back a cycle later as `source = 'scan'`, having lost
    /// the provenance only a hook can write — which is how a machine full of
    /// hook-registered sessions ends up showing rows that claim they were merely
    /// scanned.
    ///
    /// The damage in between is the part that matters: `tp ask` answers
    /// PARKED, NOT DELIVERED for a session whose process never stopped running.
    #[test]
    fn an_unverified_empty_scan_prunes_nothing() {
        let db = setup();
        reconcile(
            db.conn(),
            "m1",
            &[scanned(4242, "ttys001", "claude_code", None)],
            EmptyScan::Authoritative,
        )
        .unwrap();
        let before: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 1);

        // The scan came back empty because it could not look, not because the
        // machine went idle. Nothing may be deleted on that basis.
        reconcile(db.conn(), "m1", &[], EmptyScan::Unverified).unwrap();
        let after: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, 1,
            "one empty scan is indistinguishable from a failed one — pruning on it \
             unregisters every live session on the machine"
        );

        // Confirmed empty on a second consecutive cycle: now it is a fact.
        reconcile(db.conn(), "m1", &[], EmptyScan::Authoritative).unwrap();
        let confirmed: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            confirmed, 0,
            "a CONFIRMED empty scan must still prune — the guard is a delay, not an exemption"
        );
    }

    #[test]
    fn scan_only_row_is_pruned_when_no_longer_found() {
        let db = setup();
        let found = vec![scanned(111, "ttys999", "claude_code", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "a first-seen process must be tracked");

        // Next cycle finds nothing — the scan is authoritative, so it prunes.
        reconcile(db.conn(), "m1", &[], EmptyScan::Authoritative).unwrap();
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "a process no longer found must be pruned, even though it was never hook-registered or explicitly unregistered");
    }

    #[test]
    fn hook_registered_row_is_pruned_when_the_process_disappears() {
        let db = setup();
        crate::resolve::register(
            db.conn(),
            "m1/claude_code/real-sess",
            222,
            Some("/dev/ttys998"),
            None,
        )
        .unwrap();

        // Scan doesn't see pid 222 this cycle (process crashed, SessionEnd never fired).
        reconcile(db.conn(), "m1", &[], EmptyScan::Authoritative).unwrap();
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "the scan must prune a hook-registered row too once the process is gone — it's authoritative for liveness, not just for scan-sourced rows");
    }

    /// The generalization of the pi regression below. pi was saved by teaching
    /// `recognize_runtime` about it — a fix that only works for a runtime with a
    /// recognizable process. A harness the scan CANNOT see by construction (dsh's
    /// web profile: sessions live in a browser, no tty, and one host process
    /// serves many of them) has no signature to add, so the fix has to be that
    /// the scan does not claim authority it does not have.
    #[test]
    fn a_declared_session_survives_a_scan_that_cannot_see_it() {
        let db = setup();
        crate::resolve::register_with(
            db.conn(),
            "m1/dsh/session-abc",
            4242,
            None, // no tty — this is the whole point
            Some("/w"),
            crate::resolve::Presence::Declared,
            Some("http://127.0.0.1:8125/teleport/wake"),
        )
        .unwrap();

        // A scan cycle that finds something else entirely, and never this pid.
        let found = vec![scanned(999, "ttys000", "claude_code", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();

        let survived: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM live_session WHERE session_id = ?1",
                ["m1/dsh/session-abc"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            survived, 1,
            "a declared session must not be pruned by a scan that cannot observe it"
        );
    }

    /// A multiplexed host registers many sessions on ONE pid. The keep-exactly-
    /// one-row-per-pid rule is correct for one-session-per-process runtimes and
    /// would silently collapse N registrations into 1 here.
    #[test]
    fn declared_sessions_may_share_a_pid() {
        let db = setup();
        for id in ["m1/dsh/s1", "m1/dsh/s2", "m1/dsh/s3"] {
            crate::resolve::register_with(
                db.conn(),
                id,
                7000, // same host process for all three
                None,
                Some("/w"),
                crate::resolve::Presence::Declared,
                None,
            )
            .unwrap();
        }

        reconcile(
            db.conn(),
            "m1",
            &[scanned(7000, "ttys001", "claude_code", None)],
            EmptyScan::Authoritative,
        )
        .unwrap();

        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM live_session WHERE runtime_id = 'dsh'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3, "one host process may own many declared sessions");
    }

    #[test]
    fn a_hook_registered_pi_session_survives_a_scan_cycle() {
        // Regression: the scan used to only recognize `claude`-named
        // processes, so a hook-registered `pi` session's pid never appeared
        // in `found` and got silently pruned within one scan interval
        // (SCAN_INTERVAL_SECS) — correct hook behavior on pi's side couldn't
        // save it. `recognize_runtime` now covers pi too.
        let db = setup();
        crate::resolve::register(
            db.conn(),
            "m1/pi/real-pi-sess",
            999,
            Some("/dev/ttys000"),
            None,
        )
        .unwrap();

        let found = vec![scanned(999, "ttys000", "pi", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "a hook-registered pi session found by the scan must survive"
        );
    }

    #[test]
    fn stale_scan_placeholder_is_cleaned_up_once_the_real_hook_row_appears() {
        // Regression, verified live: a process gets scan-discovered BEFORE
        // its own hook fires (e.g. `pi` was already running when `/reload`
        // loaded the extension) → a `scan-pid-N` placeholder row is created.
        // The hook then fires and INSERTs a SEPARATE row under the real id
        // (different primary key, same pid) — without dedup, both rows sit
        // in `live_session` forever, one of them a phantom `tp live` will
        // keep showing. The very next reconcile cycle must clean this up.
        let db = setup();
        // Cycle 1: scan discovers the process first, no hook row exists yet.
        let found = vec![scanned(777, "ttys111", "pi", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let placeholder: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 777",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(placeholder, "m1/pi/scan-pid-777");

        // The hook now fires for the SAME pid (its own INSERT, separate PK).
        crate::resolve::register(
            db.conn(),
            "m1/pi/real-sess-from-hook",
            777,
            Some("/dev/ttys111"),
            None,
        )
        .unwrap();
        let count_before: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM live_session WHERE pid = 777",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count_before, 2,
            "sanity check: both rows must coexist immediately after the hook's separate INSERT"
        );

        // Cycle 2: the scan runs again and must dedup down to the real row.
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let rows: Vec<(String, String)> = db
            .conn()
            .prepare("SELECT session_id, source FROM live_session WHERE pid = 777")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![("m1/pi/real-sess-from-hook".to_string(), "hook".to_string())],
            "the scan placeholder must be deleted, leaving only the real hook row"
        );
    }

    #[test]
    fn scan_never_overwrites_a_hook_provided_session_id() {
        let db = setup();
        crate::resolve::register(
            db.conn(),
            "m1/claude_code/real-sess",
            333,
            Some("/dev/ttys997"),
            None,
        )
        .unwrap();

        // Same pid shows up in a scan cycle (this IS the hook-registered process).
        let found = vec![scanned(333, "ttys997", "claude_code", Some("/some/dir"))];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();

        let (sid, source): (String, String) = db
            .conn()
            .query_row(
                "SELECT session_id, source FROM live_session WHERE pid = 333",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            sid, "m1/claude_code/real-sess",
            "the real hook-provided session_id must survive a scan cycle unchanged"
        );
        assert_eq!(
            source, "hook",
            "source must stay 'hook', not get relabeled 'scan'"
        );
    }

    #[test]
    fn unmatched_cwd_falls_back_to_a_synthetic_but_stable_id_per_runtime() {
        let db = setup();
        let found = vec![scanned(
            444,
            "ttys996",
            "claude_code",
            Some("/no/such/known/project"),
        )];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let sid: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 444",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sid, "m1/claude_code/scan-pid-444");
    }

    #[test]
    fn unmatched_pi_process_gets_its_own_runtime_in_the_synthetic_id() {
        let db = setup();
        let found = vec![scanned(446, "ttys994", "pi", None)];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let sid: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 446",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sid, "m1/pi/scan-pid-446",
            "a pi process must never be composed under the claude_code runtime"
        );
    }

    #[test]
    fn known_cwd_resolves_to_the_real_session_id() {
        let db = setup();
        db.conn()
            .execute(
                "INSERT INTO session(id, machine_id, runtime_id, native_id, cwd, last_turn_at) VALUES (?1, 'm1', 'claude_code', 'native-abc', '/Users/me/proj', 1000)",
                ["m1/claude_code/native-abc"],
            )
            .unwrap();
        let found = vec![scanned(
            555,
            "ttys995",
            "claude_code",
            Some("/Users/me/proj"),
        )];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let sid: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 555",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sid, "m1/claude_code/native-abc",
            "a matching cwd must resolve to the REAL indexed session, not a synthetic id"
        );
    }

    #[test]
    fn cwd_match_is_scoped_to_the_same_runtime() {
        // A pi process sharing a cwd with an indexed Claude Code session must
        // NOT be mistaken for that Claude Code session.
        let db = setup();
        db.conn()
            .execute(
                "INSERT INTO session(id, machine_id, runtime_id, native_id, cwd, last_turn_at) VALUES (?1, 'm1', 'claude_code', 'native-abc', '/shared/dir', 1000)",
                ["m1/claude_code/native-abc"],
            )
            .unwrap();
        let found = vec![scanned(666, "ttys993", "pi", Some("/shared/dir"))];
        reconcile(db.conn(), "m1", &found, EmptyScan::Authoritative).unwrap();
        let sid: String = db
            .conn()
            .query_row(
                "SELECT session_id FROM live_session WHERE pid = 666",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            sid, "m1/pi/scan-pid-666",
            "must not cross-match a claude_code session just because the cwd matches"
        );
    }
}
