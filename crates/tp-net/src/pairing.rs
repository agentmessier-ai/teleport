//! Pairing state machine (LLD §8.2, Syncthing's model).
//!
//! Device identity = ed25519 keypair fingerprint (the `device_id`). The
//! machine table is keyed by that device id. First contact must be approved
//! once by a human; afterwards peers are trusted and requests are signed.

use anyhow::{bail, Result};
use ed25519_dalek::VerifyingKey;
use rusqlite::{params, Connection};
use tp_db::query;

/// Every state a relationship can be in. There is no negative state: refusing
/// a peer DELETES the row, so "not trusted" is spelled "absent".
///
/// An earlier design kept `Rejected`/`Revoked` as tombstones that pinned a
/// device against re-requesting, on the theory that the record would stop a
/// human re-approving a device they had already thrown out. It was dropped
/// because the tombstone never carried the one field that would have made it
/// useful — WHEN, or why — so it cost two extra states and a second unlock
/// step (`forget`) while delivering none of the audit value that justified
/// them. SSH's `authorized_keys`, WireGuard's peer list and Bluetooth's
/// "Forget This Device" all take this shape: present or absent, one action to
/// remove. See docs/LLD.md §8.2 for the comparison that settled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStatus {
    PendingOut,
    PendingIn,
    Trusted,
}

#[derive(Debug, Clone)]
pub struct PairingResult {
    pub status: PairingStatus,
}

/// How many unapproved INCOMING requests may sit in the table at once.
///
/// This protects the approval step, not the disk — the rows are tiny. Safety
/// rests on a human reading `tp pair list` and comparing a fingerprint out of
/// band, and a list with thousands of entries denies that control as
/// effectively as taking the endpoint down, more quietly, and with the added
/// gift that a lookalike name sitting next to the real one makes a misclick
/// into a trust decision.
///
/// Reachable from an unauthenticated endpoint, and the key is
/// `fingerprint(pubkey)` — so filling it costs an attacker one ed25519 keygen
/// per row, which is microseconds.
pub const MAX_PENDING_IN: usize = 32;

/// The longest peer name this machine will store.
///
/// Names come from `hostname`, so this is several times any real one; it is a
/// bound on a hostile input, not a style rule.
pub const MAX_NAME_CHARS: usize = 64;

/// What `record_incoming` did.
///
/// `ListFull` is a value rather than an `Err` because the caller acts on it
/// differently: a full list is a 503 whose fix is `tp pair reject`, a database
/// failure is a 500 whose fix is nothing the caller can do. Merged behind one
/// error type, "someone is flooding you" would read as "your disk is broken".
#[derive(Debug, Clone)]
pub enum Incoming {
    Recorded(PairingResult),
    /// `MAX_PENDING_IN` is reached and this device is not already in the list.
    ListFull,
}

