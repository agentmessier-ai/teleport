//! Live-session resolution (LLD §7.2): `session_id → pid → tty → pane`.
//!
//! The cached `live_session.tty` is a HINT only — the pid is re-checked for
//! liveness and the tty re-derived at wake time, so a stale cache can never
//! land a message in the wrong terminal. Pane ids are never cached (tmux
//! resurrect/renumber drifts them).

use crate::mailbox;
use crate::terminal;
use anyhow::{bail, Result};
use tp_db::reach;
use tp_db::DbConnection as Connection;

/// A delivery channel a runtime declared for itself (docs/reach-provider.md).
///
/// Two forms, deliberately only two. `Exec` is the primitive every harness can
/// satisfy — no listening socket, no port, no auth. `Http` exists because a
/// harness that already runs a server (dsh's web profile) can serve it for free
/// and spawning a process per wake would be wasteful. Kubernetes settled on a
/// similarly small closed set for probes (`exec`, `httpGet`, `tcpSocket`,
/// `grpc`) and a decade of production has not needed a pluggable transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryChannel {
    /// Spawn this argv. The control string arrives on stdin.
    Exec(Vec<String>),
    /// POST to this loopback URL.
    Http(String),
}

impl DeliveryChannel {
    /// Parse a stored `deliver` value.
    ///
    /// An `http:` channel MUST be loopback. The check is here rather than left
    /// to the descriptor because a channel is a thing that makes an agent act:
    /// a non-loopback URL would let anything routable poke a session, and a
    /// harness's own config file is not the right place to be trusted about
    /// that. Rejecting is lossless — there is no legitimate remote channel,
    /// since cross-machine reach dispatches through the peer's own daemon.
    pub fn parse(raw: &str) -> Result<Self> {
        if let Some(argv) = raw.strip_prefix("exec:") {
            let parts: Vec<String> = argv.split_whitespace().map(str::to_string).collect();
            if parts.is_empty() {
                bail!("empty exec: channel");
            }
            return Ok(DeliveryChannel::Exec(parts));
        }
        if raw.starts_with("http://") || raw.starts_with("https://") {
            let host = raw
                .split("://")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .map(|hostport| hostport.rsplit_once(':').map_or(hostport, |(h, _)| h))
                .unwrap_or_default();
            if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
                bail!("delivery channel must be loopback, got {host:?} in {raw:?}");
            }
            return Ok(DeliveryChannel::Http(raw.to_string()));
        }
        bail!(
            "unrecognized delivery channel {raw:?} (want `exec:<argv>` or a loopback http:// URL)"
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A tmux pane id, e.g. `%3`. No TCC required; safe from any process
    /// including `tpd`.
    Tmux(String),
    /// A terminal teleport can type into, identified by its tty and by WHICH
    /// descriptor claimed it (`terminal::TerminalConfig::id`) — AppleScript
    /// re-matches at wake time, same as tmux.
    ///
    /// Was `ITerm(String)`, one hardcoded backend. The id is carried rather
    /// than re-derived because resolving asks every descriptor in turn, and
    /// making `wake` ask again could get a different answer than the one this
    /// target was built from.
    ///
    /// Requires the CALLING process to hold — or be able to inherit — the
    /// Automation TCC grant (see `Backends` in `wake.rs` and
    /// docs/same-machine-poke-design.md Gap 2): safe from a CLI process
    /// launched inside a terminal, NEVER safe from `tpd` (a bare LaunchAgent).
    Terminal { id: String, tty: String },
    /// A channel the runtime declared for itself. teleport does not know what is
    /// on the other end and does not need to: it delivers the same fixed control
    /// string a pane would receive, and the runtime drains its own inbox.
    Channel(DeliveryChannel),
    /// A bare tty we can't inject into by any backend — mailbox-only.
    Unreachable,
    /// Not registered or not alive.
    NotLive,
}

/// Register (or refresh) a live session binding — the SessionStart hook path.
/// Always marks the row `source = 'hook'`: a REAL session_id from Claude Code
/// itself outranks anything the active scan (`discover.rs`) may have
/// previously inferred for the same id.
pub fn register(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
    cwd: Option<&str>,
) -> Result<()> {
    register_with(conn, session_id, pid, tty, cwd, Presence::Scan, None)
}

/// Which liveness regime governs a session (docs/reach-provider.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// The process scan is authoritative — it may create and prune this row.
    /// Every runtime teleport shipped before descriptors existed.
    Scan,
    /// The runtime owns its own liveness and renews it by heartbeat. The scan
    /// must neither create nor prune the row; it expires on timeout instead.
    /// For a harness the scan cannot see at all — a web GUI with no tty, a host
    /// multiplexing many sessions onto one pid.
    Declared,
}

