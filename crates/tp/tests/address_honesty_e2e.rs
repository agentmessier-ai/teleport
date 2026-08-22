//! End-to-end: `tp ask` must never claim delivery it cannot make.
//!
//! Written from measurement, not suspicion. On a real install, 13 messages sat
//! unread and 12 of them were undeliverable — every one reported at send time as
//! "session not live — delivered on next /tp inbox", which reads like success
//! with a delay. Two distinct causes:
//!
//!   * 4 had addresses that were never addresses: bare native ids with no
//!     `<machine>/<runtime>/` prefix, a bare machine id with no session part,
//!     and one truncated mid-runtime (`…/claud`). Agents had assembled them.
//!   * 8 were addressed to real sessions whose ids had rotated — Claude Code
//!     mints a new session id at every compaction, so an address published by
//!     `tp live` an hour earlier belonged to nobody.
//!
//! A unit test on the classifier could not have caught either: the defect was
//! that the send path never asked it. So this runs the real binary and asserts
//! on what a caller actually sees.

use std::path::Path;
use std::process::Command;

fn tp(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tp"))
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run tp");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// Every malformed address observed in the wild, as one table. Each must fail
/// BEFORE the message is stored — a rejected send is recoverable, a stored one
/// that is never delivered is not.
#[test]
fn a_malformed_address_is_refused_and_nothing_is_enqueued() {
    let home = tempfile::tempdir().unwrap();
    for bad in [
        "62407be6-b2f2-4595-a8c8-03d12cb817e6", // bare native id
        "SWSQ-BCLE-LXKA-MP2R",                  // bare machine id
        "SWSQ-BCLE-LXKA-MP2R/claud",            // truncated before the native id
        "machine//native",                      // empty runtime segment
    ] {
        let (ok, out) = tp(home.path(), &["ask", bad, "hello"]);
        assert!(!ok, "{bad:?} must be rejected, got success:\n{out}");
        assert!(
            out.contains("is not a session address"),
            "{bad:?} must say why it is not an address:\n{out}"
        );
        assert!(
            out.contains("tp live"),
            "{bad:?} must point at where real addresses come from:\n{out}"
        );
    }

    // Nothing was written. If a refusal still enqueued, the message would be
    // exactly as lost as before, just louder.
    let (_, inbox) = tp(home.path(), &["inbox", "--session-id", "machine/rt/native"]);
    assert!(
        inbox.contains("inbox empty"),
        "a refused send must not leave a message behind:\n{inbox}"
    );
}

/// A well-formed but unknown address is ACCEPTED — a mailbox deliberately has no
/// FK to `session`, so an id can legitimately arrive before its session does
/// (migration 0001). What must change is the claim: it is parked, not delivered.
#[test]
fn an_unknown_address_is_accepted_but_never_reported_as_delivered() {
    let home = tempfile::tempdir().unwrap();
    let target = "SWSQ-BCLE-LXKA-MP2R/claude_code/00000000-1111-2222-3333-444444444444";

    let (ok, out) = tp(home.path(), &["ask", target, "hello"]);
    assert!(
        ok,
        "a well-formed unknown address must still be accepted:\n{out}"
    );
    // Two claims, and the second was learned the hard way. The message must not
    // be reported as DELIVERED — and it must not be reported as FAILED either,
    // because it is stored: a session read "PARKED, NOT DELIVERED" as a
    // rejection, resent twice, and delivered three copies of one report.
    assert!(
        out.contains("STORED"),
        "an undeliverable send must still say the message was kept:\n{out}"
    );
    assert!(
        out.contains("nothing will drain it"),
        "...and must say plainly that nobody is going to read it:\n{out}"
    );
    assert!(
        out.contains("Do not resend"),
        "...and must say not to retry, which is what a reader does with an \
         apparent failure:\n{out}"
    );
    assert!(
        !out.contains("delivered on next /tp inbox"),
        "the old wording promised delivery teleport has no basis for:\n{out}"
    );
    // The hint below the status line is a second, independent promise. Saying
    // "end your turn and wait for the reply" under a PARKED line is the same
    // lie one line further down.
    assert!(
        out.contains("Do NOT wait"),
        "an undeliverable send must not tell the caller to wait for a reply:\n{out}"
    );
    assert!(
        !out.contains("The reply arrives later"),
        "the wait-for-reply hint must be suppressed when nothing can reply:\n{out}"
    );

    // Accepted really does mean stored — the message is there for the session
    // that may yet register under this id.
    let (_, inbox) = tp(home.path(), &["inbox", "--session-id", target]);
    assert!(
        inbox.contains("hello"),
        "an accepted message must be readable by its addressee:\n{inbox}"
    );
}

/// The return address a reply stamps must be the pane's LIVE conversation, not
/// whichever one the sending session happens to belong to.
///
/// This is the send half of issue #1, and it is here rather than in a unit test
/// for the reason this file exists: `conversations_of_pane` can be correct and
/// `sender_address` can still not call it. The defect that produced this bug was
/// never in the resolution — it was in what the send path asked for.
///
/// The pane is twinned the way ordinary work twins one: same pid and pid_start,
/// a changed cwd, which is what an agent does when it `cd`s into a subdirectory.
#[test]
fn a_reply_stamps_the_live_twin_not_the_session_s_own() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let (stale_conv, live_conv, stale_session) = {
        let db_path = home.join(".teleport/teleport.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let mut db = tp_db::Db::open(&db_path).unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();

        let key = |cwd| tp_db::reach::ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 4242,
            pid_start: Some("Fri Aug 21 09:00:00 2026"),
            cwd: Some(cwd),
        };
        let conn = db.conn_mut();
        // Older segment first, then a cwd change splits the pane.
        let stale = tp_db::reach::join_conversation(
            conn,
            "m1/claude_code/segOld",
            key("/work"),
            1_000,
            "m1/claude_code/conv-stale",
        )
        .unwrap();
        let live = tp_db::reach::join_conversation(
            conn,
            "m1/claude_code/segNew",
            key("/work/sub"),
            2_000,
            "m1/claude_code/conv-live",
        )
        .unwrap();
        assert_ne!(
            stale, live,
            "the cwd change must split, or this proves nothing"
        );
        (stale, live, "m1/claude_code/segOld".to_string())
    };

    // A message sent BY the session that belongs to the stale twin. Before the
    // fix its return address was the stale conversation — the address the
    // recipient would then reply to, and nobody would read.
    let (ok, out) = tp(
        home,
        &[
            "note",
            "m1/claude_code/someone-else",
            "hello",
            "--from-session",
            &stale_session,
        ],
    );
    assert!(ok, "{out}");

    let stamped: String = rusqlite::Connection::open(home.join(".teleport/teleport.db"))
        .unwrap()
        .query_row(
            "SELECT from_session FROM message ORDER BY created_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();

    assert_eq!(
        stamped, live_conv,
        "stamped the stale twin ({stale_conv}) — a reply to it reaches nobody"
    );
}
