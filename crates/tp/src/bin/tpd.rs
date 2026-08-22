#![allow(clippy::print_stdout)]
//! Exempt from the workspace `print_stdout` lint, for ONE line: `--version`
//! answers on stdout because a caller pipes it, and a log line is not an
//! answer.
//!
//! `print_stderr` is NOT exempted any more. The previous exemption covered
//! "startup and fatal diagnostics", and its own last paragraph said anything
//! merely diagnostic belonged in the logger — which was most of the file:
//! presence sweeps, discovery cycles, a stopped watcher, a stale descriptor.
//! They now go through `tp_core::logging` and gain the timestamp that a daemon
//! log needs most.
//!
//! The startup banner went with them, for the same reason rather than despite
//! it. "When did tpd restart" is the single most useful line in this file, and
//! it was the one line with no time on it.

//! `tpd` — the resident daemon (LLD §3). Three jobs, one process:
//!   - serve the peer HTTP API (`tp-net::server`)
//!   - keep the optional index fresh via the FSEvents watcher
//!
//! Deliberately TCC-free (LLD §7.4): no AppleScript, no Automation, nothing
//! that a LaunchAgent cannot do. It binds a socket, reads `~/.claude`, runs
//! `ps`/`lsof`, and writes SQLite — all of which work from a bare LaunchAgent
//! with no prompts.
//!
//! This was FALSE for as long as the discovery scan enumerated iTerm2 sessions
//! over AppleScript, every 60s, from inside this daemon — a comment describing
//! intent while the code did the opposite, which is the same failure mode that
//! kept `/v1/pair/respond`'s hole invisible through review (see
//! `tp-net::server`'s auth middleware). Injection genuinely does need
//! Automation, and it genuinely does not belong here: it lives in `tp`, which
//! a human runs from a GUI session that can hold the grant. See
//! `tp_reach::discover::scan_all`.
//!
//! It is a separate binary rather than `tp serve` because it is a different
//! kind of program: `tp` is stateless and exits, `tpd` is a supervised
//! long-lived service whose lifecycle launchd owns.