impl Presence {
    fn as_str(self) -> &'static str {
        match self {
            Presence::Scan => "scan",
            Presence::Declared => "declared",
        }
    }
}

/// How long a `declared` session may go without a heartbeat before it is marked
/// stale. Sized well above a sane heartbeat interval so one missed beat — a GC
/// pause, a busy event loop — does not flip a healthy session.
pub const PRESENCE_TTL_MS: i64 = 90_000;

/// How long a session stays marked stale before the row is actually deleted.
///
/// Deliberately much longer than `PRESENCE_TTL_MS`, and the ratio is the point:
/// Kubernetes marks a node unreachable after ~40s but does not evict its pods
/// until `tolerationSeconds` (300s, 7.5x) has also passed, because a brief stall
/// should not cause real damage. A laptop waking from sleep must not lose every
/// declared registration to a 90-second gap.
pub const PRESENCE_EVICT_AFTER_MS: i64 = 10 * 60_000;

/// Renew a declared session's presence. Writes `last_seen_at` and clears any
/// stale mark — nothing else.
///
/// Deliberately not a re-registration: Kubernetes split the light `Lease` object
/// out of the heavy `Node.status` for exactly this reason — rewriting the big
/// object every interval is the expensive part, and a heartbeat is a liveness
/// signal, not a restatement of cwd, tty and channel.
///
/// Returns whether a row was actually renewed, so a runtime heartbeating a
/// session teleport has already evicted learns that it must re-register rather
/// than beating into the void.
pub fn heartbeat(conn: &Connection, session_id: &str) -> Result<bool> {
    Ok(reach::touch_heartbeat(conn, session_id, mailbox::now_ms())? > 0)
}

/// Two-stage expiry for `declared` sessions: mark, then evict much later.
///
/// Stage one is cheap and reversible — the next heartbeat clears it, and a
/// stale row is still addressable, so `tp ask` parks a message in the mailbox
/// rather than failing. Stage two is the destructive one and waits far longer.
///
/// A row is never marked before it has had one full TTL to send its first beat.
/// Camel's health registry has `setInitialState` for the same reason: a check
/// that has not run yet must not report DOWN and cause a false-negative
/// eviction during startup.
///
/// Returns `(marked, evicted)`.
pub fn sweep_declared(conn: &Connection) -> Result<(usize, usize)> {
    let now = mailbox::now_ms();
    let marked = reach::mark_stale(conn, now, now - PRESENCE_TTL_MS)?;
    let evicted = reach::evict_stale(conn, now - PRESENCE_EVICT_AFTER_MS)?;
    Ok((marked, evicted))
}

/// Register with an explicit presence regime and delivery channel.
///
/// `deliver` is `None` for the pane path (infer a tmux pane or iTerm2 tty from
/// pid/tty, today's behaviour); a harness with no tty declares `exec:<argv>` or
/// a loopback `http://…` instead.
pub fn register_with(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
    cwd: Option<&str>,
    presence: Presence,
    deliver: Option<&str>,
) -> Result<()> {
    // `runtime_id` is the composite id's middle segment, stored so the sweep and
    // `tp live` can filter without parsing a composite id in SQL.
    let runtime_id = session_id.split('/').nth(1);
    let now = mailbox::now_ms();
    reach::upsert_registration(
        conn,
        session_id,
        pid,
        tty,
        cwd,
        presence.as_str(),
        deliver,
        runtime_id,
        now,
    )?;

    // Bind this segment to a conversation — the address that survives the next
    // compaction. Registration is the only moment teleport can see a rotation
    // happen: the transcripts carry no link between the id that ended and the
    // id that replaced it, so the continuity has to be OBSERVED, from the same
    // process re-registering under a new name, or it cannot be known at all.
    //
    // A failure here must not fail the registration. Being reachable under the
    // segment id is the guarantee that existed before conversations did, and
    // losing it to a bookkeeping error would trade a stale address for no
    // address.
    if let (Some(machine_id), Some(runtime_id)) = (session_id.split('/').next(), runtime_id) {
        let minted = format!("{machine_id}/{runtime_id}/conv-{}", uuid::Uuid::new_v4());
        let start = process_start(pid);
        let key = reach::ConversationKey {
            machine_id,
            runtime_id,
            pid,
            pid_start: start.as_deref(),
            cwd,
        };
        if let Err(e) = reach::join_conversation(conn, session_id, key, now, &minted) {
            tp_core::log_warn!("session registered, but conversation binding failed: {e:#}");
        }
    }
    Ok(())
}

