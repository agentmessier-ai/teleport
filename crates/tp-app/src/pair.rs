//! Pairing: introduce, list, decide.
//!
//! Both surfaces implemented the introduction separately, and it is the one
//! operation in teleport where the ORDER of the steps is the security property.
//! `ping` verifies that the peer's `device_id` really is the fingerprint of the
//! key it answers with, and it has to happen before anything is stored;
//! self-check has to happen before the local row is written; the local row has
//! to exist before we announce ourselves, or a peer can be pending on their side
//! and unknown on ours. Two copies of a four-step sequence is two chances to get
//! the order wrong, in the code path that decides which machines may read this
//! one's transcripts.
//!
//! Nothing here trusts anybody. `request` leaves both sides *pending*; only a
//! person comparing device ids out of band and running `decide` on EACH machine
//! makes a peer trusted.

use anyhow::{bail, Result};
use tp_db::query::MachineRow;
use tp_db::Db;
use tp_net::pairing::PairingStatus;
use tp_net::Identity;

/// Who asked whom. Stored as a `trust` string; a caller should not be reading
/// prefixes off it to find out, which is what both surfaces were doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// They introduced themselves to us. Approving means letting them read.
    TheyAskedUs,
    /// We introduced ourselves to them.
    WeAskedThem,
}

/// A pairing that has not been decided yet.
#[derive(Debug, Clone)]
pub struct Pending {
    pub device_id: String,
    pub name: String,
    pub direction: Direction,
}

/// The pairing table, split by what a caller can do about each part.
///
/// Two buckets, because there are only two states: a refused peer has no row
/// at all (see `tp_net::pairing::PairingStatus`).
#[derive(Debug, Default)]
pub struct Pairings {
    /// Awaiting a decision on this machine.
    pub pending: Vec<Pending>,
    /// Already trusted.
    pub trusted: Vec<MachineRow>,
}

/// What an introduction achieved.
#[derive(Debug)]
pub struct Requested {
    pub device_id: String,
    pub name: String,
    /// What THIS machine now records. Not always `PendingOut`: an existing
    /// trusted peer is left alone, and a caller that assumes otherwise tells
    /// the user to approve something that has nothing to approve.
    pub my_status: PairingStatus,
    /// What the far side said it recorded. Reported verbatim rather than
    /// interpreted: their side can legitimately answer "already pending".
    pub their_status: String,
}

/// Pending pairings and trusted peers.
pub fn pairings(db: &Db) -> Result<Pairings> {
    let rows = tp_db::query::all_peers(db.conn())?;
    let mut out = Pairings::default();
    for p in rows {
        match p.trust.as_str() {
            "pending_in" => out.pending.push(Pending {
                device_id: p.id,
                name: p.name,
                direction: Direction::TheyAskedUs,
            }),
            "pending_out" => out.pending.push(Pending {
                device_id: p.id,
                name: p.name,
                direction: Direction::WeAskedThem,
            }),
            "trusted" => out.trusted.push(p),
            _ => {}
        }
    }
    Ok(out)
}

/// Introduce this machine to a peer, in the order that makes the check mean
/// something.
///
/// `me`, `my_name` and `my_port` are passed in rather than read from the
/// process, so this stays a function of its arguments and the surfaces keep
/// owning what they already know about the host.
pub async fn request(
    db: &Db,
    me: &Identity,
    addr: &str,
    my_name: &str,
    my_port: u16,
) -> Result<Requested> {
    // 1. Who is there? `ping` also checks device_id == fingerprint(pubkey), so
    //    a peer lying about its identity fails before anything is stored.
    let (them, their_key) = tp_net::client::ping(addr).await?;

    // 2. Before the write, not after: pairing with yourself creates a row that
    //    makes this machine its own peer.
    if them.device_id == me.device_id {
        bail!("that address is this machine");
    }

    // 3. Record locally as pending_out — NOT trusted. The status is REPORTED,
    //    not discarded: `request_out` leaves an already-trusted row alone, so
    //    throwing this away is how a caller ends up being told "nothing is
    //    trusted yet, now run approve" about a peer that is already trusted —
    //    advice whose only possible outcome is `no pending pairing`.
    let mine = tp_net::pairing::request_out(
        db.conn(),
        &them.device_id,
        &them.name,
        &their_key,
        Some(addr),
    )?;

    // 4. Only now announce ourselves. If this fails, we are pending on our side
    //    and unknown on theirs, which a retry fixes; the reverse would leave
    //    them holding a request we have no record of.
    let their_status = tp_net::client::send_pair_request(addr, me, my_name, my_port).await?;

    Ok(Requested {
        device_id: them.device_id,
        name: them.name,
        my_status: mine.status,
        their_status,
    })
}

