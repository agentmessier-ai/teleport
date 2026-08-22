#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Exempt from the workspace print lints: this module's OUTPUT IS THE
//! PRODUCT. `tp` is a CLI; a subcommand that stopped writing to stdout would
//! have no function. Narrow and permanent — see [workspace.lints.clippy] in
//! the root Cargo.toml for why the lint exists everywhere else.

//! Reach commands: the session registry (register/heartbeat/unregister, hook
//! plumbing) and messaging (ask/reply/inbox/ack) — everything that talks TO a
//! session rather than about one.

use crate::{db_path, fmt_ts, hostname, machine_id, parse_duration};
use anyhow::Context as _;
use anyhow::Result;
use tp_db::Db;

/// This machine's composite id for a Claude Code session (LLD §4:
/// `<machine_id>/<runtime_id>/<native_id>`), matching exactly what search/
/// ingest already produce for the same session. `tp register`/`unregister`/
/// `inbox` must all compose the SAME id from a bare native id, or a session
/// id copied out of a `teleport_search` result silently never resolves to a
/// live binding (docs/same-machine-poke-design.md §1b).
///
/// `runtime` is NOT hardcoded to "claude_code": a non-Claude-Code agent with
/// its own registration story (e.g. the pi extension, docs/pi-integration.md)
/// composes under its OWN runtime id — mislabeling a pi session as
/// `claude_code` would be wrong even though nothing enforces it today (no pi
/// ingest adapter exists to collide with), and would make `tp live` lie
/// about what's actually running.
pub(crate) fn live_session_id(native_id: &str, runtime: &str) -> Result<String> {
    Ok(tp_core::SessionId::new(machine_id()?, runtime, native_id).to_string())
}

/// Accept EITHER a bare native id (compose it under `runtime`) or an already
/// composite `<machine>/<runtime>/<native>` id (use as-is, e.g. one copied
/// straight out of a `teleport_search` result). Composite ids always contain
/// two `/` separators; native ids (UUIDs, whatever form a given runtime
/// uses) don't — `SessionId::parse` succeeding is exactly that check.
pub(crate) fn resolve_session_id(raw: &str, runtime: &str) -> Result<String> {
    if tp_core::SessionId::parse(raw).is_some() {
        Ok(raw.to_string())
    } else {
        live_session_id(raw, runtime)
    }
}

/// The process-image name to search for when walking up from `tp`'s own pid
/// to find the session process that spawned it (`find_session_process`).
/// Each runtime's own binary/comm name; falls back to the runtime id itself
/// for anything not special-cased, which is right for extensions that match
/// their own runtime id 1:1 with their process name (true for `pi`).
pub(crate) fn ancestor_needle(runtime: &str) -> String {
    // Same table `recognize_runtime` reads, from the same descriptors — this is
    // the other direction of one question ("what does this runtime's process
    // look like"), and it used to be a second hardcoded map that could disagree
    // with the first. A `=`-anchored pattern is an exact `comm`; the walk below
    // substring-matches either way, so the anchor is stripped here.
    tp_ingest::adapter::process_signature_for(runtime)
        .map(|p| p.trim_start_matches('=').to_string())
        // A harness with no signature is not discoverable by process name at
        // all; falling back to the runtime id preserves the previous behaviour
        // for anything unlisted.
        .unwrap_or_else(|| runtime.to_string())
}

/// Parse a hook event's JSON payload off stdin. Claude Code hook commands
/// receive event data ONLY this way — never as env vars (verified against
/// the current hooks reference; see docs/same-machine-poke-design.md §1a).
pub(crate) struct HookEvent {
    session_id: String,
    cwd: Option<String>,
}

pub(crate) fn read_hook_event() -> Result<HookEvent> {
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
        .context("reading hook JSON from stdin")?;
    let v: serde_json::Value =
        serde_json::from_str(&buf).context("hook stdin was not valid JSON")?;
    let session_id = v
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("hook JSON has no `session_id` field"))?
        .to_string();
    let cwd = v
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Ok(HookEvent { session_id, cwd })
}