/// Whether a peer's self-declared name is safe to put in front of a human.
///
/// A name reaches this machine from two unauthenticated places — the body of
/// `/v1/pair/request` and the `/v1/ping` response that `tp pair request`
/// stores — and has exactly one use: being printed beside a fingerprint in
/// `tp pair list`, at the moment someone is deciding whether to trust that
/// fingerprint. So it must not be able to lie about its own SHAPE. An ANSI
/// escape repaints the line it sits on, a newline forges an entire extra row,
/// and a bidi override displays characters in an order they are not stored in
/// (the Trojan Source class).
///
/// REJECTED rather than sanitised. Stripping would map two distinct names onto
/// one display string, and telling two peers apart by eye is the whole job
/// this text has.
///
/// What it does NOT do: a name is self-declared and may simply be false, which
/// is why the device id beneath it is the thing actually compared. Homoglyphs
/// are untouched and cannot be handled here.
pub fn name_is_displayable(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(|c| {
            // `is_control` is category Cc. The rest are Cf: zero-width, so
            // they reorder a line without occupying any of it.
            c.is_control()
                || matches!(c,
                    '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

fn trust_to_status(trust: &str) -> PairingStatus {
    match trust {
        "pending_out" => PairingStatus::PendingOut,
        "pending_in" => PairingStatus::PendingIn,
        "trusted" => PairingStatus::Trusted,
        _ => PairingStatus::PendingOut,
    }
}

fn upsert_peer(
    conn: &Connection,
    device_id: &str,
    name: &str,
    pubkey: &VerifyingKey,
    trust: &str,
    addr: Option<&str>,
) -> Result<PairingResult> {
    // Held here, not in the callers, for the reason `delete_peer` gives below:
    // it protects the WRITE. Both paths into this function carry a name from
    // an unauthenticated stranger — `record_incoming` from the request body,
    // `request_out` from the `/v1/ping` reply of whatever answered the address
    // a user typed — and a future third caller must not be able to add itself
    // without the guard. `pair_request` checks separately so the wire answer
    // is a 400 rather than a 500; this is what makes skipping that check a
    // failed write instead of a stored escape sequence.
    if !name_is_displayable(name) {
        bail!(
            "{device_id} sent an unusable name ({} chars): a peer name must be \
             1-{MAX_NAME_CHARS} characters and free of control or text-direction \
             characters",
            name.chars().count()
        );
    }
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO machine(id, name, pubkey, trust, addr, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name,
             pubkey = excluded.pubkey,
             -- keep a known address if this path has none to offer
             addr = COALESCE(excluded.addr, machine.addr),
             trust = excluded.trust
         -- An established relationship is INERT to this path, and the WHERE
         -- is what makes that atomic rather than a check-then-write race.
         --
         -- `record_incoming` is reached from `/v1/pair/request`, which is
         -- unauthenticated BY NECESSITY: demanding a trusted signature to make
         -- first contact would deadlock pairing forever. The endpoint checks
         -- device_id == fingerprint(pubkey), but BOTH are public — a peer
         -- hands them out on its own /v1/ping and advertises over mDNS — so
         -- that check proves the key is real, never that the caller owns it.
         -- Without this clause any stranger on the LAN could replay a trusted
         -- peer's identity to knock it back to `pending_in` (cutting it off),
         -- repoint the `addr` fan-out sends signed queries to at itself, and
         -- rewrite the name a human reads when deciding. Proven by
         -- `a_pair_request_cannot_downgrade_or_repoint_an_already_trusted_peer`.
         WHERE machine.trust != 'trusted'",
        params![
            device_id,
            name,
            pubkey.as_bytes().to_vec(),
            trust,
            addr,
            now
        ],
    )?;
    let row = query::machine(conn, device_id)?.expect("just inserted");
    Ok(PairingResult {
        status: trust_to_status(&row.trust),
    })
}

/// I initiated a pairing with `device_id`. Records `pending_out`.
///
/// `addr` is where I reached them — this is the ONLY path that learns a
/// peer's address from an outgoing action, and without storing it here a
/// trusted peer would have no address to fan out to later.
pub fn request_out(
    conn: &Connection,
    device_id: &str,
    name: &str,
    pubkey: &VerifyingKey,
    addr: Option<&str>,
) -> Result<PairingResult> {
    upsert_peer(conn, device_id, name, pubkey, "pending_out", addr)
}

/// I received a pairing request from `device_id`. Records `pending_in`.
///
/// `addr` is the socket the request arrived from, so an approved peer is
/// immediately reachable without waiting for an mDNS round.
pub fn record_incoming(
    conn: &Connection,
    device_id: &str,
    name: &str,
    pubkey: &VerifyingKey,
    addr: Option<&str>,
) -> Result<Incoming> {
    // Only a device with no row at all can be refused. One already in the list
    // must still be able to update — a peer retrying after a restart re-sends
    // the same fingerprint, and locking it out of its own row would turn a
    // full table into a permanent one. An already-trusted device is inert here
    // anyway (see the WHERE clause in `upsert_peer`), so it is never refused
    // for a list it is not in.
    if query::machine(conn, device_id)?.is_none() && pending_in_count(conn)? >= MAX_PENDING_IN {
        return Ok(Incoming::ListFull);
    }
    upsert_peer(conn, device_id, name, pubkey, "pending_in", addr).map(Incoming::Recorded)
}

/// Only `pending_in` counts against the cap. `pending_out` rows exist because
/// THIS operator ran `tp pair request`, so counting them would let one's own
/// outgoing attempts eat the budget for incoming ones — a stranger's flood
/// would not be limited any harder, and the operator would be.
fn pending_in_count(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM machine WHERE trust = 'pending_in'",
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

/// Approve a pending pairing (must be pending_out or pending_in first).
pub fn approve(conn: &Connection, device_id: &str) -> Result<PairingResult> {
    let row = query::machine(conn, device_id)?;
    let Some(row) = row else {
        bail!("no pending pairing for {device_id}");
    };
    if row.trust != "pending_out" && row.trust != "pending_in" {
        bail!(
            "no pending pairing for {device_id} (current: {})",
            row.trust
        );
    }
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "UPDATE machine SET trust = 'trusted', paired_at = ?1 WHERE id = ?2",
        params![now, device_id],
    )?;
    Ok(PairingResult {
        status: PairingStatus::Trusted,
    })
}

/// Refuse a pairing that was never approved. Deletes the row: "not trusted"
/// is spelled "absent" (see `PairingStatus`), so the device is free to ask
/// again later and a human is free to say yes then.
///
/// NOT for a peer that IS trusted — that is `revoke`. The two do exactly the
/// same thing to the database, so this is not about the outcome; it is about
/// catching the mistake at the moment of action. "Refuse a stranger's
/// request" and "throw out a machine I trusted" deserve different words, and
/// each guard makes using the wrong one an error instead of a surprise.
pub fn reject(conn: &Connection, device_id: &str) -> Result<()> {
    let Some(row) = query::machine(conn, device_id)? else {
        // `approve` errors on an unknown device; this used to succeed at
        // rejecting nothing, so a typo'd id reported a decision that never
        // happened. Same state machine, same guard.
        bail!("no pairing request from {device_id} to reject");
    };
    if row.trust == "trusted" {
        bail!("{device_id} is trusted — use `tp pair revoke` to take that back, not reject");
    }
    delete_peer(conn, device_id, &row.trust)
}

/// Take back trust from a peer that currently has it. Deletes the row.
///
/// Purely local, and effective on the peer's very next request: nothing is
/// cached, so `lookup_trusted_pubkey` simply finds no key to verify against
/// (see `revoking_a_trusted_peer_cuts_it_off_on_its_very_next_request` in
/// `tp-net/tests/fanout_test.rs`). There is no network route to tell the peer
/// and there will not be one — a refusal on its next request is the guarantee
/// this machine can actually keep, and a notification is not.
///
/// The peer's own database still says it trusts US. That asymmetry is
/// deliberate: 401 is trivially forgeable by anyone on the path, so treating
/// one as "they revoked me" would hand a network attacker the power to tear
/// down trust relationships it cannot otherwise touch.
pub fn revoke(conn: &Connection, device_id: &str) -> Result<()> {
    let Some(row) = query::machine(conn, device_id)? else {
        bail!("no relationship with {device_id} to revoke");
    };
    if row.trust != "trusted" {
        bail!(
            "{device_id} is not trusted (current: {}) — nothing to revoke; \
             use `tp pair reject` for a pending request",
            row.trust
        );
    }
    delete_peer(conn, device_id, &row.trust)
}

/// The shared removal, with the one guard neither caller may skip.
///
/// `self` lives in this same table and this same column, so a device id that
/// happens to be our own would otherwise delete this machine's identity row
/// out from under the daemon. Held here rather than in each caller because
/// it protects the DELETE, and a third caller must not be able to add itself
/// without it.
fn delete_peer(conn: &Connection, device_id: &str, trust: &str) -> Result<()> {
    if trust == "self" {
        bail!("{device_id} is this machine");
    }
    conn.execute("DELETE FROM machine WHERE id = ?1", [device_id])?;
    Ok(())
}
