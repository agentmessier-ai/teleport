//! Live-session resolution tests. The optimistic path (a real tmux/iTerm2
//! session) can't run headless, so these assert the stable meanings of the
//! harder cases.

use tp_db::Db;
use tp_reach::{register, resolve, unregister, Target};

/// An alive pid guaranteed to have NO controlling tty, regardless of what
/// terminal/tty this TEST PROCESS itself happens to be running under —
/// `std::process::id()` is NOT safe for that (on a machine with a real
/// iTerm2 session live at the test runner's own tty, `resolve()` would find
/// it and return `Target::Terminal`, not `Target::Unreachable`). A detached
/// child with stdio piped away from any terminal has none, everywhere.
struct TtylessChild(std::process::Child);
impl TtylessChild {
    fn spawn() -> Self {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }
    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
}
impl Drop for TtylessChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn no_binding_is_not_live() {
    let db = Db::open_in_memory().unwrap();
    let t = resolve(db.conn(), "m1/claude_code/nope").unwrap();
    assert_eq!(t, Target::NotLive);
}

#[test]
fn register_then_unregister_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    let child = TtylessChild::spawn();
    register(
        db.conn(),
        "m1/claude_code/sess1",
        child.pid(),
        Some("/dev/ttys000"),
        None,
    )
    .unwrap();
    // No tmux pane and no matching iTerm2 session for a genuinely tty-less
    // pid → Unreachable, not NotLive (the pid IS alive).
    let t = resolve(db.conn(), "m1/claude_code/sess1").unwrap();
    assert_eq!(
        t,
        Target::Unreachable,
        "alive pid + no tty must be Unreachable, not NotLive"
    );

    unregister(db.conn(), "m1/claude_code/sess1", None).unwrap();
    assert_eq!(
        resolve(db.conn(), "m1/claude_code/sess1").unwrap(),
        Target::NotLive
    );
}

#[test]
fn unregister_is_a_noop_when_the_session_id_was_reclaimed_by_a_different_pid() {
    // session_id reuse across `/clear`: SessionEnd for the OLD process racing
    // SessionStart for the NEW one — see docs/same-machine-poke-design.md's
    // ordering risk. If the new registration already overwrote the row by
    // the time the old process's SessionEnd hook runs, unregistering by
    // session_id alone would delete the WRONG (newer, still-live) binding.
    let db = Db::open_in_memory().unwrap();
    let old_child = TtylessChild::spawn();
    let new_child = TtylessChild::spawn();

    register(
        db.conn(),
        "m1/claude_code/reused",
        old_child.pid(),
        None,
        None,
    )
    .unwrap();
    // The new session's SessionStart lands first and reclaims the same id.
    register(
        db.conn(),
        "m1/claude_code/reused",
        new_child.pid(),
        None,
        None,
    )
    .unwrap();

    // The OLD process's SessionEnd now fires, pinned to its own (now stale) pid.
    unregister(db.conn(), "m1/claude_code/reused", Some(old_child.pid())).unwrap();
    assert_ne!(
        resolve(db.conn(), "m1/claude_code/reused").unwrap(),
        Target::NotLive,
        "unregistering with a stale pid must not remove the newer registration"
    );

    // The NEW process's own SessionEnd, pinned to the pid that's actually stored, proceeds normally.
    unregister(db.conn(), "m1/claude_code/reused", Some(new_child.pid())).unwrap();
    assert_eq!(
        resolve(db.conn(), "m1/claude_code/reused").unwrap(),
        Target::NotLive
    );
}

#[test]
fn dead_pid_is_dropped_and_reports_not_live() {
    let db = Db::open_in_memory().unwrap();
    // A pid that cannot exist.
    register(
        db.conn(),
        "m1/claude_code/sess2",
        2_147_483_647,
        Some("/dev/ttys000"),
        None,
    )
    .unwrap();
    let t = resolve(db.conn(), "m1/claude_code/sess2").unwrap();
    assert_eq!(
        t,
        Target::NotLive,
        "a dead pid must resolve to NotLive, not Unreachable"
    );
    // And the stale binding must have been cleaned up.
    assert_eq!(
        resolve(db.conn(), "m1/claude_code/sess2").unwrap(),
        Target::NotLive
    );
}

#[test]
fn find_session_process_walks_up_to_live_ancestor() {
    // Walk from a child with a deterministic, un-truncatable comm (`sleep`)
    // rather than from our own test process: macOS `ps -o comm=` truncates the
    // exec path to 16 chars, and a cargo-spawned test binary's comm varies by
    // runner (relative path, absolute path, hashed binary name), so it can't
    // be matched portably. `sleep` survives the truncation on every runner.
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let found = tp_reach::resolve::find_session_process(child.id() as i32, "sleep");
    assert!(
        found.is_some(),
        "walking up from a `sleep` child must find it by its comm"
    );

    // A needle that exists nowhere must terminate cleanly (None), never panic.
    let absent = tp_reach::resolve::find_session_process(child.id() as i32, "no-such-process");
    let _ = absent;

    let _ = child.kill();
    let _ = child.wait();
}

/// `session_of_process` answers "which registered session am I inside" from the
/// REGISTRY, keyed by pid — the one identity that can't fork. Env vars can and
/// do disagree with what was registered (observed after a `--resume`: the same
/// pid had one id in `$CLAUDE_CODE_SESSION_ID` and another in `live_session`,
/// so reads and replies silently addressed a mailbox nobody writes to).
#[test]
fn session_of_process_finds_this_processs_own_registration() {
    let db = Db::open_in_memory().unwrap();
    let me = std::process::id() as i32;
    register(db.conn(), "m1/claude_code/mine", me, None, None).unwrap();

    let found = tp_reach::session_of_process(db.conn(), me).unwrap();
    assert_eq!(found.as_deref(), Some("m1/claude_code/mine"));
}

/// The load-bearing case: `tp` is never the agent — it runs as a DESCENDANT of
/// it. Resolution has to walk up the parent chain to the registered ancestor,
/// which is what makes this work for a runtime with no session env var at all
/// (pi shelling out to `tp` from bash).
#[test]
fn session_of_process_walks_up_to_a_registered_ancestor() {
    let db = Db::open_in_memory().unwrap();
    let parent = unsafe { libc::getppid() };
    register(db.conn(), "m1/pi/ancestor", parent, None, None).unwrap();

    // Asked about OURSELVES; the answer must come from our parent.
    let found = tp_reach::session_of_process(db.conn(), std::process::id() as i32).unwrap();
    assert_eq!(found.as_deref(), Some("m1/pi/ancestor"));
}

/// Nothing registered anywhere up the chain is a clean `None`, not an error and
/// not a guess — the caller then says so instead of inventing an address.
#[test]
fn session_of_process_is_none_when_no_ancestor_is_registered() {
    let db = Db::open_in_memory().unwrap();
    let found = tp_reach::session_of_process(db.conn(), std::process::id() as i32).unwrap();
    assert!(found.is_none());
}