/// This session's own composite id — the RETURN ADDRESS stamped onto every
/// message it sends.
///
/// Without it a recipient has no way to answer: it sees a body and a machine
/// name, and if it wants to reply it must guess an address. That is not
/// hypothetical — before this existed, a real reply was sent to a bare machine
/// id (`SWSQ-…` with no `/runtime/native` suffix), matched no session, was
/// never woken, and sat unread in the mailbox while the asker polled the
/// target's transcript on a `sleep` loop waiting for an answer that could not
/// arrive.
///
/// `--from-session` is how a runtime extension that knows its own id (pi)
/// supplies it; the env var is Claude Code's Bash-tool context, which is where
/// `tp ask` runs when an agent calls it. `None` is still tolerated — a human
/// running `tp ask` by hand has no session — but it costs the recipient the
/// ability to reply, so `run_ask` says so out loud.
pub(crate) fn sender_session_id(from_session: Option<&str>, runtime: &str) -> Option<String> {
    if let Some(raw) = from_session {
        return resolve_session_id(raw, runtime).ok();
    }
    own_session_id()
}

/// The return address to stamp on an outgoing message.
///
/// Prefers this sender's CONVERSATION address, because a return address outlives
/// the exchange it belongs to: the reply is written minutes or hours later, by
/// which time the sender may have compacted and its segment id may belong to
/// nobody. That is not hypothetical — both directions of one exchange had to be
/// resent by hand on the day this was written, each because the OTHER side's
/// stamped address had expired in the gap.
///
/// Falls back to the segment id when there is no conversation yet (a session
/// registered before this existed, a runtime that never registers), so the
/// stamp is never worse than it was.
pub(crate) fn sender_address(db: &Db, from_session: Option<&str>, runtime: &str) -> Option<String> {
    let sid = sender_session_id(from_session, runtime)?;
    // The pane's MOST RECENTLY SEEN conversation, not the one this session
    // happens to belong to. A pane can own several (`conversations_of_pane`),
    // and the one a given session joined may be the twin nobody is registered
    // under — in which case the recipient does everything right, addresses the
    // reply exactly where it came from, and the message is STORED for a mailbox
    // the live window does not read.
    //
    // Observed 2026-08-21: two conversation ids alternating as the return
    // address of one window across consecutive turns.
    //
    // `conversations_of_pane` orders by `last_seen_at DESC`, so the head is the
    // one currently being talked on — an address that is live by construction
    // rather than by luck.
    match tp_reach::conversations_of_pane(db.conn(), &sid) {
        Ok(convs) if !convs.is_empty() => Some(convs[0].clone()),
        _ => Some(sid),
    }
}

/// Which session this process belongs to, WITH the reason when there is none.
///
/// Separate from `own_session_id` because the two callers want different
/// things: the CLI only needs the happy path, while a tool result has to tell a
/// model whether to pick from candidates or to go check its daemon.
pub(crate) fn own_session() -> Result<tp_reach::OwnSession> {
    let db = Db::open(&db_path())?;
    tp_reach::own_session(db.conn(), std::process::id() as i32)
}

/// This process's own session, in the order the sources deserve to be trusted.
///
/// 1. **The registry, by pid.** `tp` runs as a descendant of the agent process,
///    so walking up to a registered pid answers "which session am I in" with
///    the same id `tp live` publishes — which is the id other sessions
///    actually address. Works for every runtime, including ones with no
///    session env var at all (pi shelling out to `tp`).
/// 2. `$CLAUDE_CODE_SESSION_ID`, only as a fallback. It is Claude Code-specific
///    and *can disagree with the registry* — after a `--resume` the env id and
///    the hook-registered id were observed to differ for the same pid, which
///    silently pointed reads and replies at a mailbox nobody writes to. Still
///    worth keeping: it's the only source when `tpd` isn't running, so nothing
///    is registered to look up.
pub(crate) fn own_session_id() -> Option<String> {
    if let Ok(db) = Db::open(&db_path()) {
        if let Ok(Some(sid)) = tp_reach::session_of_process(db.conn(), std::process::id() as i32) {
            return Some(sid);
        }
    }
    let native = std::env::var("CLAUDE_CODE_SESSION_ID").ok()?;
    live_session_id(&native, "claude_code").ok()
}

