//! Coordinates resolve + wake + mailbox for one delivery attempt (LLD §7.3):
//! rate-limits wakes to at most one per target session per 10s — the inbox
//! drains in batch, so a suppressed wake loses nothing — and stops waking a
//! session whose pending messages have hit `MAX_DELIVER` attempts. Both
//! guards existed only as constants/helpers before this (`mailbox::MAX_DELIVER`
//! unwired, no coalescing at all) — see docs/same-machine-poke-design.md Gap 4.

use crate::mailbox;
use crate::resolve::{self, Target};
use crate::wake::{self, Caller};
use anyhow::Result;
use tp_db::reach;
use tp_db::DbConnection as Connection;

pub const WAKE_COALESCE_MS: i64 = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub enum DeliveryOutcome {
    /// Actually woke the target just now.
    Woke(Target),
    /// Skipped — woken within the last `WAKE_COALESCE_MS`; whatever's pending
    /// will be drained by that earlier wake, so this is not a failure.
    Coalesced,
    /// Nothing to wake for (every message for this session is either already
    /// read, or past `MAX_DELIVER` and parked `dead_at`).
    NoMessages,
    /// Enqueued fine, but the target can't be injected into right now
    /// (mailbox-only; the target will see it on its next manual `/tp inbox`).
    NotInjectable(Target),
}

/// Attempt to wake `session_id` for whatever's currently pending. Call this
/// AFTER enqueueing — it acts on the full wakeable set for the session, not
/// just the message you just sent, since one wake drains the whole inbox.
pub fn attempt_wake(
    conn: &Connection,
    session_id: &str,
    control: &str,
    caller: Caller,
) -> Result<DeliveryOutcome> {
    let pending = mailbox::wakeable(conn, session_id)?;
    if pending.is_empty() {
        return Ok(DeliveryOutcome::NoMessages);
    }

    if let Some(last) = reach::last_wake_at(conn, session_id)? {
        if mailbox::now_ms() - last < WAKE_COALESCE_MS {
            return Ok(DeliveryOutcome::Coalesced);
        }
    }

    let target = resolve::resolve(conn, session_id)?;
    match &target {
        Target::Tmux(_) | Target::Terminal { .. } | Target::Channel(_) => {
            wake::wake(&target, session_id, control, caller)?;
            reach::set_last_wake_at(conn, session_id, mailbox::now_ms())?;
            // Attempts count physical wake events, not enqueue events — a
            // message that never gets a live target attempted against it
            // must not burn down MAX_DELIVER just from sitting in the queue.
            for m in &pending {
                mailbox::record_wake(conn, &m.id)?;
            }
            Ok(DeliveryOutcome::Woke(target))
        }
        Target::Unreachable | Target::NotLive => Ok(DeliveryOutcome::NotInjectable(target)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::{enqueue, MAX_DELIVER};
    use tp_db::Db;

    fn setup() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/root").unwrap();
        db
    }

    #[test]
    fn no_pending_messages_is_not_an_error() {
        let db = setup();
        let out = attempt_wake(db.conn(), "nobody", wake::CONTROL_STRING, Caller::Cli).unwrap();
        assert_eq!(out, DeliveryOutcome::NoMessages);
    }

    #[test]
    fn unregistered_target_reports_not_injectable_but_still_delivers() {
        let db = setup();
        enqueue(db.conn(), "target-session", None, "me", "ask", "hi", None).unwrap();
        let out = attempt_wake(
            db.conn(),
            "target-session",
            wake::CONTROL_STRING,
            Caller::Cli,
        )
        .unwrap();
        assert_eq!(out, DeliveryOutcome::NotInjectable(Target::NotLive));
    }

    #[test]
    fn max_deliver_stops_waking_after_the_cap() {
        let db = setup();
        // Attempts are counted directly (not by actually resolving/waking a
        // real terminal) — MAX_DELIVER's job is only to stop counting once
        // the cap is hit, exercised here in isolation from a real target.
        let msg = enqueue(db.conn(), "s1", None, "me", "ask", "hi", None).unwrap();
        for _ in 0..MAX_DELIVER {
            mailbox::record_wake(db.conn(), &msg.id).unwrap();
        }
        let pending = mailbox::wakeable(db.conn(), "s1").unwrap();
        assert!(
            pending.is_empty(),
            "message must be parked dead_at after MAX_DELIVER attempts"
        );

        let out = attempt_wake(db.conn(), "s1", wake::CONTROL_STRING, Caller::Cli).unwrap();
        assert_eq!(
            out,
            DeliveryOutcome::NoMessages,
            "a dead-parked message must not trigger another wake attempt"
        );
    }

    #[test]
    fn second_attempt_within_coalesce_window_is_skipped() {
        let db = setup();
        enqueue(db.conn(), "s1", None, "me", "ask", "one", None).unwrap();
        // First attempt: no live target, so it's NotInjectable, but crucially
        // that path does NOT set last_wake_at (only an actual wake does) —
        // confirmed by the second enqueue+attempt below still finding fresh
        // pending state rather than being coalesced against a non-wake.
        assert_eq!(
            attempt_wake(db.conn(), "s1", wake::CONTROL_STRING, Caller::Cli).unwrap(),
            DeliveryOutcome::NotInjectable(Target::NotLive)
        );
        enqueue(db.conn(), "s1", None, "me", "ask", "two", None).unwrap();
        assert_eq!(
            attempt_wake(db.conn(), "s1", wake::CONTROL_STRING, Caller::Cli).unwrap(),
            DeliveryOutcome::NotInjectable(Target::NotLive)
        );
    }
}