/// Approve or refuse a PENDING pairing. Not for a `trusted` row — `reject`
/// refuses one and points at `revoke`.
///
/// `None` back means the pairing is gone rather than moved: refusing deletes
/// the row, so there is no status left to report and a caller must not be
/// handed one that implies otherwise.
pub fn decide(db: &Db, device_id: &str, accept: bool) -> Result<Option<PairingStatus>> {
    if accept {
        Ok(Some(tp_net::pairing::approve(db.conn(), device_id)?.status))
    } else {
        tp_net::pairing::reject(db.conn(), device_id)?;
        Ok(None)
    }
}

/// Take back trust from a peer that currently has it, removing it entirely.
pub fn revoke(db: &Db, device_id: &str) -> Result<()> {
    tp_net::pairing::revoke(db.conn(), device_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("me", "TestMac").unwrap();
        db
    }

    fn machine(db: &Db, id: &str, trust: &str) {
        db.conn()
            .execute(
                "INSERT INTO machine(id, name, trust, created_at) VALUES (?1, ?2, ?3, unixepoch())",
                [id, &format!("host-{id}"), trust],
            )
            .unwrap();
    }

    /// The direction is a fact about who may be waiting on whom, and both
    /// surfaces were deriving it by matching on a `trust` string prefix.
    #[test]
    fn direction_says_who_asked() {
        let db = db();
        machine(&db, "a", "pending_in");
        machine(&db, "b", "pending_out");
        let p = pairings(&db).unwrap();
        let dirs: Vec<_> = p
            .pending
            .iter()
            .map(|x| (&*x.device_id, x.direction))
            .collect();
        assert!(dirs.contains(&("a", Direction::TheyAskedUs)));
        assert!(dirs.contains(&("b", Direction::WeAskedThem)));
    }

    /// A refused peer leaves the list entirely — there is no third bucket for
    /// it to sit in, because there is no state left to describe.
    #[test]
    fn a_refused_peer_leaves_the_list() {
        let db = db();
        machine(&db, "nope", "pending_in");
        assert_eq!(decide(&db, "nope", false).unwrap(), None);

        let p = pairings(&db).unwrap();
        assert!(p.pending.is_empty(), "{:?}", p.pending);
        assert!(p.trusted.is_empty(), "{:?}", p.trusted);
    }

    /// The tp-app-layer proof that the `tp-net` guards are actually wired
    /// through, not merely present one layer down.
    #[test]
    fn revoking_removes_a_trusted_peer() {
        let db = db();
        machine(&db, "friend", "pending_in");
        decide(&db, "friend", true).unwrap();
        revoke(&db, "friend").unwrap();

        assert!(pairings(&db).unwrap().trusted.is_empty());
    }

    #[test]
    fn revoking_a_peer_that_was_never_trusted_is_refused() {
        let db = db();
        machine(&db, "stranger", "pending_in");

        let err = revoke(&db, "stranger").unwrap_err().to_string();
        assert!(err.contains("not trusted"), "{err}");
        assert_eq!(pairings(&db).unwrap().pending.len(), 1);
    }

    #[test]
    fn approving_moves_a_pending_peer_to_trusted() {
        let db = db();
        machine(&db, "friend", "pending_in");
        assert_eq!(
            decide(&db, "friend", true).unwrap(),
            Some(PairingStatus::Trusted)
        );

        let p = pairings(&db).unwrap();
        assert!(p.pending.is_empty());
        assert_eq!(p.trusted.len(), 1);
        assert_eq!(p.trusted[0].id, "friend");
    }

    /// Approving something nobody asked for must fail rather than invent a
    /// trusted peer: `approve` is the only step that grants read access, and a
    /// mistyped id must not become one.
    #[test]
    fn approving_an_unknown_machine_is_refused() {
        let db = db();
        let err = decide(&db, "never-heard-of-it", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no pending pairing"), "{err}");
    }

    /// This machine is in the machine table too, as `self`. It is not a peer.
    #[test]
    fn our_own_row_is_not_a_pairing() {
        let db = db();
        let p = pairings(&db).unwrap();
        assert!(p.pending.is_empty());
        assert!(p.trusted.is_empty());
    }
}
