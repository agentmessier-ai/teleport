//! Mailbox semantics: at-least-once delivery with a delivery-attempt cap
//! (Pattern8 MAX_DELIVER), unread cursor, and dead-letter parking.

use tp_db::Db;
use tp_reach::{
    ack, enqueue, get_by_prefix, history, inbox, mark_read, pending, record_wake, wakeable,
    MAX_DELIVER,
};

fn setup() -> Db {
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine("m1", "TestMac").unwrap();
    db.ensure_runtime("claude_code", "/root").unwrap();
    db
}

#[test]
fn enqueue_then_drain() {
    let db = setup();
    let conn = db.conn();

    enqueue(
        conn,
        "m1/claude_code/sessA",
        Some("m1/claude_code/sessB"),
        "m1",
        "ask",
        "what's the port?",
        None,
    )
    .unwrap();
    let msgs = inbox(conn, "m1/claude_code/sessA").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].body, "what's the port?");
    assert_eq!(
        msgs[0].from_session.as_deref(),
        Some("m1/claude_code/sessB")
    );

    // Drain marks read; inbox is now empty.
    mark_read(conn, &msgs[0].id).unwrap();
    assert!(inbox(conn, "m1/claude_code/sessA").unwrap().is_empty());
}

#[test]
fn undrained_session_is_woken_with_cap() {
    let db = setup();
    let conn = db.conn();
    let sid = "m1/claude_code/sessA";

    enqueue(conn, sid, None, "m1", "note", "hello", None).unwrap();
    let msg = wakeable(conn, sid).unwrap().remove(0);

    // Simulate repeated wake attempts with no drain. Mirrors the daemon loop:
    // every tick, wakeable() returns it, record_wake() bumps attempts.
    let mut dead = false;
    for i in 1..=MAX_DELIVER {
        let pending = wakeable(conn, sid).unwrap();
        assert!(
            !pending.is_empty(),
            "attempt {i}: message must still be wakeable before the cap"
        );
        dead = record_wake(conn, &msg.id).unwrap();
    }
    assert!(dead, "message must be parked dead at MAX_DELIVER");

    // Past the cap: no longer wakeable — but still readable on a manual drain.
    assert!(
        wakeable(conn, sid).unwrap().is_empty(),
        "dead message must not wake again"
    );
    let drained = inbox(conn, sid).unwrap();
    assert_eq!(
        drained.len(),
        1,
        "dead message stays readable via /tp inbox"
    );
    assert!(drained[0].dead_at.is_some());
}

#[test]
fn reply_to_links_messages() {
    let db = setup();
    let conn = db.conn();
    let q = enqueue(
        conn,
        "m1/claude_code/sessA",
        Some("m1/claude_code/sessB"),
        "m1",
        "ask",
        "q?",
        None,
    )
    .unwrap();
    let a = enqueue(
        conn,
        "m1/claude_code/sessB",
        Some("m1/claude_code/sessA"),
        "m1",
        "reply",
        "a!",
        Some(&q.id),
    )
    .unwrap();
    assert_eq!(a.reply_to.as_deref(), Some(q.id.as_str()));
}

#[test]
fn deliver_then_read_marks_both() {
    let db = setup();
    let conn = db.conn();
    let sid = "m1/claude_code/sessA";
    let m = enqueue(conn, sid, None, "m1", "ask", "ping", None).unwrap();

    // Wake path: record_wake sets delivered_at; target drains → mark_read.
    record_wake(conn, &m.id).unwrap();
    let drained = inbox(conn, sid).unwrap();
    assert_eq!(drained.len(), 1);
    assert!(drained[0].delivered_at.is_some());
    mark_read(conn, &m.id).unwrap();
    assert!(inbox(conn, sid).unwrap().is_empty());
}