/// Unregister a live session binding — the SessionEnd hook path.
///
/// A no-op if the stored binding's `pid` doesn't match `expected_pid`: if
/// `session_id` gets REUSED across a `/clear` (SessionEnd for the old
/// incarnation racing SessionStart for the new one — see
/// docs/same-machine-poke-design.md's ordering risk), a second registration
/// may already have overwritten the row by the time the first process's
/// SessionEnd hook runs. Deleting unconditionally in that case would
/// unregister the WRONG (newer) live session by id collision. Pinning to the
/// pid the unregistering process itself resolves (the same
/// `find_session_process` walk `register` uses) means only the process that
/// actually owned that binding can remove it. `None` skips the check — used
/// by tests and any manual `tp unregister` invocation that has no pid to
/// compare against.
pub fn unregister(conn: &Connection, session_id: &str, expected_pid: Option<i32>) -> Result<()> {
    match expected_pid {
        Some(pid) => reach::delete_session_pinned(conn, session_id, pid),
        None => reach::delete_session(conn, session_id),
    }
}

/// Resolve a session to an injectable target. Always re-verifies:
///   1. pid is alive (kill(pid, 0))
///   2. pid → tty freshly (via /proc-less macOS: `lsof`-style is heavy; we use
///      `ps -o tty=` which is cheap and reliable)
///   3. tty → tmux pane, THEN tty → iTerm2 session (never cached)
pub fn resolve(conn: &Connection, session_id: &str) -> Result<Target> {
    let Some(row) = reach::target_row(conn, session_id)? else {
        return Ok(Target::NotLive);
    };

    // A declared channel wins over pane inference — it is the runtime's own
    // statement about how to reach it, and for a harness with no tty there is
    // nothing to infer. A stale row is deliberately NOT woken: the message
    // still lands in the mailbox, and waking a session whose host has gone
    // quiet would spend a delivery attempt on nothing.
    if let Some(raw) = row.deliver {
        if row.stale_at.is_some() {
            return Ok(Target::NotLive);
        }
        // A malformed or non-loopback channel is not a reason to fall back to
        // pane injection — the runtime told us it has no pane. Report it.
        return DeliveryChannel::parse(&raw).map(Target::Channel);
    }

    if !process_alive(row.pid) {
        unregister(conn, session_id, None)?;
        return Ok(Target::NotLive);
    }

    let Some(tty) = tty_of_pid(row.pid) else {
        return Ok(Target::Unreachable);
    };
    resolve_tty(&tty)
}

/// Resolve a bare tty directly to an injectable target — for a process
/// teleport has no `live_session` binding for at all (no session_id, no pid
/// on record), e.g. a different agent CLI reached via `wake::type_raw`
/// (docs/same-machine-poke-design.md's pi follow-up). Same tmux-then-iTerm2
/// check `resolve()` uses once it has a tty in hand, exposed directly for
/// callers that already know the tty by other means (`ps`, a window title).
pub fn resolve_tty(tty: &str) -> Result<Target> {
    if let Some(pane) = tmux_pane_for_tty(tty)? {
        return Ok(Target::Tmux(pane));
    }
    // Every declared terminal, in id order, first claim wins. Two terminals
    // cannot own one tty, so order decides nothing real — it is fixed only so
    // that a machine with an odd descriptor set behaves the same on every run.
    for cfg in terminal::all() {
        if terminal_owns_tty(&cfg, tty) {
            return Ok(Target::Terminal {
                id: cfg.id,
                tty: tty.to_string(),
            });
        }
    }
    Ok(Target::Unreachable)
}

/// Ask ONE terminal whether it owns `tty`.
///
/// Replaces `iterm_session_exists_for_tty`, whose normalization note now lives
/// in `terminal::needle`: `tty of s` answers the full `/dev/...` form, so a
/// caller passing the bare form (as it would get from `ps -o tty=`) would
/// silently never match without stripping — verified live, and the same
/// normalization `tmux_pane_for_tty` does on both its sides.
fn terminal_owns_tty(cfg: &terminal::TerminalConfig, tty: &str) -> bool {
    let Some(script) = cfg.applescript_probe(tty) else {
        // A command-driven terminal answers by listing, not by scripting;
        // nothing ships one yet, and claiming ownership without asking would
        // be worse than declining.
        return false;
    };
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "ok")
        .unwrap_or(false)
}