pub(crate) fn run_ask(
    session_id: &str,
    message: &str,
    no_wake: bool,
    from_session: Option<&str>,
    runtime: &str,
    kind: tp_app::Kind,
) -> Result<()> {
    let machine_id = machine_id()?;
    let db = Db::open(&db_path())?;
    db.ensure_self_machine(&machine_id, &hostname())?;

    let from = sender_address(&db, from_session, runtime);
    let sent = tp_app::send(&db, &machine_id, session_id, message, kind, from)?;

    // A note carries a return address too — the receiver may still have
    // something to say — but nothing about it asks for one.
    if sent.from.is_none() && kind == tp_app::Kind::Ask {
        // Never silent: a one-way message looks identical to a two-way one at
        // the call site, and the difference only shows up as an answer that
        // never comes.
        eprintln!("[warn] no return address on this message — the target cannot reply to it. Pass --from-session <your session id> if you expect an answer.");
    }

    // The anti-polling hint is only honest when a reply can actually come back.
    // Two independent ways it cannot: no return address, or nothing on the other
    // end to read the message in the first place. Telling a caller to "end your
    // turn and wait" for an answer that cannot arrive is the same lie as
    // reporting delivery, one line further down.
    // `deliverable` is asked of `Sent` rather than re-derived here. Spelling
    // out `addressability == Registered` at the call site is how the CLI and
    // MCP came to hold two copies of one rule: MCP asks `sent.answerable()`,
    // which is that test AND the return-address test together.
    let deliverable = sent.deliverable();
    let hint = match (kind, sent.from.is_some(), deliverable) {
        // Undeliverable outranks everything: there is no point telling a caller
        // what kind of message it sent to a mailbox nobody drains.
        (_, _, false) => "Do NOT wait for an answer: nothing is currently registered to read this mailbox, so no reply can come back until something registers under this exact id. Find the live address with `tp live` and send again.",
        (tp_app::Kind::Note, _, true) => "This is a NOTE: the target is told it does not need to answer. Do not wait for a reply, and do not send the same thing again as an `ask` to get one.",
        (tp_app::Kind::Ask, true, true) => "This does NOT wait for an answer. The reply arrives later as a `/tp inbox` wake that resumes you — end your turn and say you're waiting. Do not `sleep`-poll for it; that delays nothing but you.",
        (tp_app::Kind::Ask, false, true) => "This does NOT wait, and carries no return address, so no answer can come back. Check the work directly (its output files), not this message.",
        // `tp reply` has its own path and never arrives here. Spelled out
        // rather than caught by a wildcard: a wildcard would silently absorb
        // whatever kind is added next, and the hint is the thing most likely to
        // be wrong for it.
        (tp_app::Kind::Reply, _, _) => unreachable!("replies are sent through run_reply"),
    };
    finish_send(
        &db,
        &sent.target,
        &sent.message_id,
        no_wake,
        "queued",
        Some(hint),
    )
}

/// Answer a message, addressed automatically to whoever sent it.
pub(crate) fn run_reply(
    message_id: &str,
    message: &str,
    no_wake: bool,
    from_session: Option<&str>,
    runtime: &str,
) -> Result<()> {
    let machine_id = machine_id()?;
    let db = Db::open(&db_path())?;
    db.ensure_self_machine(&machine_id, &hostname())?;

    let from = sender_address(&db, from_session, runtime);
    let sent = tp_app::reply(&db, &machine_id, message_id, message, from)?;

    // No anti-polling hint here: the replier is finishing an exchange, not
    // starting one, so there is nothing to wait for.
    let hint = if sent.from.is_none() {
        Some("Your answer carries no return address, so this exchange ends here — the sender cannot come back with a follow-up.")
    } else {
        None
    };
    finish_send(
        &db,
        &sent.target,
        &sent.message_id,
        no_wake,
        "replied",
        hint,
    )
}

