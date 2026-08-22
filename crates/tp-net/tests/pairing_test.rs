//! Pairing state machine (LLD §8.2): request → approve/refuse. Refusing
//! DELETES the row — there is no negative state — so "not trusted" is spelled
//! "absent". Tested with two in-memory DBs, no network.

use tp_db::Db;
use tp_net::pairing::{
    approve, name_is_displayable, record_incoming, reject, request_out, revoke, Incoming,
    PairingResult, PairingStatus, MAX_NAME_CHARS, MAX_PENDING_IN,
};
use tp_net::Identity;

fn setup() -> Db {
    Db::open_in_memory().unwrap()
}

/// Unwrap the ordinary outcome. A `ListFull` in a test that is not about the
/// cap should fail loudly here rather than confusingly three assertions later.
fn recorded(outcome: Incoming) -> PairingResult {
    match outcome {
        Incoming::Recorded(r) => r,
        Incoming::ListFull => panic!("the pending list filled up unexpectedly"),
    }
}

/// Fill the incoming list to `MAX_PENDING_IN` with distinct strangers, exactly
/// as an attacker would: one fresh keypair per row, because the table is keyed
/// on the fingerprint.
fn fill_pending(db: &Db) {
    for i in 0..MAX_PENDING_IN {
        let s = Identity::generate();
        let out = record_incoming(
            db.conn(),
            &s.device_id,
            &format!("flood-{i}"),
            &s.verifying,
            None,
        )
        .unwrap();
        assert!(matches!(out, Incoming::Recorded(_)), "row {i} of the cap");
    }
}

#[test]
fn full_pair_roundtrip() {
    let a_db = setup();
    let b_db = setup();

    let a = Identity::generate();
    let b = Identity::generate();

    // A initiates a pairing with B.
    let req = request_out(a_db.conn(), &b.device_id, "user-B", &b.verifying, None).unwrap();
    assert_eq!(req.status, PairingStatus::PendingOut);

    // B receives the request.
    let received =
        recorded(record_incoming(b_db.conn(), &a.device_id, "user-A", &a.verifying, None).unwrap());
    assert_eq!(received.status, PairingStatus::PendingIn);

    // Both approve.
    approve(b_db.conn(), &a.device_id).unwrap();
    let b_side = approve(a_db.conn(), &b.device_id).unwrap();
    assert_eq!(b_side.status, PairingStatus::Trusted);

    // Both sides now trust each other.
    let a_peers = tp_db::query::trusted_peers(a_db.conn()).unwrap();
    let b_peers = tp_db::query::trusted_peers(b_db.conn()).unwrap();
    assert_eq!(a_peers.len(), 1);
    assert_eq!(a_peers[0].id, b.device_id);
    assert!(
        a_peers[0].pubkey.is_some(),
        "trusted peer must have its pubkey stored"
    );
    assert_eq!(b_peers.len(), 1);
    assert_eq!(b_peers[0].id, a.device_id);
}

#[test]
fn reject_removes_the_row_entirely() {
    let a_db = setup();
    let b = Identity::generate();
    request_out(a_db.conn(), &b.device_id, "user-B", &b.verifying, None).unwrap();

    reject(a_db.conn(), &b.device_id).unwrap();

    assert!(tp_db::query::trusted_peers(a_db.conn()).unwrap().is_empty());
    assert!(
        tp_db::query::machine(a_db.conn(), &b.device_id)
            .unwrap()
            .is_none(),
        "refusing a peer leaves no row: 'not trusted' is spelled 'absent'"
    );
}

#[test]
fn revoke_removes_a_trusted_peer_from_the_trusted_set() {
    let a_db = setup();
    let b = Identity::generate();
    request_out(a_db.conn(), &b.device_id, "user-B", &b.verifying, None).unwrap();
    approve(a_db.conn(), &b.device_id).unwrap();
    assert_eq!(tp_db::query::trusted_peers(a_db.conn()).unwrap().len(), 1);

    revoke(a_db.conn(), &b.device_id).unwrap();

    assert!(tp_db::query::trusted_peers(a_db.conn()).unwrap().is_empty());
    assert!(tp_db::query::machine(a_db.conn(), &b.device_id)
        .unwrap()
        .is_none());
}