/// `kill(pid, 0)` — liveness probe without sending a signal.
fn process_alive(pid: i32) -> bool {
    // Signal 0 sends nothing; it only asks the kernel whether the pid exists
    // and is signalable. No memory is touched, so the unsafe block carries no
    // memory-safety obligation beyond libc's own FFI declaration.
    // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
    unsafe { libc::kill(pid, 0) == 0 }
}

/// `ps -o tty= -p <pid>` → `/dev/ttys003`.
fn tty_of_pid(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "tty=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "??" {
        None
    } else {
        // `ps` may return a bare `ttys003`; normalize to /dev/ttys003.
        Some(if s.starts_with('/') {
            s
        } else {
            format!("/dev/{s}")
        })
    }
}

/// `tmux list-panes -a -F '#{pane_tty} #{pane_id}'` → first pane whose tty matches.
fn tmux_pane_for_tty(tty: &str) -> Result<Option<String>> {
    let out = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_tty} #{pane_id}"])
        .output()
        .ok();
    let Some(out) = out else { return Ok(None) };
    if !out.status.success() {
        return Ok(None); // tmux not running
    }
    let needle = tty.trim_start_matches("/dev/").to_string();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        if let (Some(tt), Some(pane)) = (parts.next(), parts.next()) {
            let tt = tt.trim_start_matches("/dev/").to_string();
            if tt == needle {
                return Ok(Some(pane.to_string()));
            }
        }
    }
    Ok(None)
}