/// The reply path's lookup primitive. Everything user-facing prints only an
/// 8-char id, so that prefix is the only handle a recipient has.
#[test]
fn get_by_prefix_resolves_the_short_id_agents_actually_see() {
    let db = setup();
    let conn = db.conn();

    let m = enqueue(
        conn,
        "m1/claude_code/sessA",
        Some("m1/pi/sessB"),
        "m1",
        "ask",
        "body",
        None,
    )
    .unwrap();

    let found = tp_reach::get_by_prefix(conn, &m.id[..8]).unwrap();
    assert_eq!(found.id, m.id);
    assert_eq!(found.from_session.as_deref(), Some("m1/pi/sessB"));

    // A prefix matching nothing is an error, not an empty success — a reply
    // that silently goes nowhere is the exact failure this whole path exists
    // to remove.
    assert!(tp_reach::get_by_prefix(conn, "ffffffff").is_err());
}

/// A message with no return address must be detectably unanswerable. Before
/// senders were recorded, a recipient wanting to reply invented an address (a
/// bare machine id), which matched no session and was never delivered while
/// the asker polled for an answer that could not arrive.
#[test]
fn a_message_without_a_sender_is_visibly_unrepliable() {
    let db = setup();
    let conn = db.conn();

    enqueue(
        conn,
        "m1/claude_code/sessA",
        None,
        "m1",
        "ask",
        "orphan",
        None,
    )
    .unwrap();
    let msgs = inbox(conn, "m1/claude_code/sessA").unwrap();
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0].from_session.is_none(),
        "the absence of a return address must survive the round trip, so callers can say so"
    );
}

/// A reply is addressed FROM the original message and linked back to it, so a
/// conversation is followable and can't be misrouted by a hand-written address.
#[test]
fn reply_is_addressed_to_the_sender_and_links_back() {
    let db = setup();
    let conn = db.conn();

    let original = enqueue(
        conn,
        "m1/claude_code/sessA",
        Some("m1/pi/sessB"),
        "m1",
        "ask",
        "do the thing",
        None,
    )
    .unwrap();

    // What `tp reply` does: target := original.from_session, link := original.id.
    let target = original.from_session.clone().unwrap();
    let answer = enqueue(
        conn,
        &target,
        Some("m1/claude_code/sessA"),
        "m1",
        "reply",
        "done",
        Some(&original.id),
    )
    .unwrap();

    // It lands in the ORIGINAL SENDER's inbox, not the recipient's.
    let sender_inbox = inbox(conn, "m1/pi/sessB").unwrap();
    assert_eq!(sender_inbox.len(), 1);
    assert_eq!(sender_inbox[0].id, answer.id);
    assert_eq!(sender_inbox[0].kind, "reply");
    assert_eq!(
        sender_inbox[0].reply_to.as_deref(),
        Some(original.id.as_str())
    );

    // And the answer is itself repliable, so the exchange can continue.
    assert!(sender_inbox[0].from_session.is_some());
}

/// The SQL guard directly, independent of any caller-side pre-check: `ack` is
/// guarded on `acked_at IS NULL` the same way `mark_read` is guarded on
/// `read_at IS NULL`, so a replayed ack cannot move the timestamp forward.
/// `tp-app::ack` also checks this before calling down here — this test exists
/// so the guard in THIS layer stays proven even if that pre-check is ever
/// refactored away as "redundant".
#[test]
fn ack_is_idempotent_at_the_storage_layer_directly() {
    let db = setup();
    let conn = db.conn();
    let msg = enqueue(
        conn,
        "m1/claude_code/sessA",
        Some("m1/claude_code/sessB"),
        "m1",
        "ask",
        "hello",
        None,
    )
    .unwrap();
    mark_read(conn, &msg.id).unwrap();

    let first = ack(conn, &msg.id).unwrap();
    // A real gap, not a coincidence: without the WHERE guard the second call
    // would overwrite acked_at with a LATER timestamp, and the first version
    // of this test caught nothing — ack's return value is freshly computed
    // on every call regardless of whether the row actually changed, so
    // comparing two return values passed even with the guard removed.
    // Querying stored state, with a gap large enough for the clock to move,
    // is what makes this a real assertion.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = ack(conn, &msg.id).unwrap();
    assert!(second > first, "the test's own gap must be observable");

    let stored = get_by_prefix(conn, &msg.id).unwrap();
    assert_eq!(
        stored.acked_at,
        Some(first),
        "a replayed ack must not move the STORED timestamp forward"
    );
}

