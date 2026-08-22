#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Exempt: test output goes to the test harness, which is the point of it.

//! Fan-out: query a real local server (with an actual indexed fixture) through
//! the peer client, and verify a dead peer is reported, never silently dropped.

use std::path::Path;
use tp_db::Db;
use tp_net::peer::{merge_hits, query_peers, PeerAddr};
use tp_net::server::{default_state, serve, AppState};
use tp_net::Identity;

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp(ms / 1000, ((ms % 1000) * 1_000_000) as u32)
        .unwrap()
        .to_rfc3339()
}

/// Build a scan retrieval rooted at a fixture dir containing one session.
fn retrieval_with_fixture(root: &std::path::Path) -> tp_search::Retrieval {
    // The scan provider reads from the fixture root.
    let _ = std::fs::create_dir_all(root.join("-Users-test-dev-demo"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let line = serde_json::json!({
        "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": rfc3339(now),
        "message": {"content": "the purple unicorn flies at dawn"}
    });
    std::fs::write(
        root.join("-Users-test-dev-demo")
            .join("ffffffff-1111-2222-3333-444444444444.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    tp_search::Retrieval::new(Box::new(tp_search::ScanProvider::new(
        "test-machine",
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![("claude_code".to_string(), root.to_path_buf())],
    )))
}

/// A peer state that TRUSTS `caller`, which is what a real one does before it
/// will answer anything.
///
/// Two tests here used to pass without this, because the request arrived from
/// loopback and `auth_middleware` waved it through — so they asserted signed
/// fan-out while exercising the bypass instead. Removing the exemption is what
/// surfaced that; the setup is the honest version of what they always meant.
fn make_state_trusting(identity: Identity, root: &Path, caller: &Identity) -> AppState {
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&identity.device_id, "TestMac")
        .unwrap();
    tp_net::pairing::request_out(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    default_state(db, identity, retrieval_with_fixture(root))
}

#[tokio::test(flavor = "multi_thread")]
async fn fan_out_returns_hits_from_live_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let peer_ident = Identity::generate();
    let client_id = Identity::generate();
    let addr = serve(
        make_state_trusting(peer_ident.clone(), &root, &client_id),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    let peers = vec![PeerAddr {
        device_id: peer_ident.device_id.clone(),
        name: "peer".into(),
        addr: format!("127.0.0.1:{}", addr.port()),
        pubkey: Some(peer_ident.verifying.to_bytes().to_vec()),
    }];
    let fan = query_peers(&client_id, &peers, "purple unicorn", 3_600_000, 10)
        .await
        .unwrap();
    assert!(
        fan.failed.is_empty(),
        "the live peer must answer, not fail: {:?}",
        fan.failed
    );
    assert_eq!(fan.answered.len(), 1);
    assert_eq!(fan.answered[0].1, 1, "one hit expected from the fixture");

    let (merged, degraded) = merge_hits(vec![], fan);
    assert!(degraded.is_none(), "no peers failed, so no degradation");
    assert_eq!(merged.len(), 1);
    assert_eq!(
        merged[0].0, peer_ident.device_id,
        "hit must be tagged with the peer machine"
    );
    assert!(merged[0]
        .1
        .excerpt
        .to_lowercase()
        .contains("purple unicorn"));
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_peer_is_reported_not_silently_dropped() {
    // A peer on an unused port: connect refused → must appear in `failed`.
    let peers = vec![PeerAddr {
        device_id: "dead-peer".into(),
        name: "dead".into(),
        addr: "127.0.0.1:1".into(), // port 1: nothing listens
        // Irrelevant here — the connection fails before any response exists —
        // but present so this test exercises the connect failure and not the
        // missing-key one.
        pubkey: Some([7u8; 32].to_vec()),
    }];
    let client_id = Identity::generate();
    let fan = query_peers(&client_id, &peers, "anything", 3_600_000, 10)
        .await
        .unwrap();
    assert!(fan.hits.is_empty());
    assert_eq!(fan.failed.len(), 1, "dead peer must be reported");
    assert_eq!(fan.failed[0].0, "dead-peer");
    assert!(
        !fan.failed[0].1.is_empty(),
        "the failure reason must be kept, not erased"
    );
    assert!(fan.answered.is_empty());

    // merge_hits must surface the failure as degraded, not hide it.
    let (_merged, degraded) = merge_hits(vec![], fan);
    assert!(
        degraded.is_some(),
        "partial answers must degrade explicitly"
    );
    assert!(degraded.as_deref().unwrap().contains("dead-peer"));
}

/// The signed path, end to end, over a NON-loopback address — the branch every
/// existing test skipped by talking to 127.0.0.1 (which the middleware trusts
/// unconditionally). This is what proves the crypto is actually wired:
/// an unsigned/untrusted client must be rejected, and a signed request from a
/// TRUSTED peer must be accepted.
#[tokio::test(flavor = "multi_thread")]
async fn signed_fanout_against_a_non_loopback_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let host = Identity::generate();
    let caller = Identity::generate();

    // Build the host's state and pre-trust the caller's key.
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    tp_net::pairing::record_incoming(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    let state = tp_net::default_state(db, host.clone(), retrieval_with_fixture(&root));

    let addr = serve(state, "0.0.0.0:0".parse().unwrap()).await.unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        eprintln!("no non-loopback IPv4; skipping");
        return;
    };
    let peers = vec![PeerAddr {
        device_id: host.device_id.clone(),
        name: "host".into(),
        addr: format!("{lan_ip}:{}", addr.port()),
        pubkey: Some(host.verifying.to_bytes().to_vec()),
    }];

    // A trusted, signing caller gets real results over the network.
    let fan = query_peers(&caller, &peers, "purple unicorn", 3_600_000, 10)
        .await
        .unwrap();
    assert!(
        fan.failed.is_empty(),
        "a signed request from a trusted peer must be accepted: {:?}",
        fan.failed
    );
    assert_eq!(fan.answered.len(), 1);
    assert_eq!(
        fan.answered[0].1, 1,
        "the fixture hit must come back over the signed path"
    );

    // A stranger signs with a key the host does not trust → rejected, and the
    // reason is preserved rather than erased.
    let stranger = Identity::generate();
    let fan = query_peers(&stranger, &peers, "purple unicorn", 3_600_000, 10)
        .await
        .unwrap();
    assert_eq!(fan.failed.len(), 1, "an untrusted signer must be rejected");
    assert!(
        fan.failed[0].1.contains("401"),
        "the 401 must be reported, got: {}",
        fan.failed[0].1
    );
    assert!(fan.hits.is_empty());
}

fn non_loopback_ipv4() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.255.255.255:1").ok()?;
    let ip = s.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip.to_string())
    }
}