/// Whether an iTerm2 session with this tty currently exists — checked at
/// resolve time (not cached) same as tmux, so a closed window degrades to
/// `Unreachable` rather than a wake attempt into nothing. A `false` result
/// covers both "iTerm2 isn't running" and "no session has that tty," which is
/// the correct degrade for both.
///
/// A process's start time, verbatim from `ps -o lstart=`.
///
/// Deliberately NOT parsed. Identity here needs only equality — the same
/// incarnation always prints the same string, and a reused pid necessarily
/// started later and prints a different one — so parsing would add a
/// locale-dependent failure mode in exchange for nothing. `LC_ALL=C` pins the
/// format anyway, because the string is compared against one recorded earlier,
/// possibly under a different environment.
///
/// `None` when the process is gone or `ps` could not be run. Callers treat that
/// as "unknown", not as "different": an unknown start time falls back to the
/// time window rather than refusing to recognise a conversation.
pub fn process_start(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Turn any address into the session id that should actually receive mail.
///
/// Two forms are accepted and only one of them is stable:
///
/// * a CONVERSATION address (`…/conv-<uuid>`) resolves to whichever segment that
///   conversation currently answers on — this is what survives a compaction, and
///   what `tp live` publishes;
/// * a SESSION id passes through untouched, so every address ever printed,
///   copied into a note, or stored in an old message's return field keeps
///   working exactly as before.
///
/// Anything else is returned unchanged too. Resolution is not the place to
/// judge an address — `validate_address` and `addressability` already do that,
/// and silently rewriting an unknown id here would hide which of the two the
/// caller actually supplied.
pub fn address_to_session(conn: &Connection, address: &str) -> Result<String> {
    if is_conversation_address(address) {
        return Ok(reach::conversation_current_session(conn, address)?
            .unwrap_or_else(|| address.to_string()));
    }
    // A SEGMENT id is followed forward to whatever its conversation answers on
    // now. Addressing one is not a request to write into a particular
    // transcript file — it is a request to reach the correspondent that segment
    // belonged to, and that correspondent has almost certainly compacted since
    // the address was copied down.
    //
    // Both failure directions reported from the field are this one function:
    // a reply to a message stamped with a segment id parked because the segment
    // was no longer registered, and a reply to one stamped with a conversation
    // address parked because only `tp ask` resolved it. An agent following the
    // `/tp` skill literally — "ANSWER IT. Use tp reply" — parked its answer in
    // the common case.
    //
    // Reading an old segment's mailbox directly is still possible and still
    // means what it says: `tp inbox --session-id <segment>`.
    if let Some(conv) = reach::conversation_of(conn, address)? {
        if let Some(current) = reach::conversation_current_session(conn, &conv)? {
            return Ok(current);
        }
    }
    Ok(address.to_string())
}

/// Whether an address names a conversation rather than a transcript segment.
///
/// The `conv-` prefix on the last segment is the marker. It is minted by
/// teleport, never by a runtime, so it cannot collide with a native session id:
/// a runtime that happened to produce an id starting `conv-` would still be
/// namespaced under its own `runtime_id`, and the lookup that follows simply
/// finds nothing and falls through.
pub fn is_conversation_address(address: &str) -> bool {
    address
        .rsplit('/')
        .next()
        .is_some_and(|last| last.starts_with("conv-"))
}

/// The stable address to publish for a session, if it has one.
pub fn conversation_address(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    reach::conversation_of(conn, session_id)
}

/// Every conversation this session's PANE owns, most-recently-seen first.
///
/// Re-exported rather than reached for through `tp_db` so the reach crate stays
/// the one surface the CLI talks to for session identity. Callers want the head
/// (an address to publish) or the whole list (mailboxes to drain); see
/// `tp_db::reach::conversations_of_pane` for why a pane can own more than one.
pub fn conversations_of_pane(conn: &Connection, session_id: &str) -> Result<Vec<String>> {
    reach::conversations_of_pane(conn, session_id)
}

/// Resolve a session's source path (for reading turns) — used by `/tp inbox`.
pub fn source_path(conn: &Connection, session_id: &str) -> Result<Option<PathBuf>> {
    Ok(tp_db::query::source_path(conn, session_id)?.map(PathBuf::from))
}

/// Walk up the parent chain from `from_pid`, returning the first ancestor that
/// has a controlling tty and whose image name contains `needle` (falling back
/// to the first ancestor with any tty). Used by the SessionStart hook: `tp`
/// runs as a child of the session, so one of its ancestors IS the session
/// process and shares its terminal.
pub fn find_session_process(from_pid: i32, needle: &str) -> Option<(i32, String)> {
    let mut pid = from_pid;
    let mut matched_without_tty: Option<i32> = None;
    let mut fallback: Option<(i32, String)> = None;
    for _ in 0..16 {
        let (ppid, comm, tty) = ps_info(pid)?;
        let needle_match = comm.to_lowercase().contains(&needle.to_lowercase());
        if needle_match {
            if let Some(tty) = &tty {
                return Some((pid, tty.clone()));
            }
            // Matched the session process but it has no tty (e.g. headless
            // test runner) — remember it, keep walking for a tty-bearing one.
            if matched_without_tty.is_none() {
                matched_without_tty = Some(pid);
            }
        }
        if let Some(tty) = &tty {
            if fallback.is_none() {
                fallback = Some((pid, tty.clone()));
            }
        }
        if ppid <= 0 || ppid == pid {
            break;
        }
        pid = ppid;
    }
    // Prefer a tty-bearing ancestor; fall back to the matched process even
    // without a tty (the caller can still record the binding).
    matched_without_tty
        .map(|p| (p, "/dev/none".to_string()))
        .or(fallback)
}

/// Which registered live session is this process running inside?
///
/// Walks up the parent chain and asks the REGISTRY — not the environment, not
/// a process name — whether each ancestor is a registered agent session. The
/// pid is the one identity that cannot fork:
///
/// * `$CLAUDE_CODE_SESSION_ID` can disagree with what the SessionStart hook
///   registered. Observed live after a `--resume`: the hook registered
///   `…/claude_code/0403241e-…` (which is what `tp live` publishes, and
///   therefore what every other session addresses) while the MCP server's env
///   carried `…/eb22fe48-…` for the very same pid. Anything trusting the env
///   read an empty mailbox while real messages sat in the other one, and
///   stamped a return address nobody could ever wake.
/// * A runtime with no hook-set env var at all (pi, invoking `tp` straight
///   from bash) has nothing to read in the first place, so every message it
///   sent was anonymous and therefore unanswerable.
///
/// Both failures are silent — the wrong-but-plausible id is accepted and the
/// message simply never arrives. Resolving through the registry makes the
/// answer agree with what `tp live` advertises by construction, for every
/// runtime and every invocation path.
/// The answer to "which session am I", including WHY when there isn't one.
///
/// `Option` collapsed two outcomes a caller acts on differently, and the cost
/// showed up as a misleading error: a codex session was told "is tpd running and
/// this session registered?" when tpd WAS running and it WAS registered — two
/// segments simply shared its pid and teleport refused to guess between them.
/// The advice was the opposite of the truth.
///
/// This is the same shape as `Target::NotLive` merging three states: a type that
/// merges outcomes a caller would branch on is a decision made on the caller's
/// behalf without telling it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnSession {
    Resolved(String),
    /// Several sessions are registered on this process and none is
    /// unambiguously "mine". Carries them, because the caller can name one —
    /// which is a far better answer than "not found".
    Ambiguous(Vec<String>),
    /// No ancestor process has any registration.
    Unknown,
}