use anyhow::{Context, Result};
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> Result<()> {
    // Answered before ANYTHING else, and before binding in particular: asking
    // a supervised daemon its version must work while a copy of it is already
    // running, which is exactly when the question gets asked. `tpd --version`
    // used to reach the bind and fail with "address in use".
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("tpd {}", tp_core::VERSION_LINE);
        return Ok(());
    }

    // Before anything that can log. `tp` and `tpd` write to the same stderr
    // convention and, under launchd, the same file — without this both say
    // "teleport" and the field that exists to tell them apart tells you nothing.
    // `tpd --version` is piped by callers exactly as `tp` is.
    tp_core::exit_quietly_on_broken_pipe();
    tp_core::logging::set_service("tpd");
    // The daemon writes its own log so it can rotate it. launchd's stderr file
    // stays as the capture for panics and anything before this line — which is
    // why that one needs no rotation and this one does.
    let log_path = tp_db::teleport_dir().join("tpd.log");
    if let Err(e) = tp_core::logging::to_file(&log_path) {
        // Stderr is still the fallback inside `emit`, so this degrades to the
        // previous behaviour rather than to silence.
        tp_core::log_warn!(
            "cannot write {}: {e:#} — logging to stderr",
            log_path.display()
        );
    }

    let port: u16 = std::env::var("TP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(tp_net::DEFAULT_PORT);

    let identity = tp_net::Identity::load_or_create(&tp_net::identity::default_key_path())
        .context("load device identity")?;
    let name = tp_net::identity::hostname();

    tp_core::log_info!(
        "tpd {} — {} ({})",
        tp_core::VERSION_LINE,
        name,
        identity.device_id
    );

    let db = tp_db::Db::open(&tp_db::default_db_path()).context("open db")?;
    db.ensure_self_machine(&identity.device_id, &name)?;
    // Publish what is actually serving, so the panel can tell a stale daemon
    // from a current one — installing binaries does not restart a LaunchAgent.
    db.record_daemon_start(tp_core::VERSION_LINE, std::process::id())?;

    // Serve the SCAN provider: it needs no index, so a fresh install answers
    // peer queries correctly before `tp index` has ever run (LLD §16).
    let retrieval = tp_search::Retrieval::new(Box::new(tp_search::ScanProvider::new(
        identity.device_id.clone(),
        tp_ingest::adapter::all_adapters(),
        tp_ingest::adapter::all_roots(),
    )));

    let state = tp_net::default_state(db, identity.clone(), retrieval);

    // 0.0.0.0: peers are by definition not on loopback. Every non-loopback
    // request still has to carry a valid signature from a trusted peer.
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;
    let bound = tp_net::serve(state, addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tp_core::log_info!("serving on {bound}");
    // A stale copy here is the one configuration fault that makes a freshly
    // installed daemon run old parsing rules while reporting a new version.
    for o in tp_ingest::adapter::descriptor_overrides() {
        if !o.identical {
            tp_core::log_warn!(
                "descriptor override {} differs from the embedded {} — customized or stale; `tp version` explains",
                o.path.display(),
                o.id
            );
        }
    }

    // The watcher owns its own DB handle: it writes, the server reads, and
    // WAL lets those proceed concurrently. It runs on a plain thread because
    // its work is blocking filesystem + SQLite, which would stall a tokio
    // worker for the whole life of the process.
    let watch_db = tp_db::Db::open(&tp_db::default_db_path()).context("open db for watcher")?;
    let machine_id = identity.device_id.clone();
    std::thread::spawn(move || {
        if let Err(e) = run_watcher(watch_db, machine_id) {
            tp_core::log_error!("watcher stopped: {e:#}");
        }
    });

    // Same reasoning: its own DB handle, its own blocking thread (tmux/ps/
    // lsof/osascript subprocess calls, not async-friendly work).
    let scan_db =
        tp_db::Db::open(&tp_db::default_db_path()).context("open db for discovery scan")?;
    let machine_id = identity.device_id.clone();
    std::thread::spawn(move || run_discovery_scan(scan_db, machine_id));

    // Serve until killed. launchd restarts us if we exit.
    tokio::signal::ctrl_c().await.ok();
    tp_core::log_info!("shutting down");
    Ok(())
}

fn run_watcher(db: tp_db::Db, machine_id: String) -> Result<()> {
    let roots = tp_watch::all_roots();
    if roots.is_empty() {
        tp_core::log_warn!("no known agent transcript roots on this machine — watcher idle");
        return Ok(());
    }
    for wr in &roots {
        db.ensure_runtime(&wr.runtime_id, wr.root.to_str().unwrap_or_default())?;
        tp_core::log_info!("watching {} ({})", wr.root.display(), wr.runtime_id);
    }
    // `run` loops forever: debounce, drain FSEvents, reconcile every 30s.
    tp_watch::Watcher::new(machine_id, db, roots)?.run()
}

/// Active-scan discovery (docs/same-machine-poke-design.md follow-up):
/// authoritative for `live_session` liveness every `SCAN_INTERVAL_SECS` —
/// adds what hook registration missed, prunes what it never got to clean up.
/// One bad cycle (e.g. a transient `lsof`/`osascript` hiccup) must not kill
/// the loop — `reconcile` returning `Err` is logged and the loop continues,
/// same "keep going, don't take the daemon down" posture `run_watcher` takes
/// for its own errors.
fn run_discovery_scan(db: tp_db::Db, machine_id: String) {
    // Read once at startup, not per cycle: descriptors change when a user edits
    // ~/.teleport/runtimes.d and restarts, which is the same lifecycle as the
    // transcript roots this daemon already resolves once in `run_watcher`.
    let signatures: Vec<tp_reach::discover::ProcessSignature> =
        tp_ingest::adapter::process_signatures()
            .into_iter()
            .map(
                |(runtime_id, pattern)| tp_reach::discover::ProcessSignature {
                    runtime_id,
                    pattern,
                },
            )
            .collect();
    if signatures.is_empty() {
        tp_core::log_warn!("no harness declares a process signature — scan discovery is inert");
    }

    // Consecutive cycles that found nothing. Reset on any non-empty scan.
    let mut empty_cycles = 0u32;

    loop {
        // Scan first, then sleep — so a fresh `tpd` restart doesn't leave
        // `live_session` stale/empty for up to a full interval before the
        // first reconcile.
        // Declared sessions expire on their own heartbeat, not on the scan —
        // the two halves of one job, so they share a cycle. Failure here must
        // not skip the scan, and vice versa: rocketmq's registry wraps its
        // whole expiry scan for the same reason, so one bad entry cannot end
        // the cycle for every other member.
        match tp_reach::sweep_declared(db.conn()) {
            Ok((marked, evicted)) if marked > 0 || evicted > 0 => {
                tp_core::log_info!("presence sweep marked {marked} stale, evicted {evicted}");
            }
            Ok(_) => {}
            Err(e) => tp_core::log_warn!("presence sweep failed (will retry): {e:#}"),
        }

        let found = tp_reach::scan_all(&signatures);
        // An empty scan is only believed on the SECOND consecutive cycle. The
        // scan's inputs (`ps`, tmux, osascript) all degrade silently to empty,
        // so a single empty result cannot be told apart from "I could not look"
        // — and acting on it deletes every scan-tracked session on the machine.
        // One cycle of delay is free; the sessions are already registered and
        // stay addressable throughout. This is the same grace-then-act shape as
        // `sweep_declared`, which marks stale long before it evicts.
        if found.is_empty() {
            empty_cycles += 1;
        } else {
            empty_cycles = 0;
        }
        let empty = if empty_cycles >= 2 {
            tp_reach::discover::EmptyScan::Authoritative
        } else {
            if empty_cycles == 1 {
                tp_core::log_info!(
                    "scan found nothing — not pruning yet (a failed `ps`/tmux/osascript looks \
                     identical to an idle machine; confirming on the next cycle)"
                );
            }
            tp_reach::discover::EmptyScan::Unverified
        };
        if let Err(e) = tp_reach::reconcile(db.conn(), &machine_id, &found, empty) {
            tp_core::log_warn!("discovery scan cycle failed (will retry): {e:#}");
        }
        std::thread::sleep(std::time::Duration::from_secs(tp_reach::SCAN_INTERVAL_SECS));
    }
}