/// `ack` before `mark_read` must not silently succeed — the WHERE clause
/// requires `read_at IS NOT NULL`, so acking a message nothing has ever drained
/// is a no-op at this layer (the caller, `tp-app::ack`, is what turns that into
/// a real error the user sees; here it is just verified that the row truly did
/// not change).
#[test]
fn ack_before_read_does_not_set_acked_at() {
    let db = setup();
    let conn = db.conn();
    let msg = enqueue(
        conn,
        "m1/claude_code/sessA",
        Some("m1/claude_code/sessB"),
        "m1",
        "ask",
        "unread",
        None,
    )
    .unwrap();

    ack(conn, &msg.id).unwrap();
    let still = inbox(conn, "m1/claude_code/sessA").unwrap();
    assert_eq!(still.len(), 1, "an unread message must still be unread");
    assert!(still[0].read_at.is_none());
}

/// `pending`/`history` at the crate boundary tp-app actually calls through:
/// delivered-not-acked shows under `pending`; once acked it moves to
/// `history` and leaves `pending`.
#[test]
fn pending_and_history_split_on_ack() {
    let db = setup();
    let conn = db.conn();
    let msg = enqueue(
        conn,
        "m1/claude_code/sessA",
        Some("m1/claude_code/sessB"),
        "m1",
        "ask",
        "split me",
        None,
    )
    .unwrap();
    mark_read(conn, &msg.id).unwrap();

    assert_eq!(pending(conn, "m1/claude_code/sessA").unwrap().len(), 1);
    assert!(history(conn, "m1/claude_code/sessA", 0).unwrap().is_empty());

    ack(conn, &msg.id).unwrap();

    assert!(
        pending(conn, "m1/claude_code/sessA").unwrap().is_empty(),
        "acked messages must leave the pending view"
    );
    let hist = history(conn, "m1/claude_code/sessA", 0).unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].id, msg.id);
}

/// A pane that owns two conversations must still read one mailbox.
///
/// `join_conversation` keys on `(pid, pid_start, cwd)` and mints a new row when
/// that key does not match — which ordinary work causes two ways: `pid_start` is
/// NULL on rows predating migration 0008 (added without a backfill, and 6 of the
/// 8 such rows on the dev machine belong to exited processes, so it can never be
/// filled), and `cwd` changes when an agent `cd`s into a subdirectory. Both were
/// observed live; three panes on that machine own twins today.
///
/// The consequence is not the extra row. It is that `inbox` used to ask only
/// about the conversation the CURRENT session joined, so mail addressed to the
/// twin was invisible to a window sitting open and idle — reproduced on
/// 2026-08-21 with a real message.
mod twins {
    use super::*;
    use tp_db::reach::{join_conversation, ConversationKey};

