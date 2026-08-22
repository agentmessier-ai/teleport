//! Mailbox (LLD §7.3): messages live in the DB, never typed into a peer pane.
//! The only thing that crosses into a pane is a fixed control string; the
//! target reads its body from the DB via `/tp inbox`.
//!
//! Delivery-attempt cap (Pattern8 MAX_DELIVER): a session that never drains its
//! inbox must not be woken forever. Past the cap, the row is marked `dead_at`
//! and stays readable but never triggers another wake.
//!
//! The SQL lives in `tp_db::reach`, next to the migrations that define these
//! columns. What stays here is the policy: the cap, the clock, and what an
//! ambiguous prefix means.

use anyhow::Result;
use tp_db::reach;
use tp_db::DbConnection as Connection;

pub use tp_db::reach::Message;

/// Max wake attempts before a message is parked as dead.
pub const MAX_DELIVER: i64 = 5;

/// Enqueue a message into a target's mailbox.
pub fn enqueue(
    conn: &Connection,
    to_session: &str,
    from_session: Option<&str>,
    from_machine: &str,
    kind: &str,
    body: &str,
    reply_to: Option<&str>,
) -> Result<Message> {
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = now_ms();
    reach::insert_message(
        conn,
        &id,
        to_session,
        from_session,
        from_machine,
        kind,
        body,
        reply_to,
        created_at,
    )?;
    Ok(Message {
        id,
        to_session: to_session.to_string(),
        from_session: from_session.map(|s| s.to_string()),
        from_machine: from_machine.to_string(),
        kind: kind.to_string(),
        body: body.to_string(),
        reply_to: reply_to.map(|s| s.to_string()),
        created_at,
        delivered_at: None,
        read_at: None,
        attempts: 0,
        dead_at: None,
        acked_at: None,
    })
}

/// Read across every conversation this PANE owns, not just the one the current
/// session belongs to.
///
/// A pane can own several (see `conversations_of_pane`), and mail addressed to
/// one twin is invisible to a session sitting in the other. Not a theoretical
/// ordering: a message parked in a twin's mailbox on 2026-08-21 sat undelivered
/// while the window that should have read it was open and idle, because this
/// path asked only about the conversation that session happened to have joined.
///
/// Sorted by the message clock rather than any one mailbox's, for the same
/// reason the per-conversation read already was — a drain spanning several
/// mailboxes must still hand messages over oldest-first.
fn across_pane(
    conn: &Connection,
    session_id: &str,
    per_conversation: impl Fn(&Connection, &str) -> Result<Vec<Message>>,
    fallback: impl FnOnce(&Connection, &str) -> Result<Vec<Message>>,
) -> Result<Vec<Message>> {
    let convs = reach::conversations_of_pane(conn, session_id)?;
    if convs.is_empty() {
        // No conversation at all — a session registered before conversations
        // existed, or a runtime that never registers. Its own mailbox, as before.
        return fallback(conn, session_id);
    }
    let mut out = Vec::new();
    for c in &convs {
        out.extend(per_conversation(conn, c)?);
    }
    out.sort_by_key(|m| m.created_at);
    Ok(out)
}

/// Unread messages for a session (the `/tp inbox` drain).
///
/// If this session belongs to a conversation, the drain covers every id that
/// conversation has answered to — mail addressed before a compaction is sitting
/// in a mailbox whose id nothing drains any more, and collecting it is the whole
/// point of having a conversation address. A session with no conversation (an
/// older row, a runtime that never registered) reads exactly its own mailbox,
/// which is the behaviour that existed before.
pub fn inbox(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    across_pane(
        conn,
        session_id,
        reach::unread_for_conversation,
        reach::unread,
    )
}

/// Delivered-but-unacked messages for a session — the recovery view for a
/// drain interrupted before it finished acting on everything. Conversation-
/// aware for the same reason `inbox` is: a message parked before a compaction
/// must recover the same way here as it does on the normal read path.
/// Read-only — unlike `inbox`, calling this does not change anything, so
/// checking it never counts as processing.
pub fn pending(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    across_pane(
        conn,
        session_id,
        reach::pending_ack_for_conversation,
        reach::pending_ack,
    )
}

/// Acked messages for a session since `since_ms` — "what did that say again",
/// not a work queue. Same conversation-aware dispatch as `inbox`/`pending`.
pub fn history(conn: &Connection, session_id: &str, since_ms: i64) -> Result<Vec<Message>> {
    across_pane(
        conn,
        session_id,
        |c, id| reach::acked_since_for_conversation(c, id, since_ms),
        |c, sid| reach::acked_since(c, sid, since_ms),
    )
}

/// Confirm a message finished being acted on — NOT the same as `mark_read`,
/// which fires the instant a message is shown. Returns the timestamp it used,
/// so a caller already holding the `Message` can update its own copy instead
/// of re-querying (the same shape `record_wake` already uses below).
pub fn ack(conn: &Connection, id: &str) -> Result<i64> {
    let now = now_ms();
    reach::ack(conn, id, now)?;
    Ok(now)
}

/// Look a message up by an id PREFIX. Everything user-facing prints only the
/// first 8 chars of a message id (`queued 369d2bd2 → …`), so that is the only
/// handle a caller — or an agent reading its inbox — actually has to refer
/// back to a message with. An ambiguous prefix is an error rather than a
/// silent first-match: replying to the wrong conversation is worse than being
/// told to be more specific.
pub fn get_by_prefix(conn: &Connection, prefix: &str) -> Result<Message> {
    let mut rows = reach::by_prefix(conn, prefix)?;
    match rows.len() {
        0 => anyhow::bail!("no message with id starting {prefix:?}"),
        1 => Ok(rows.remove(0)),
        _ => anyhow::bail!("message id {prefix:?} is ambiguous — use more characters of the id"),
    }
}

/// Mark a message read (target actually drained it).
pub fn mark_read(conn: &Connection, id: &str) -> Result<()> {
    reach::mark_read(conn, id, now_ms())
}

/// Record a wake attempt. Returns true if the message is now parked as dead
/// (past MAX_DELIVER); caller should stop waking it.
pub fn record_wake(conn: &Connection, id: &str) -> Result<bool> {
    let now = now_ms();
    let attempts = reach::attempts_of(conn, id)? + 1;
    let dead = attempts >= MAX_DELIVER;
    reach::set_attempt(conn, id, attempts, now, dead.then_some(now))?;
    Ok(dead)
}

/// Messages a session should wake for (undelivered, not dead, not read).
pub fn wakeable(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    reach::wakeable(conn, session_id)
}

/// Re-exported from `tp-core`, where the single copy lives.
pub fn now_ms() -> i64 {
    tp_core::now_ms()
}