/// Wake the target and report what actually happened. Shared by `ask` and
/// `reply` so the two can't drift on delivery semantics or on how a
/// non-delivery is described — "queued but nobody was woken" is a materially
/// different outcome from "delivered", and both commands must say which.
///
/// `hint` is printed under the outcome line. It exists because guidance that
/// lives only in a tool description or a skill body does NOT reach an agent
/// that shells out to `tp` directly — and shelling out is the norm for any
/// runtime without native teleport tools. Observed live: a pi session with the
/// tools installed still ran `tp ask` from bash and then sat in a `sleep 420`
/// loop, because nothing in the command's own output said the send was
/// asynchronous. The converse is also on record: a pi session with NO skill and
/// NO tools replied correctly, purely because `tp inbox` prints the literal
/// `tp reply <id> "…"` command. Put the affordance at the point of use.
/// What to tell the caller about an address that could not be woken.
///
/// Never "delivered on next /tp inbox" unless something is actually expected to
/// run one. For a dormant address the single most likely cause is worth naming:
/// a Claude Code conversation is issued a NEW session id at every compaction, so
/// an address that worked an hour ago can belong to no one now while the same
/// conversation runs on under a different id.
pub(crate) fn undeliverable_note(db: &Db, target: &str) -> Result<String> {
    Ok(match tp_db::reach::addressability(db.conn(), target)? {
        tp_db::reach::Addressability::Registered => {
            "session not injectable right now — delivered on its next /tp inbox".to_string()
        }
        // "PARKED, NOT DELIVERED" read as "the send failed". It did not: the
        // message IS stored. A session that read it as a rejection resent
        // twice and delivered three copies of the same report — so every line
        // below now says STORED first and what is missing second.
        tp_db::reach::Addressability::DormantConversation => {
            "STORED — this conversation has no registered session right now, so nobody was woken. \
             It is collected as soon as any segment of that conversation registers again; do not \
             resend"
                .to_string()
        }
        tp_db::reach::Addressability::Dormant => {
            "STORED but nothing will drain it — this session is indexed and no longer registered. \
             A Claude Code session id changes at every compaction, so the same conversation is \
             probably live under a different address. Do not resend to this one: find the current \
             address with `tp live`"
                .to_string()
        }
        tp_db::reach::Addressability::Unknown => {
            "STORED but nothing will drain it — teleport has never seen this session id, and it is \
             delivered only if a session registers under exactly this id. Do not resend to this \
             one: check `tp live`"
                .to_string()
        }
    })
}

/// What to type into a target's pane, by its runtime.
///
/// `/tp inbox` is a slash command Claude Code and pi both provide, which made it
/// look like teleport's vocabulary when it is theirs. A codex session receiving
/// it answers `Unrecognized command '/tp'` — the wake lands and the target
/// cannot act on it. Resolved HERE rather than inside `tp-reach`, which holds no
/// runtime knowledge by design.
pub(crate) fn control_string_for(target: &str) -> String {
    target
        .split('/')
        .nth(1)
        .and_then(tp_ingest::adapter::control_string_for)
        .unwrap_or_else(|| tp_reach::CONTROL_STRING.to_string())
}