impl OwnSession {
    pub fn resolved(self) -> Option<String> {
        match self {
            OwnSession::Resolved(s) => Some(s),
            _ => None,
        }
    }
}

pub fn session_of_process(conn: &Connection, from_pid: i32) -> Result<Option<String>> {
    Ok(own_session(conn, from_pid)?.resolved())
}

pub fn own_session(conn: &Connection, from_pid: i32) -> Result<OwnSession> {
    let mut pid = from_pid;
    // Same bound as `find_session_process`: a session process is a handful of
    // levels up at most, and this stops a pid cycle from spinning.
    for _ in 0..16 {
        // One pid may own MANY rows now that a multiplexed runtime can register
        // several `declared` sessions against its host process (step 1 of
        // docs/reach-provider.md deliberately stopped deduping those). A bare
        // `LIMIT 1` therefore picks arbitrarily, and picking wrong here is not
        // cosmetic: this answers "which session am I", so a wrong answer stamps
        // the wrong sender on every `tp ask` and routes the reply to a third
        // party. Observed live — a Claude Code session's asks came back
        // addressed from the dsh session sharing its pid.
        //
        // A `scan` row is the process's OWN session: the scan identifies a
        // session BY that process. A `declared` row only means "this runtime
        // told us its host pid", which several sessions can share. So scan wins.
        let rows = reach::rows_for_pid(conn, pid)?;
        let scan_owned: Vec<&(String, String)> = rows.iter().filter(|(_, p)| p == "scan").collect();
        if let [(sid, _)] = scan_owned.as_slice() {
            return Ok(OwnSession::Resolved(sid.clone()));
        }
        if scan_owned.is_empty() {
            // No scan-owned session on this pid. A single declared row is still
            // an unambiguous answer; several are not, and guessing between them
            // is exactly the bug above. Report nothing rather than a coin flip —
            // callers already handle `None` by asking for `--from-session`.
            if let [(sid, _)] = rows.as_slice() {
                return Ok(OwnSession::Resolved(sid.clone()));
            }
            if rows.len() > 1 {
                return Ok(OwnSession::Ambiguous(
                    rows.iter().map(|(sid, _)| sid.clone()).collect(),
                ));
            }
        } else if scan_owned.len() > 1 {
            // Two scan rows on one pid should be impossible — reconcile dedupes
            // them — but it happens transiently while a runtime rotates its
            // session id, and ambiguity is still not a guess.
            return Ok(OwnSession::Ambiguous(
                scan_owned.iter().map(|(sid, _)| sid.clone()).collect(),
            ));
        }
        let Some((ppid, _, _)) = ps_info(pid) else {
            break;
        };
        if ppid <= 0 || ppid == pid {
            break;
        }
        pid = ppid;
    }
    Ok(OwnSession::Unknown)
}
fn ps_info(pid: i32) -> Option<(i32, String, Option<String>)> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=,comm=,tty=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut parts = line.split_whitespace();
    let ppid = parts.next()?.parse().ok()?;
    let comm = parts.next().unwrap_or("").to_string();
    let tty = parts
        .next()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty() && s != "??");
    Some((ppid, comm, tty))
}

use std::path::PathBuf;

#[cfg(test)]
mod own_session_tests {
    use super::*;

    fn db() -> tp_db::Db {
        let d = tp_db::Db::open(std::path::Path::new(":memory:")).unwrap();
        d.ensure_self_machine("m1", "h").unwrap();
        d
    }