/// Regression for two auth defects found in review (predating the RFC 9421
/// migration, still load-bearing after it):
///  1. the query string was excluded from what's signed, so a captured
///     signature could be replayed against ANY query;
///  2. `Content-Digest` was never compared to the actual body, leaving the
///     body entirely unauthenticated.
#[tokio::test(flavor = "multi_thread")]
async fn signature_covers_query_string_and_body() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let host = Identity::generate();
    let caller = Identity::generate();
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    tp_net::pairing::record_incoming(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    let addr = serve(
        tp_net::default_state(db, host.clone(), retrieval_with_fixture(&root)),
        "0.0.0.0:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        return;
    };
    let base = format!("https://{lan_ip}:{}", addr.port());
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let signed_pq = "/v1/search?q=unicorn&since_ms=3600000&limit=10";
    let signed = tp_net::sign_request(&caller, "GET", signed_pq, b"", None).unwrap();

    // The exact signed request is accepted.
    let ok = client
        .get(format!("{base}{signed_pq}"))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &signed.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &signed.signature)
        .header(tp_net::auth::CONTENT_DIGEST_HEADER, &signed.content_digest)
        .send()
        .await
        .unwrap();
    assert_eq!(
        ok.status(),
        200,
        "the exact signed request must be accepted"
    );

    // Same signature, DIFFERENT query → must be rejected (defect 1).
    let tampered = client
        .get(format!(
            "{base}/v1/search?q=password&since_ms=99999999999&limit=100000"
        ))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &signed.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &signed.signature)
        .header(tp_net::auth::CONTENT_DIGEST_HEADER, &signed.content_digest)
        .send()
        .await
        .unwrap();
    assert_eq!(
        tampered.status(),
        401,
        "a signature must not transfer to a different query"
    );

    // Content-Digest that does not match the actual body → rejected. Uses a
    // GET (with a synthetic non-empty body claimed via a mismatched digest)
    // instead of a write route: writes are now unconditionally blocked from
    // non-loopback callers before body verification even runs (see
    // `pair_respond_is_loopback_only` for that separate guard), so this must
    // exercise a route the body check can actually reach.
    let mismatched =
        tp_net::sign_request(&caller, "GET", signed_pq, b"not-the-real-empty-body", None).unwrap();
    let lying = client
        .get(format!("{base}{signed_pq}"))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &mismatched.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &mismatched.signature)
        .header(
            tp_net::auth::CONTENT_DIGEST_HEADER,
            &mismatched.content_digest,
        ) // digest of the WRONG body
        // actual request body sent is empty, so it won't match `mismatched.content_digest`
        .send()
        .await
        .unwrap();
    assert_eq!(
        lying.status(),
        401,
        "a body that does not match Content-Digest must be rejected"
    );
}