pub(crate) fn finish_send(
    db: &Db,
    target: &str,
    msg_id: &str,
    no_wake: bool,
    verb: &str,
    hint: Option<&str>,
) -> Result<()> {
    let short = &msg_id[..8];
    let outcome = if no_wake {
        "no wake".to_string()
    } else {
        // Always a CLI process (or a subprocess an agent spawns for a tool
        // call) — never `tpd` — so any backend including iTerm2 AppleScript is
        // fair game (see `tp_reach::Caller` doc).
        match tp_reach::attempt_wake(
            db.conn(),
            target,
            &control_string_for(target),
            tp_reach::Caller::Cli,
        )? {
            tp_reach::DeliveryOutcome::Woke(tp_reach::Target::Tmux(pane)) => {
                format!("woke tmux pane {pane}")
            }
            tp_reach::DeliveryOutcome::Woke(tp_reach::Target::Terminal { id, tty }) => {
                format!("woke {id} session {tty}")
            }
            tp_reach::DeliveryOutcome::Woke(tp_reach::Target::Channel(chan)) => {
                format!("woke via the runtime's own channel ({chan:?})")
            }
            // The two NON-injectable targets, spelled out rather than caught
            // by a wildcard, so this match is exhaustive over `Target` and the
            // compiler points HERE when a variant is added.
            //
            // A wildcard is not free just because it is currently unreachable.
            // `attempt_wake` has no wildcard, so adding a variant does fail to
            // compile — in tp-reach. That is a different file, and 550a662
            // (which added `Target::Channel`) touched delivery.rs, resolve.rs,
            // wake.rs and this file, but NOT mcp.rs; the MCP arm arrived a day
            // later in 6636a16, and in between every MCP `teleport_ask` to a
            // declared-presence session panicked. The compile error did not
            // prevent it, because it never pointed at the surface that broke.
            tp_reach::DeliveryOutcome::Woke(
                other @ (tp_reach::Target::Unreachable | tp_reach::Target::NotLive),
            ) => {
                unreachable!("attempt_wake only returns Woke for injectable targets, got {other:?}")
            }
            tp_reach::DeliveryOutcome::Coalesced => format!(
                "already woken in the last {}ms — will be drained with the rest",
                tp_reach::WAKE_COALESCE_MS
            ),
            // Can't happen right after our own enqueue succeeded, but handle it
            // rather than assume.
            tp_reach::DeliveryOutcome::NoMessages => "queued".to_string(),
            tp_reach::DeliveryOutcome::NotInjectable(tp_reach::Target::Unreachable) => {
                // A registered session with no injectable pane — it IS being
                // drained by someone, just not pokeable from here.
                "registered but not injectable — target checks on next /tp inbox".to_string()
            }
            tp_reach::DeliveryOutcome::NotInjectable(_) => undeliverable_note(db, target)?,
        }
    };
    println!("{verb} {short} → {target} ({outcome})");
    if let Some(h) = hint {
        println!("  {h}");
    }
    Ok(())
}

pub(crate) fn run_type(tty: &str, message: &str) -> Result<()> {
    let tty = tty.trim_start_matches("/dev/");
    let target = tp_reach::resolve_tty(tty)?;
    if matches!(target, tp_reach::Target::Unreachable) {
        anyhow::bail!("no tmux pane or iTerm2 session found with tty {tty} — check `ps -o tty=,comm=` for the right one");
    }
    // Always run as a CLI process (this IS `tp`, invoked directly), so any
    // backend including iTerm2 AppleScript is fair game.
    tp_reach::type_raw(&target, message, tp_reach::Caller::Cli)?;
    println!("typed into {target:?}");
    Ok(())
}

/// An explicit --session-id may be bare (a runtime's own extension, e.g. pi,
/// passing its native id) or already composite (copied out of a
/// teleport_search result) — resolve_session_id handles both. The env-var
/// fallback is always the bare native id Claude Code itself sets.
///
/// Registry-first (see `own_session_id`): reading the mailbox the env var
/// points at is exactly how a resumed session ends up reporting "no
/// messages" while its real inbox — the one `tp live` advertises and
/// everyone else writes to — fills up unread. Shared by every command that
/// reads a mailbox, so this resolution rule cannot drift between them.
pub(crate) fn resolve_inbox_session(session_id: Option<String>, runtime: &str) -> Result<String> {
    match session_id {
        Some(sid) => resolve_session_id(&sid, runtime),
        None => own_session_id().ok_or_else(|| {
            anyhow::anyhow!(
                "no session id — pass --session-id, or run from a registered agent session (is tpd running?)"
            )
        }),
    }
}