    /// The regression, observed live: a dsh host launched from a Claude Code
    /// session registered its sessions under the CLAUDE process's pid, so one
    /// pid owned both a scan row (the Claude Code session that process really
    /// is) and declared rows (dsh sessions merely hosted under it). `LIMIT 1`
    /// picked arbitrarily; when it picked dsh, every `tp ask` from Claude Code
    /// was stamped as coming FROM dsh, and dsh's reply — correctly addressed to
    /// the original sender — went back to dsh itself.
    /// Ambiguity must be REPORTABLE, not just refused.
    ///
    /// A codex session hit this live: two of its segments shared a pid while it
    /// rotated its session id, `session_of_process` correctly declined to guess,
    /// and the tool told it "is tpd running and this session registered?" — with
    /// tpd running and the session registered. The refusal was right; collapsing
    /// it into the same `None` as "never registered" made the advice the
    /// opposite of the truth, and the caller could have picked from the
    /// candidates if it had been given them.
    #[test]
    fn ambiguity_names_the_candidates_instead_of_looking_unregistered() {
        let d = db();
        for sid in ["m1/codex/one", "m1/codex/two"] {
            d.conn()
                .execute(
                    "INSERT INTO live_session(session_id, pid, source, registered_at, last_seen_at, presence)
                     VALUES (?1, 4242, 'hook', 0, 0, 'scan')",
                    [sid],
                )
                .unwrap();
        }
        match own_session(d.conn(), 4242).unwrap() {
            OwnSession::Ambiguous(mut c) => {
                c.sort();
                assert_eq!(c, vec!["m1/codex/one", "m1/codex/two"]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        // And a pid with nothing registered stays distinguishable from it.
        assert_eq!(own_session(d.conn(), 9999).unwrap(), OwnSession::Unknown);
    }

    #[test]
    fn the_scan_owned_session_wins_over_declared_ones_sharing_its_pid() {
        let d = db();
        // Declared rows FIRST, which is the real ordering: the host was
        // launched from the Claude Code session, so its sessions register
        // while that one is already live. Under the old `LIMIT 1` this is
        // what made the wrong row win — with the scan row inserted first it
        // happened to be right, which is worse than being reliably wrong.
        for sid in ["m1/dsh/hosted-a", "m1/dsh/hosted-b"] {
            register_with(d.conn(), sid, 11822, None, None, Presence::Declared, None).unwrap();
        }
        register(
            d.conn(),
            "m1/claude_code/mine",
            11822,
            Some("/dev/ttys7"),
            None,
        )
        .unwrap();

        assert_eq!(
            session_of_process(d.conn(), 11822).unwrap().as_deref(),
            Some("m1/claude_code/mine"),
            "the process's OWN session is the scan-identified one; declared rows              only claim it as a host"
        );
    }

    /// With no scan row to disambiguate, several declared sessions on one pid
    /// is a genuine ambiguity. Returning any of them would stamp the wrong
    /// sender — the caller handles `None` by asking for `--from-session`.
    #[test]
    fn several_declared_sessions_on_one_pid_are_ambiguous_not_guessed() {
        let d = db();
        for sid in ["m1/dsh/a", "m1/dsh/b"] {
            register_with(d.conn(), sid, 7000, None, None, Presence::Declared, None).unwrap();
        }
        assert_eq!(session_of_process(d.conn(), 7000).unwrap(), None);
    }

    /// One declared session alone is unambiguous and must still resolve —
    /// otherwise a runtime that correctly declares its own pid could never
    /// identify itself.
    #[test]
    fn a_lone_declared_session_still_resolves() {
        let d = db();
        register_with(
            d.conn(),
            "m1/dsh/only",
            7001,
            None,
            None,
            Presence::Declared,
            None,
        )
        .unwrap();
        assert_eq!(
            session_of_process(d.conn(), 7001).unwrap().as_deref(),
            Some("m1/dsh/only")
        );
    }
}

#[cfg(test)]
mod channel_tests {
    use super::DeliveryChannel;

    #[test]
    fn exec_channels_split_on_whitespace() {
        assert_eq!(
            DeliveryChannel::parse("exec:/usr/local/bin/dsh-tp --wake").unwrap(),
            DeliveryChannel::Exec(vec!["/usr/local/bin/dsh-tp".into(), "--wake".into()])
        );
    }

    /// The security constraint, enforced in code rather than trusted to the
    /// descriptor: a channel is a thing that makes an agent act, so a routable
    /// address would let anything on the network poke a session. There is no
    /// legitimate remote channel — cross-machine reach dispatches through the
    /// peer's own daemon — so refusing is lossless.
    #[test]
    fn http_channels_must_be_loopback() {
        for ok in [
            "http://127.0.0.1:8125/teleport/wake",
            "http://localhost:8125/wake",
        ] {
            assert!(
                DeliveryChannel::parse(ok).is_ok(),
                "{ok} should be accepted"
            );
        }
        for bad in [
            "http://10.0.0.42:8125/wake",
            "http://evil.example.com/wake",
            "https://0.0.0.0:8125/wake",
        ] {
            let err = DeliveryChannel::parse(bad).unwrap_err().to_string();
            assert!(
                err.contains("loopback"),
                "{bad} must be refused as non-loopback, got: {err}"
            );
        }
    }

    #[test]
    fn garbage_is_refused_rather_than_guessed() {
        for bad in ["", "exec:", "ftp://127.0.0.1/x", "just-a-string"] {
            assert!(
                DeliveryChannel::parse(bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }
}

#[cfg(test)]
mod presence_tests {
    use super::*;
    use rusqlite::{params, OptionalExtension};

    fn db() -> tp_db::Db {
        let d = tp_db::Db::open(std::path::Path::new(":memory:")).unwrap();
        d.ensure_self_machine("m1", "h").unwrap();
        d
    }

    /// Backdate a row so the sweep sees it as old, without sleeping.
    fn age(conn: &Connection, sid: &str, by_ms: i64) {
        conn.execute(
            "UPDATE live_session SET last_seen_at = last_seen_at - ?2,
                                     registered_at = registered_at - ?2
              WHERE session_id = ?1",
            params![sid, by_ms],
        )
        .unwrap();
    }

    fn row(conn: &Connection, sid: &str) -> Option<Option<i64>> {
        conn.query_row(
            "SELECT stale_at FROM live_session WHERE session_id = ?1",
            [sid],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn expiry_marks_before_it_evicts() {
        let d = db();
        register_with(
            d.conn(),
            "m1/dsh/s",
            1,
            None,
            None,
            Presence::Declared,
            None,
        )
        .unwrap();

        // Fresh: untouched.
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));
        assert_eq!(
            row(d.conn(), "m1/dsh/s"),
            Some(None),
            "healthy row not marked"
        );

        // Past the TTL: marked, NOT deleted — a stale session is still
        // addressable, so a message still lands in its mailbox.
        age(d.conn(), "m1/dsh/s", PRESENCE_TTL_MS + 1_000);
        assert_eq!(sweep_declared(d.conn()).unwrap(), (1, 0));
        assert!(
            row(d.conn(), "m1/dsh/s").unwrap().is_some(),
            "should be marked stale"
        );

        // Marking is idempotent — a second sweep must not re-mark.
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));

        // Only after the much longer eviction window is the row removed.
        d.conn()
            .execute(
                "UPDATE live_session SET stale_at = stale_at - ?1",
                params![PRESENCE_EVICT_AFTER_MS + 1_000],
            )
            .unwrap();
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 1));
        assert_eq!(row(d.conn(), "m1/dsh/s"), None, "evicted");
    }

    /// The grace window: a session registered moments ago has not had time to
    /// send its first beat. Camel's `setInitialState` exists for this exact
    /// false-negative — a slow-starting plugin must not sweep its own row.
    #[test]
    fn a_just_registered_session_is_never_marked() {
        let d = db();
        register_with(
            d.conn(),
            "m1/dsh/new",
            1,
            None,
            None,
            Presence::Declared,
            None,
        )
        .unwrap();
        // Old heartbeat, but registered just now.
        d.conn()
            .execute(
                "UPDATE live_session SET last_seen_at = last_seen_at - ?1",
                params![PRESENCE_TTL_MS * 5],
            )
            .unwrap();
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));
        assert_eq!(row(d.conn(), "m1/dsh/new"), Some(None));
    }

