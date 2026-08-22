//! Sending a message, and answering one.
//!
//! Both surfaces used to implement these twice over. See the crate doc for what
//! that cost; what matters here is that the work returns a value and says
//! nothing about how to present it.

use anyhow::Result;
use tp_db::reach::Addressability;
use tp_db::Db;

/// Whether an outgoing message is asking for something back.
///
/// `kind` has had a `note` variant in the schema since the first migration and
/// nothing produced one for 175 messages. So every status update went out
/// looking like a request, the `/tp` skill told the receiver to ANSWER IT, and
/// the sender was told to end its turn and wait for a reply nobody needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Asks for something. The receiver should answer; the sender should stop
    /// and wait for it.
    Ask,
    /// Tells the receiver something. Still wakes them — a status update that
    /// arrives hours late is not much of an update — but the receiver is told
    /// plainly that no reply is expected.
    Note,
    /// Answers a message. Addressed from the original rather than by hand, so
    /// it cannot be misrouted the way an `ask` can.
    Reply,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Ask => "ask",
            Kind::Note => "note",
            Kind::Reply => "reply",
        }
    }
}

/// What a send actually did. Every field is a fact; not one is a sentence.
#[derive(Debug, Clone)]
pub struct Sent {
    pub message_id: String,
    /// The segment the message was stored against — NOT necessarily the address
    /// the caller supplied: a conversation address resolves to whichever
    /// segment it currently answers on.
    pub target: String,
    /// Absent when this caller could not be identified, which means the target
    /// cannot reply however long anyone waits.
    pub from: Option<String>,
    pub kind: Kind,
    /// Whether anything is currently expected to drain this mailbox. Kept as
    /// the classification rather than a bool: "nobody is reading it" and "this
    /// is not an address teleport knows" call for different advice.
    pub addressability: Addressability,
}

impl Sent {
    /// Whether anything is currently reading the mailbox this went to.
    ///
    /// One half of `answerable`, exposed because a caller that distinguishes
    /// "nobody is listening" from "no way back" needs to ask for it rather than
    /// re-deriving it from `addressability` — which is what the CLI did, giving
    /// the rule two homes.
    pub fn deliverable(&self) -> bool {
        matches!(self.addressability, Addressability::Registered)
    }

    /// Whether an answer can arrive at all: something has to be reading the
    /// mailbox AND there has to be a way back.
    pub fn answerable(&self) -> bool {
        self.from.is_some() && self.deliverable()
    }
}

/// Enqueue a message. Does NOT wake anything — waking is a separate concern
/// with its own rate limiting, and a caller may legitimately want neither.
pub fn send(
    db: &Db,
    machine_id: &str,
    address: &str,
    message: &str,
    kind: Kind,
    from: Option<String>,
) -> Result<Sent> {
    // Before anything is written: a malformed address can never become valid,
    // and enqueueing one is how a message gets lost with no trace of failure.
    crate::validate_address(address)?;

    // A conversation address resolves to whichever segment it currently answers
    // on; a session id passes through unchanged, so every address printed before
    // conversations existed still works.
    let target = tp_reach::address_to_session(db.conn(), address)?;
    store(db, machine_id, &target, message, kind, from, None)
}

/// Answer a message, addressed from the original.
///
/// The stored return address is whatever the sender was called WHEN IT SENT. By
/// the time a reply is written it may name a segment that has compacted away, or
/// a conversation that must be resolved to its current segment — both of which
/// parked answers in the field, on the one path the `/tp` skill mandates.
pub fn reply(
    db: &Db,
    machine_id: &str,
    message_id: &str,
    message: &str,
    from: Option<String>,
) -> Result<Sent> {
    let original = tp_reach::get_by_prefix(db.conn(), message_id)?;
    let Some(to) = original.from_session.clone() else {
        anyhow::bail!(
            "message {} carries no return address, so it cannot be replied to \
             (it was sent by a caller that didn't identify itself — see `tp ask --from-session`). \
             To reach that machine anyway, find a live session with `tp live` and use `tp ask`.",
            &original.id[..8]
        );
    };
    let target = tp_reach::address_to_session(db.conn(), &to)?;
    store(
        db,
        machine_id,
        &target,
        message,
        Kind::Reply,
        from,
        Some(original.id),
    )
}

/// The half `send` and `reply` share once each has decided where it is going.
fn store(
    db: &Db,
    machine_id: &str,
    target: &str,
    message: &str,
    kind: Kind,
    from: Option<String>,
    reply_to: Option<String>,
) -> Result<Sent> {
    let msg = tp_reach::enqueue(
        db.conn(),
        target,
        from.as_deref(),
        machine_id,
        kind.as_str(),
        message,
        reply_to.as_deref(),
    )?;
    Ok(Sent {
        message_id: msg.id,
        addressability: tp_db::reach::addressability(db.conn(), target)?,
        target: target.to_string(),
        from,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db
    }

    /// The behaviour both surfaces depend on, tested where it lives rather than
    /// by spawning a binary — which is the point of this crate existing.
    #[test]
    fn a_send_reports_what_it_did_without_deciding_what_it_means() {
        let db = db();
        let sent = send(
            &db,
            "m1",
            "m1/claude_code/nobody",
            "hi",
            Kind::Ask,
            Some("m1/claude_code/me".into()),
        )
        .unwrap();
        assert_eq!(sent.kind, Kind::Ask);
        assert_eq!(sent.addressability, Addressability::Unknown);
        assert!(
            !sent.answerable(),
            "a return address is not enough — something must be reading the mailbox"
        );
    }

    /// A reply follows the ORIGINAL's return address, and links the two.
    #[test]
    fn a_reply_is_addressed_from_the_message_it_answers() {
        let db = db();
        let first = send(
            &db,
            "m1",
            "m1/claude_code/them",
            "question",
            Kind::Ask,
            Some("m1/claude_code/me".into()),
        )
        .unwrap();
        let answer = reply(&db, "m1", &first.message_id, "answer", None).unwrap();
        assert_eq!(answer.kind, Kind::Reply);
        assert_eq!(
            answer.target, "m1/claude_code/me",
            "a reply goes back to whoever sent the original, never to a hand-written address"
        );
    }

    /// Refusing is the whole value: a message with no way home cannot be
    /// answered, and saying so beats storing an answer nobody receives.
    #[test]
    fn a_message_with_no_return_address_cannot_be_replied_to() {
        let db = db();
        let anon = send(&db, "m1", "m1/claude_code/them", "anon", Kind::Ask, None).unwrap();
        let err = reply(&db, "m1", &anon.message_id, "answer", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no return address"), "{err}");
    }
}