    /// Two sessions in one pane, split by a `cwd` change — the split that
    /// happens during ordinary work rather than only on old rows.
    fn pane_with_twins(db: &Db) -> (String, String, String, String) {
        let conn = db.conn();
        let (older, newer) = ("m1/claude_code/segA", "m1/claude_code/segB");
        let key = |cwd| ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 4242,
            pid_start: Some("Fri Aug 21 09:00:00 2026"),
            cwd: Some(cwd),
        };
        let conv_a =
            join_conversation(conn, older, key("/work"), 1_000, "m1/claude_code/conv-aaa").unwrap();
        let conv_b = join_conversation(
            conn,
            newer,
            key("/work/sub"),
            2_000,
            "m1/claude_code/conv-bbb",
        )
        .unwrap();
        assert_ne!(
            conv_a, conv_b,
            "a cwd change must split the pane, or this test proves nothing"
        );
        (older.into(), newer.into(), conv_a, conv_b)
    }

    /// The core repair: a session reads every mailbox its pane owns.
    #[test]
    fn a_session_drains_mail_addressed_to_its_twin() {
        let db = setup();
        let (older, newer, _, _) = pane_with_twins(&db);

        // Addressed to the OLDER segment — the twin the current session is not
        // a member of. Before the fix this was unreachable from `newer`.
        enqueue(
            db.conn(),
            &older,
            Some("m1/claude_code/other"),
            "m1",
            "ask",
            "parked in the twin",
            None,
        )
        .unwrap();

        let got = inbox(db.conn(), &newer).unwrap();
        assert_eq!(
            got.len(),
            1,
            "a message in the pane's other conversation must still be read"
        );
        assert_eq!(got[0].body, "parked in the twin");
    }

    /// Oldest-first has to hold ACROSS mailboxes, not within each — a drain that
    /// concatenated per-conversation reads would hand them over in row order.
    #[test]
    fn mail_from_both_twins_arrives_in_time_order() {
        let db = setup();
        let (older, newer, _, _) = pane_with_twins(&db);
        let send = |to: &str, body: &str| {
            enqueue(
                db.conn(),
                to,
                Some("m1/claude_code/other"),
                "m1",
                "ask",
                body,
                None,
            )
            .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        // Interleaved: twin, self, twin.
        send(&older, "first");
        send(&newer, "second");
        send(&older, "third");

        let bodies: Vec<String> = inbox(db.conn(), &newer)
            .unwrap()
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(bodies, ["first", "second", "third"]);
    }

    /// The SEND half of the same repair, and the half that had no test at all
    /// until this one: the head of the list is the address a reply stamps.
    ///
    /// Ordering is the whole contract here. `sender_address` takes `[0]`, so
    /// "most recently seen first" is not a convenience for reading — it is what
    /// makes the stamped address live by construction rather than by luck. A
    /// sort that drifted to creation order would hand out the OLDER twin, which
    /// is exactly the address the bug parked messages on.
    #[test]
    fn the_pane_address_is_the_one_most_recently_seen() {
        let db = setup();
        let (older, newer, conv_a, conv_b) = pane_with_twins(&db);

        // conv_b was joined second (now = 2_000), so it is the live one.
        let from_new = tp_reach::conversations_of_pane(db.conn(), &newer).unwrap();
        assert_eq!(from_new[0], conv_b, "the head must be the live twin");

        // And the answer does not depend on WHICH member session asks — a
        // session that belongs to the older twin still publishes the live
        // address, which is the case the bug produced.
        let from_old = tp_reach::conversations_of_pane(db.conn(), &older).unwrap();
        assert_eq!(
            from_old[0], conv_b,
            "a session in the stale twin must still stamp the live address"
        );

        // Both list the same set, so the read half sees the same mailboxes from
        // either side.
        let mut a = from_new.clone();
        let mut b = from_old.clone();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        assert!(a.contains(&conv_a) && a.contains(&conv_b));
    }

    /// `pending` is the recovery view; it must span the pane for the same reason
    /// `inbox` does, or an interrupted drain loses exactly the messages the twin
    /// held.
    #[test]
    fn pending_also_spans_the_pane() {
        let db = setup();
        let (older, newer, _, _) = pane_with_twins(&db);
        enqueue(
            db.conn(),
            &older,
            Some("m1/claude_code/other"),
            "m1",
            "ask",
            "parked in the twin",
            None,
        )
        .unwrap();

        let got = inbox(db.conn(), &newer).unwrap();
        for m in &got {
            mark_read(db.conn(), &m.id).unwrap();
        }
        let still = pending(db.conn(), &newer).unwrap();
        assert_eq!(
            still.len(),
            1,
            "read but unacked, and visible from the twin"
        );
    }
}