    #[test]
    fn a_heartbeat_clears_the_stale_mark() {
        let d = db();
        register_with(
            d.conn(),
            "m1/dsh/s",
            1,
            None,
            None,
            Presence::Declared,
            None,
        )
        .unwrap();
        age(d.conn(), "m1/dsh/s", PRESENCE_TTL_MS + 1_000);
        sweep_declared(d.conn()).unwrap();
        assert!(row(d.conn(), "m1/dsh/s").unwrap().is_some());

        assert!(heartbeat(d.conn(), "m1/dsh/s").unwrap(), "renewed");
        assert_eq!(row(d.conn(), "m1/dsh/s"), Some(None), "recovered");
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));
    }

    /// The sweep must never touch a scan-governed row — that one is the
    /// process scan's to prune, and expiring it here would delete a live
    /// Claude Code session that simply cannot heartbeat.
    #[test]
    fn scan_sessions_are_not_swept() {
        let d = db();
        register(d.conn(), "m1/claude_code/s", 1, None, None).unwrap();
        age(d.conn(), "m1/claude_code/s", PRESENCE_EVICT_AFTER_MS * 10);
        assert_eq!(sweep_declared(d.conn()).unwrap(), (0, 0));
        assert!(
            row(d.conn(), "m1/claude_code/s").is_some(),
            "must still exist"
        );
    }

    #[test]
    fn heartbeating_an_unknown_session_reports_it() {
        let d = db();
        assert!(!heartbeat(d.conn(), "m1/dsh/never-registered").unwrap());
    }
}