/// Render one message. Shared by `tp inbox`'s three modes — drain, pending,
/// history — because a message means the same thing in all three; only the
/// footer around the list changes.
pub(crate) fn print_message(m: &tp_reach::Message) {
    // The id and the sender are part of the message, not decoration: a
    // recipient that can't see them can't answer, and will invent an address
    // instead (observed: a reply sent to a bare machine id, never delivered).
    // Print the exact command rather than describing it.
    println!("[{}] from {}", m.kind, m.from_machine);
    println!("  id: {}", &m.id[..8]);
    match &m.from_session {
        Some(from) => {
            println!("  from-session: {from}");
            // A note wakes the reader like anything else, so without this
            // line it is indistinguishable from a request — and the skill
            // says ANSWER IT. Two agents then spend a turn each being
            // polite. Say which one this is at the point of decision.
            // Three kinds, so three arms. This was `if kind == "note"` with
            // an else, and a `[reply]` — the ANSWER to something you asked —
            // fell into the else and was told "reply with: tp reply …". The
            // wake skill enumerates only `[ask]` and `[note]`, so an agent
            // reading an answer with a reply instruction under it answers
            // the answer, and the sender gets woken for nothing.
            match m.kind.as_str() {
                "note" => {
                    println!("  FYI — no reply expected. Answer only if you have something to add.")
                }
                "reply" => println!(
                    "  This ANSWERS something you asked. Nothing to do unless you have a follow-up."
                ),
                _ => println!("  reply with: tp reply {} \"...\"", &m.id[..8]),
            }
        }
        None => {
            println!("  from-session: (none — this message cannot be replied to)");
        }
    }
    println!("  {}", m.body);
}

pub(crate) fn run_inbox(
    session_id: Option<String>,
    runtime: &str,
    pending: bool,
    history: bool,
    since: &str,
) -> Result<()> {
    let sid = resolve_inbox_session(session_id, runtime)?;
    let db = Db::open(&db_path())?;

    // `--pending` and `--history` are READ-ONLY views — neither drains nor
    // marks anything, so checking them never counts as having processed a
    // message. Only the default (no flag) path calls `drain`, which does.
    if pending {
        let msgs = tp_app::pending(&db, &sid)?;
        if msgs.is_empty() {
            println!("nothing pending ack for {sid}");
            return Ok(());
        }
        for m in &msgs {
            print_message(m);
        }
        println!(
            "({} message(s) delivered but not yet acked — `tp ack <id>` once you've handled each)",
            msgs.len()
        );
        return Ok(());
    }
    if history {
        let since_ms = tp_core::now_ms() - parse_duration(since)?.as_millis() as i64;
        let msgs = tp_app::history(&db, &sid, since_ms)?;
        if msgs.is_empty() {
            println!("no acked messages for {sid} since {since}");
            return Ok(());
        }
        for m in &msgs {
            print_message(m);
            if m.acked_at.is_some() {
                println!("  acked: {}", fmt_ts(m.acked_at));
            }
        }
        println!("({} acked message(s) since {since})", msgs.len());
        return Ok(());
    }

    let drained = tp_app::drain(&db, &sid)?;
    let msgs = &drained.messages;
    if msgs.is_empty() {
        println!("inbox empty for {sid}");
        return Ok(());
    }
    for m in msgs {
        print_message(m);
    }
    println!(
        "({} message(s) drained — `tp ack <id>` once you've acted on each, so an interrupted \
         batch stays recoverable via `tp inbox --pending`)",
        msgs.len()
    );
    Ok(())
}

pub(crate) fn run_ack(message_id: &str) -> Result<()> {
    let db = Db::open(&db_path())?;
    let m = tp_app::ack(&db, message_id)?;
    println!(
        "acked {} — [{}] from {}",
        &m.id[..8],
        m.kind,
        m.from_machine
    );
    Ok(())
}

pub(crate) fn run_register(
    session_id: Option<String>,
    cwd: Option<String>,
    from_hook: bool,
    runtime: &str,
    presence: &str,
    deliver: Option<&str>,
    declared_pid: Option<i32>,
) -> Result<()> {
    let presence = tp_app::session::parse_presence(presence)
        .map_err(|e| anyhow::anyhow!("{e} (--presence)"))?;
    let (native_id, cwd) = if from_hook {
        let ev = read_hook_event()?;
        (ev.session_id, ev.cwd)
    } else {
        let sid = session_id
            .ok_or_else(|| anyhow::anyhow!("--session-id is required without --from-hook"))?;
        (sid, cwd)
    };
    let session_id = live_session_id(&native_id, runtime)?;

    let machine_id = machine_id()?;
    let db = Db::open(&db_path())?;
    db.ensure_self_machine(&machine_id, &hostname())?;

    // `tp` here is a child of the session (or a shell wrapping it), so an
    // undeclared host has to be found by walking up. A runtime that states its
    // own pid is taken at its word — the walk cannot tell "the process hosting
    // this session" from "whatever happened to launch it".
    let host = match declared_pid {
        Some(pid) => tp_app::Host::Declared(pid),
        None => tp_app::Host::Inferred {
            from_pid: std::process::id() as i32,
            needle: ancestor_needle(runtime),
        },
    };

    let r = tp_app::session::register(&db, &session_id, host, cwd.as_deref(), presence, deliver)?;
    println!(
        "registered {} → pid {} tty {} [{}{}]",
        r.session_id,
        r.pid,
        r.tty.as_deref().unwrap_or("(none)"),
        if r.presence == tp_reach::resolve::Presence::Declared {
            "declared"
        } else {
            "scan"
        },
        r.deliver
            .map(|d| format!(", deliver {d}"))
            .unwrap_or_default(),
    );
    Ok(())
}