/// `reject` and `revoke` do the same thing to the database, so the guards are
/// the entire point of keeping two verbs: using the wrong one for what you
/// actually have must be an error, not a surprise.
#[test]
fn reject_refuses_a_trusted_peer_and_points_at_revoke() {
    let a_db = setup();
    let b = Identity::generate();
    request_out(a_db.conn(), &b.device_id, "user-B", &b.verifying, None).unwrap();
    approve(a_db.conn(), &b.device_id).unwrap();

    let err = reject(a_db.conn(), &b.device_id).unwrap_err().to_string();
    assert!(err.contains("revoke"), "{err}");
    assert_eq!(
        tp_db::query::trusted_peers(a_db.conn()).unwrap().len(),
        1,
        "a refused reject must not have changed anything"
    );
}

#[test]
fn revoke_refuses_a_merely_pending_peer() {
    let a_db = setup();
    let b = Identity::generate();
    request_out(a_db.conn(), &b.device_id, "user-B", &b.verifying, None).unwrap();

    let err = revoke(a_db.conn(), &b.device_id).unwrap_err().to_string();
    assert!(err.contains("not trusted"), "{err}");
    assert!(
        tp_db::query::machine(a_db.conn(), &b.device_id)
            .unwrap()
            .is_some(),
        "a refused revoke must not have deleted the row"
    );
}

/// `approve` errors on a device nobody asked about; `reject` used to succeed
/// at rejecting nothing, so a typo'd id reported a decision that never
/// happened. Same state machine, same guard.
#[test]
fn reject_refuses_an_unknown_device() {
    let a_db = setup();
    let err = reject(a_db.conn(), "never-heard-of-it")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no pairing request"), "{err}");
}

/// This machine's own row lives in the same table, in the same column, as the
/// peers. Neither removal verb may delete the daemon's identity out from
/// under it because a device id happened to match.
#[test]
fn neither_verb_will_delete_this_machine() {
    let a_db = setup();
    a_db.ensure_self_machine("me", "TestMac").unwrap();

    let err = reject(a_db.conn(), "me").unwrap_err().to_string();
    assert!(err.contains("this machine"), "{err}");
    let err = revoke(a_db.conn(), "me").unwrap_err().to_string();
    assert!(err.contains("not trusted"), "{err}");

    assert!(
        tp_db::query::machine(a_db.conn(), "me").unwrap().is_some(),
        "the self row must survive both"
    );
}

/// `/v1/pair/request` is unauthenticated BY NECESSITY (requiring a trusted
/// signature there would deadlock first contact), so anything it can reach must
/// survive a hostile caller. A trusted peer's `device_id` and `pubkey` are both
/// PUBLIC — its own `/v1/ping` hands them out, and it advertises over mDNS — so
/// the fingerprint check the endpoint performs proves only "this is a real key",
/// never "this is the key's owner". Anyone on the LAN can therefore replay a
/// trusted peer's identity into a pair request. That must be inert.
#[test]
fn a_pair_request_cannot_downgrade_or_repoint_an_already_trusted_peer() {
    let a_db = setup();
    let b = Identity::generate();
    request_out(
        a_db.conn(),
        &b.device_id,
        "user-B",
        &b.verifying,
        Some("10.0.0.5:47400"),
    )
    .unwrap();
    approve(a_db.conn(), &b.device_id).unwrap();

    // Attacker replays B's public identity from its own address.
    let res = record_incoming(
        a_db.conn(),
        &b.device_id,
        "attacker-chosen-name",
        &b.verifying,
        Some("10.0.0.99:1"),
    )
    .unwrap();

    assert_eq!(
        recorded(res).status,
        PairingStatus::Trusted,
        "an unauthenticated re-request must not knock a trusted peer back to pending"
    );
    let row = tp_db::query::machine(a_db.conn(), &b.device_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.trust, "trusted");
    assert_eq!(
        row.addr.as_deref(),
        Some("10.0.0.5:47400"),
        "the address fan-out sends signed queries to must not be repointable by a stranger"
    );
    assert_eq!(
        row.name, "user-B",
        "the name a human reads when deciding must not be rewritable by a stranger"
    );
}