/// A2 regression: pairing approval must be a local (loopback) decision only.
/// Without this guard, `verify_signed` proving "signed by *some* trusted peer"
/// (not *which* one) would let any already-trusted peer remotely approve
/// arbitrary new peers into the trust store.
#[tokio::test(flavor = "multi_thread")]
async fn pair_respond_is_loopback_only() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let host = Identity::generate();
    let caller = Identity::generate();
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    // caller is trusted — proving even a legitimately trusted peer is denied.
    tp_net::pairing::record_incoming(
        db.conn(),
        &caller.device_id,
        "caller",
        &caller.verifying,
        None,
    )
    .unwrap();
    tp_net::pairing::approve(db.conn(), &caller.device_id).unwrap();
    let addr = serve(
        tp_net::default_state(db, host.clone(), retrieval_with_fixture(&root)),
        "0.0.0.0:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        return;
    };
    let base = format!("https://{lan_ip}:{}", addr.port());
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let body = r#"{"device_id":"someone-else","accept":true}"#;
    let signed = tp_net::sign_request(
        &caller,
        "POST",
        "/v1/pair/respond",
        body.as_bytes(),
        Some("any-nonce"),
    )
    .unwrap();

    let res = client
        .post(format!("{base}/v1/pair/respond"))
        .header(
            tp_net::auth::SIGNATURE_INPUT_HEADER,
            &signed.signature_input,
        )
        .header(tp_net::auth::SIGNATURE_HEADER, &signed.signature)
        .header(tp_net::auth::CONTENT_DIGEST_HEADER, &signed.content_digest)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        403,
        "a trusted peer's valid signature must still be rejected on this route"
    );
}

/// A peer whose response does not verify contributes NOTHING, and says so.
///
/// This is the constraint the whole feature rests on. If an unsigned or
/// mis-signed response were a soft failure whose hits still flowed through, an
/// attacker would simply never sign — the check would be decorative. There is
/// deliberately no flag to relax it: the flag would be the vulnerability.
///
/// The consumer of these hits is `mcp.rs`, which puts them into a coding
/// agent's context. An unverified hit is not a wrong search result, it is a
/// prompt-injection channel.
#[tokio::test(flavor = "multi_thread")]
async fn a_response_signed_by_the_wrong_key_is_dropped_not_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();
    let peer_ident = Identity::generate();
    // The caller must be TRUSTED, or the request is refused before a response
    // exists and this asserts a 401 instead of the response-signature check it
    // is written for.
    let client_id = Identity::generate();
    let addr = serve(
        make_state_trusting(peer_ident.clone(), &root, &client_id),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();

    // The peer really is who it says, and really does answer — but we hold the
    // WRONG key for it, which is indistinguishable from an impostor answering
    // in its place. That is the case that must fail closed.
    let impostor = Identity::generate();
    let peers = vec![PeerAddr {
        device_id: peer_ident.device_id.clone(),
        name: "peer".into(),
        addr: format!("127.0.0.1:{}", addr.port()),
        pubkey: Some(impostor.verifying.to_bytes().to_vec()),
    }];

    let fan = query_peers(&client_id, &peers, "purple unicorn", 3_600_000, 10)
        .await
        .unwrap();

    assert!(
        fan.hits.is_empty(),
        "hits from an unverifiable response must not reach the caller, got {:?}",
        fan.hits
    );
    assert_eq!(
        fan.failed.len(),
        1,
        "and the peer must be REPORTED as failed — silently returning zero hits \
         reads as 'nothing was discussed there', which is the false negative \
         this whole module is written to avoid"
    );
    assert!(
        fan.failed[0].1.contains("signature"),
        "the reason must name the actual cause, got {:?}",
        fan.failed[0].1
    );
}