pub(crate) fn run_ingest(
    session_id: &str,
    runtime: &str,
    cwd: Option<&str>,
    title: Option<&str>,
    title_source: &str,
) -> Result<()> {
    use std::io::Read;
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let turns = tp_app::ingest::parse_turns(&raw)?;

    let composite = live_session_id(session_id, runtime)?;
    let mut meta = tp_core::turn::SessionMeta {
        cwd: cwd.map(str::to_string),
        started_at: turns.iter().filter_map(|t| t.ts).min(),
        ..Default::default()
    };
    if let Some(t) = title {
        let source = match title_source {
            "ai" => tp_core::TitleSource::Ai,
            _ => tp_core::TitleSource::User,
        };
        meta.set_title(source, t);
    }

    let mut db = Db::open(&db_path())?;
    db.ensure_self_machine(&machine_id()?, &hostname())?;
    let out = tp_app::ingest::push(&mut db, &machine_id()?, &composite, runtime, &meta, turns)?;

    println!(
        "ingested {} turn(s) into {} ({} duplicate(s) skipped)",
        out.inserted, out.session_id, out.duplicates
    );
    if out.unkeyed > 0 {
        eprintln!(
            "[warn] {} turn(s) had no prov.uuid and cannot be deduplicated —              re-pushing them will duplicate them",
            out.unkeyed
        );
    }
    Ok(())
}

pub(crate) fn run_heartbeat(session_id: &str, runtime: &str) -> Result<()> {
    let session_id = live_session_id(session_id, runtime)?;
    let db = Db::open(&db_path())?;
    // Report non-delivery rather than exiting 0 on a no-op: a runtime beating
    // into a row teleport already evicted needs to know it must re-register,
    // and silence would let it keep beating at nothing forever.
    if tp_app::session::heartbeat(&db, &session_id)? {
        println!("heartbeat {session_id}");
    } else {
        println!("no live registration for {session_id} — re-register to be reachable");
    }
    Ok(())
}

pub(crate) fn run_unregister(
    session_id: Option<String>,
    from_hook: bool,
    runtime: &str,
) -> Result<()> {
    let native_id = if from_hook {
        read_hook_event()?.session_id
    } else {
        session_id.ok_or_else(|| anyhow::anyhow!("--session-id is required without --from-hook"))?
    };
    let session_id = live_session_id(&native_id, runtime)?;

    // Pin the delete to OUR OWN resolved ancestor pid: if this session_id was
    // reused (e.g. `/clear`) and a new SessionStart already reclaimed the
    // row, this SessionEnd must not unregister that newer, still-live
    // binding — see docs/same-machine-poke-design.md's ordering risk. Only
    // meaningful when we actually find a matching ancestor to pin to (the
    // same walk `register` used to store it); without one there's no
    // reliable identity to compare against, so fall back to the old
    // unconditional delete rather than pinning to `tp`'s own (unrelated) pid.
    let self_pid = std::process::id() as i32;
    let expected_pid = tp_reach::resolve::find_session_process(self_pid, &ancestor_needle(runtime))
        .map(|(pid, _)| pid);

    let db = Db::open(&db_path())?;
    tp_app::session::unregister(&db, &session_id, expected_pid)?;
    println!("unregistered {session_id}");
    Ok(())
}

// ── Federation ───────────────────────────────────────────────────────────────