#[test]
fn approve_unknown_peer_is_error() {
    let a_db = setup();
    let r = approve(a_db.conn(), "never-requested").unwrap_err();
    assert!(r.to_string().contains("no pending"), "unexpected: {r}");
}

/// A refused device is gone, so the next request from it is genuine first
/// contact rather than something that has to be un-pinned first. This is the
/// property the old `rejected`/`revoked` tombstones deliberately prevented,
/// and dropping them is what the collapse to present-or-absent bought.
#[test]
fn a_refused_device_can_ask_again_and_lands_back_in_pending() {
    let a_db = setup();
    let b = Identity::generate();
    record_incoming(a_db.conn(), &b.device_id, "user-B", &b.verifying, None).unwrap();
    reject(a_db.conn(), &b.device_id).unwrap();

    let fresh =
        recorded(record_incoming(a_db.conn(), &b.device_id, "user-B", &b.verifying, None).unwrap());
    assert_eq!(fresh.status, PairingStatus::PendingIn);

    // And it is still only PENDING — asking again never grants anything.
    assert!(tp_db::query::trusted_peers(a_db.conn()).unwrap().is_empty());
}

// ── The pending list is a bounded resource ───────────────────────────────────
// `/v1/pair/request` is unauthenticated by necessity, so anyone who can reach
// this machine can add a row. The rows are cheap; what is not cheap is the
// control they sit in front of. Approval means a human finds the right entry in
// `tp pair list` and compares a fingerprint out of band, and that is exactly
// what an arbitrarily long list denies.

#[test]
fn the_pending_list_stops_growing_at_the_cap() {
    let db = setup();
    fill_pending(&db);

    let one_more = Identity::generate();
    let out = record_incoming(
        db.conn(),
        &one_more.device_id,
        "late",
        &one_more.verifying,
        None,
    )
    .unwrap();
    assert!(
        matches!(out, Incoming::ListFull),
        "past the cap a NEW device must be refused, not stored"
    );
    assert!(
        tp_db::query::machine(db.conn(), &one_more.device_id)
            .unwrap()
            .is_none(),
        "a refused request must leave nothing behind"
    );
}

/// The other half of choosing refuse-new over evict-oldest: refusing must not
/// lock a peer out of a row it already holds. A peer retrying after a restart
/// re-sends the same fingerprint, and if a full table froze those rows too, a
/// flood would make the condition permanent instead of clearable.
#[test]
fn a_device_already_pending_can_still_update_when_the_list_is_full() {
    let db = setup();
    let early = Identity::generate();
    recorded(
        record_incoming(
            db.conn(),
            &early.device_id,
            "early",
            &early.verifying,
            Some("10.0.0.7:47400"),
        )
        .unwrap(),
    );
    for _ in 1..MAX_PENDING_IN {
        let s = Identity::generate();
        recorded(record_incoming(db.conn(), &s.device_id, "flood", &s.verifying, None).unwrap());
    }

    let again = record_incoming(
        db.conn(),
        &early.device_id,
        "early",
        &early.verifying,
        Some("10.0.0.7:47500"),
    )
    .unwrap();
    assert!(
        matches!(again, Incoming::Recorded(_)),
        "its own row must stay writable"
    );
    let row = tp_db::query::machine(db.conn(), &early.device_id)
        .unwrap()
        .unwrap();
    assert_eq!(row.addr.as_deref(), Some("10.0.0.7:47500"));
}