/// A trusted peer with no stored public key is unverifiable, so it is dropped
/// the same way — before a request is even sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_peer_with_no_stored_key_is_reported_not_queried() {
    let peers = vec![PeerAddr {
        device_id: "keyless".into(),
        name: "keyless".into(),
        addr: "127.0.0.1:1".into(),
        pubkey: None,
    }];
    let client_id = Identity::generate();
    let fan = query_peers(&client_id, &peers, "anything", 3_600_000, 10)
        .await
        .unwrap();
    assert!(fan.hits.is_empty());
    assert_eq!(fan.failed.len(), 1);
    assert!(
        fan.failed[0].1.contains("public key"),
        "got {:?}",
        fan.failed[0].1
    );
}

/// The question a network can never answer on its own: once a peer is
/// revoked, can it still get in? Bytes always arrive — nothing on the wire
/// stops that — so the guarantee has to be that the server refuses to
/// authenticate them, checked FRESH on every single request rather than
/// cached from when trust was granted. This proves it end to end: a real
/// signed request, from the peer's real (once-trusted) key, over the real
/// network — accepted before `tp pair revoke`, refused after, with no
/// restart, no cache to invalidate, nothing on the peer's side changed at
/// all. The only thing that moved is one row's `trust` column.
#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_trusted_peer_cuts_it_off_on_its_very_next_request() {
    let host = Identity::generate();
    let peer = Identity::generate();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("fixture");

    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&host.device_id, "TestMac").unwrap();
    tp_net::pairing::request_out(db.conn(), &peer.device_id, "peer", &peer.verifying, None)
        .unwrap();
    tp_net::pairing::approve(db.conn(), &peer.device_id).unwrap();
    let state = default_state(db, host.clone(), retrieval_with_fixture(&root));

    let addr = serve(state.clone(), "0.0.0.0:0".parse().unwrap())
        .await
        .unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        eprintln!("no non-loopback IPv4; skipping");
        return;
    };
    let peers = vec![PeerAddr {
        device_id: host.device_id.clone(),
        name: "host".into(),
        addr: format!("{lan_ip}:{}", addr.port()),
        pubkey: Some(host.verifying.to_bytes().to_vec()),
    }];

    // Before: a trusted signer gets real results.
    let before = query_peers(&peer, &peers, "purple unicorn", 3_600_000, 10)
        .await
        .unwrap();
    assert!(
        before.failed.is_empty(),
        "must be accepted while still trusted: {:?}",
        before.failed
    );
    assert_eq!(before.answered.len(), 1);

    // The revoke itself: no HTTP call, no message to the peer, nothing the
    // peer's own machine can see happen. Purely a local write to the host's
    // own database.
    {
        let db = state.db.lock().unwrap();
        tp_net::pairing::revoke(db.conn(), &peer.device_id).unwrap();
        assert!(
            tp_db::query::machine(db.conn(), &peer.device_id)
                .unwrap()
                .is_none(),
            "revoke removes the relationship outright — there is no negative state"
        );
    }

    // After: the IDENTICAL signer, same key, same peer address, no restart —
    // refused. Nothing cached the old trust state; the next request re-reads
    // it and finds `trust != 'trusted'`, so `lookup_trusted_pubkey` returns
    // None and there is no key left to verify ANY signature against,
    // including a perfectly valid one.
    let after = query_peers(&peer, &peers, "purple unicorn", 3_600_000, 10)
        .await
        .unwrap();
    assert_eq!(
        after.failed.len(),
        1,
        "a revoked peer's signed request must now be refused"
    );
    assert!(
        after.failed[0].1.contains("401"),
        "got: {}",
        after.failed[0].1
    );
    assert!(after.hits.is_empty(), "no data may reach a revoked peer");
}
