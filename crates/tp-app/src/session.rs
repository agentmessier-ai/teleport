//! Session lifecycle: register, renew, unregister.
//!
//! No duplication to remove here — no surface but the CLI has ever exposed
//! these, and registering is something a runtime does to itself rather than
//! something an agent asks for. They are extracted for the other reason: this
//! is the path tonight's identity bug was on, where a pid hosting several
//! sessions made `session_of_process` pick arbitrarily and stamped the wrong
//! sender on every message. Verifying the fix meant editing the database by hand
//! and running the real binary, because there was no function to call.
//!
//! Everything below returns a value, so the next fix on this path can be
//! asserted rather than observed.

use anyhow::Result;
use tp_db::Db;
use tp_reach::resolve::{DeliveryChannel, Presence};

/// Where a session says it can be found.
///
/// `pid` is separated from "work it out by walking the process tree" on
/// purpose. The walk finds the nearest ancestor matching a runtime's process
/// name, which is right for a runtime that spawns `tp` as a child and WRONG for
/// a host that multiplexes several sessions: a dsh host started from a Claude
/// Code session registered every one of its sessions under the CLAUDE process's
/// pid, which then routed replies to dsh itself (f178516).
#[derive(Debug, Clone)]
pub enum Host {
    /// The runtime states its own pid. Always correct when available, because
    /// the runtime knows and teleport does not.
    Declared(i32),
    /// Walk up from `from_pid` looking for a process whose name matches
    /// `needle`, falling back to `from_pid` itself.
    Inferred { from_pid: i32, needle: String },
}

/// What a registration recorded.
#[derive(Debug, Clone)]
pub struct Registered {
    pub session_id: String,
    pub pid: i32,
    pub tty: Option<String>,
    pub presence: Presence,
    pub deliver: Option<String>,
}

/// Parse a presence name.
///
/// Here rather than in the CLI because it is a domain rule with a closed set,
/// and the error naming the valid values is part of it.
pub fn parse_presence(s: &str) -> Result<Presence> {
    match s {
        "scan" => Ok(Presence::Scan),
        "declared" => Ok(Presence::Declared),
        other => anyhow::bail!("unknown presence {other:?} (want `scan` or `declared`)"),
    }
}

/// Record (or refresh) where a session is.
///
/// The delivery channel is validated BEFORE anything is written: a registration
/// carrying an unusable channel is worse than none, because the session then
/// looks reachable and never is.
pub fn register(
    db: &Db,
    session_id: &str,
    host: Host,
    cwd: Option<&str>,
    presence: Presence,
    deliver: Option<&str>,
) -> Result<Registered> {
    if let Some(raw) = deliver {
        DeliveryChannel::parse(raw)?;
    }

    let (pid, tty) = match host {
        Host::Declared(pid) => (pid, None),
        Host::Inferred { from_pid, needle } => {
            match tp_reach::resolve::find_session_process(from_pid, &needle) {
                Some((pid, tty)) => (pid, Some(tty)),
                None => (from_pid, None),
            }
        }
    };

    tp_reach::resolve::register_with(
        db.conn(),
        session_id,
        pid,
        tty.as_deref(),
        cwd,
        presence,
        deliver,
    )?;

    Ok(Registered {
        session_id: session_id.to_string(),
        pid,
        tty,
        presence,
        deliver: deliver.map(str::to_string),
    })
}

/// Remove a registration, pinned to the pid that owns it.
///
/// `expected_pid` is not optional politeness. A session id can be REUSED across
/// a `/clear`: SessionEnd for the old incarnation can race SessionStart for the
/// new one, and an unpinned delete then unregisters the live session by id
/// collision. `None` skips the check and is for callers with no pid to compare.
pub fn unregister(db: &Db, session_id: &str, expected_pid: Option<i32>) -> Result<()> {
    tp_reach::unregister(db.conn(), session_id, expected_pid)
}

/// Renew a declared session's presence.
///
/// Returns whether a row was actually renewed, so a runtime beating into a
/// session teleport has already evicted learns it must re-register rather than
/// beating into the void.
pub fn heartbeat(db: &Db, session_id: &str) -> Result<bool> {
    tp_reach::heartbeat(db.conn(), session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db
    }

    /// A declared pid is used verbatim. This is the fix for the bug that made
    /// this module worth extracting: a multiplexing host knows which process it
    /// is, and teleport's walk would find whatever launched it instead.
    #[test]
    fn a_declared_host_is_taken_at_its_word() {
        let db = db();
        let r = register(
            &db,
            "m1/dsh/session-a",
            Host::Declared(4242),
            Some("/w"),
            Presence::Declared,
            Some("http://127.0.0.1:3080/wake"),
        )
        .unwrap();
        assert_eq!(r.pid, 4242);
        assert_eq!(r.tty, None, "a declared host reports no tty to infer");
    }

    /// An unusable channel is refused before anything is written — a session
    /// registered with one looks reachable and never is.
    #[test]
    fn a_bad_delivery_channel_is_refused_and_nothing_is_recorded() {
        let db = db();
        let err = register(
            &db,
            "m1/dsh/x",
            Host::Declared(1),
            None,
            Presence::Declared,
            // Not loopback: a routable URL would let anything on the network
            // poke this session.
            Some("http://10.0.0.5:3080/wake"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("loopback"), "{err}");

        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "a refused registration must leave no row");
    }

    /// The pid pin, which exists because a session id can be reused across a
    /// `/clear` and an unpinned delete would unregister the NEW incarnation.
    #[test]
    fn unregister_does_not_remove_a_row_owned_by_another_pid() {
        let db = db();
        register(
            &db,
            "m1/claude_code/s",
            Host::Declared(100),
            None,
            Presence::Scan,
            None,
        )
        .unwrap();
        unregister(&db, "m1/claude_code/s", Some(999)).unwrap();
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            rows, 1,
            "a stale SessionEnd must not evict the live session"
        );

        unregister(&db, "m1/claude_code/s", Some(100)).unwrap();
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM live_session", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn a_heartbeat_into_an_evicted_session_reports_that_it_is_gone() {
        let db = db();
        assert!(!heartbeat(&db, "m1/dsh/never-registered").unwrap());
        register(
            &db,
            "m1/dsh/s",
            Host::Declared(1),
            None,
            Presence::Declared,
            None,
        )
        .unwrap();
        assert!(heartbeat(&db, "m1/dsh/s").unwrap());
    }

    #[test]
    fn presence_names_its_valid_values_when_wrong() {
        let err = parse_presence("sometimes").unwrap_err().to_string();
        assert!(err.contains("scan"), "{err}");
        assert!(err.contains("declared"), "{err}");
    }
}