/// A trusted peer is inert on this path (the WHERE clause in `upsert_peer`), so
/// the cap must never be what turns it away — that would let a flood sever
/// established relationships, which is a far worse outcome than a full list.
#[test]
fn a_full_pending_list_does_not_refuse_an_already_trusted_peer() {
    let db = setup();
    let friend = Identity::generate();
    recorded(
        record_incoming(
            db.conn(),
            &friend.device_id,
            "friend",
            &friend.verifying,
            None,
        )
        .unwrap(),
    );
    approve(db.conn(), &friend.device_id).unwrap();
    fill_pending(&db);

    let out = record_incoming(
        db.conn(),
        &friend.device_id,
        "friend",
        &friend.verifying,
        None,
    )
    .unwrap();
    assert!(
        matches!(out, Incoming::Recorded(r) if r.status == PairingStatus::Trusted),
        "a trusted peer must not be refused because strangers filled the list"
    );
}

/// `pending_out` rows are created by THIS operator running `tp pair request`.
/// Counting them would mean one's own outgoing attempts shrink the budget for
/// incoming ones — no harder on the flood, and harder on the operator.
#[test]
fn our_own_outgoing_requests_do_not_count_against_the_incoming_cap() {
    let db = setup();
    for i in 0..MAX_PENDING_IN {
        let s = Identity::generate();
        request_out(
            db.conn(),
            &s.device_id,
            &format!("mine-{i}"),
            &s.verifying,
            None,
        )
        .unwrap();
    }
    let stranger = Identity::generate();
    let out = record_incoming(
        db.conn(),
        &stranger.device_id,
        "them",
        &stranger.verifying,
        None,
    )
    .unwrap();
    assert!(
        matches!(out, Incoming::Recorded(_)),
        "outgoing requests must not fill the incoming list"
    );
}

// ── The name is text a human decides from ────────────────────────────────────

#[test]
fn a_name_that_can_lie_about_its_own_shape_is_refused() {
    for bad in [
        "",                                  // nothing to identify
        "a\nmachine-b   trusted   deadbeef", // forges an entire extra row
        "a\u{1b}[2Kmachine-b",               // ANSI: repaints the line it is on
        "a\rmachine-b",                      // carriage return: same, cheaper
        "mac\u{202e}kcatta",                 // bidi override: displayed reversed
        "x\u{2066}y\u{2069}z",               // isolates: same class
    ] {
        assert!(
            !name_is_displayable(bad),
            "{bad:?} must not reach `tp pair list`"
        );
    }
    assert!(!name_is_displayable(&"a".repeat(MAX_NAME_CHARS + 1)));
    // …and ordinary names, including non-ASCII ones, must still pass: a machine
    // named in Chinese is not an attack.
    for good in ["studio-mac.local", "build-box", "büro-mac", "mac-⌘"] {
        assert!(name_is_displayable(good), "{good:?} is a legitimate name");
    }
}

/// The guard lives in `upsert_peer`, so it covers the path a user takes when
/// THEY initiate: `tp pair request <addr>` stores the name from the remote
/// `/v1/ping` reply, which is exactly as unauthenticated as the request body.
/// Checking only the inbound endpoint would leave this half open.
#[test]
fn a_hostile_name_is_refused_on_the_outgoing_path_too() {
    let db = setup();
    let them = Identity::generate();
    let err = request_out(
        db.conn(),
        &them.device_id,
        "friendly\u{1b}[31m-name",
        &them.verifying,
        Some("10.0.0.9:47400"),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("unusable name"),
        "unexpected: {err}"
    );
    assert!(
        tp_db::query::machine(db.conn(), &them.device_id)
            .unwrap()
            .is_none(),
        "the write must not have happened"
    );
}
