#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Exempt: test output goes to the test harness, which is the point of it.

//! Server integration: two local servers pair over HTTP and the trust state is
//! observable via `/v1/machines`. Both run on loopback, exercising the
//! local-trust path; the signature path is unit-tested separately (auth.rs).

use std::net::SocketAddr;
use tp_db::Db;
use tp_net::server::{default_state, serve, AppState};
use tp_net::Identity;

fn base_url(addr: SocketAddr) -> String {
    format!("https://127.0.0.1:{}", addr.port())
}

fn empty_retrieval() -> tp_search::Retrieval {
    tp_search::Retrieval::new(Box::new(tp_search::ScanProvider::new(
        "test-machine",
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![(
            "claude_code".to_string(),
            "/nonexistent-teleport-test-root".into(),
        )],
    )))
}

fn make_state(identity: Identity) -> AppState {
    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(&identity.device_id, "TestMac")
        .unwrap();
    default_state(db, identity, empty_retrieval())
}

fn hex_public(id: &Identity) -> String {
    id.verifying
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_flow_over_http() {
    let a = Identity::generate();
    let b = Identity::generate();

    let state_a = make_state(a.clone());
    let addr_a = serve(state_a.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr_b = serve(make_state(b.clone()), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url_a = base_url(addr_a);
    let url_b = base_url(addr_b);

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // Ping (unauthenticated handshake).
    let pa: serde_json::Value = client
        .get(format!("{url_a}/v1/ping"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pa["device_id"].as_str().unwrap(), a.device_id);

    // A → B: B records A as pending_in.
    let res = client
        .post(format!("{url_b}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": a.device_id, "name": "machine-a", "pubkey": hex_public(&a), "port": 47400
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "pair request must succeed");

    // B → A: A records B as pending_in (mirror side of the handshake).
    client
        .post(format!("{url_a}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": b.device_id, "name": "machine-b", "pubkey": hex_public(&b), "port": 47400
        }))
        .send()
        .await
        .unwrap();

    // A approves B — a WRITE, but this whole test runs over loopback, which
    // the middleware trusts unconditionally before any signature/challenge
    // check runs (see `write_without_challenge_is_rejected_on_loopback_via_middleware`
    // just below, which documents that explicitly). `/v1/challenge` still
    // works pre-auth either way.
    let _ch: serde_json::Value = client
        .get(format!("{url_a}/v1/challenge"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Approval happens the way it actually happens: `tp pair approve` calls
    // `pairing::approve` against the database. There is no HTTP route for it,
    // and this test used to be the reason it looked like there had to be.
    {
        let db = state_a.db.lock().unwrap();
        tp_net::pairing::approve(db.conn(), &b.device_id).unwrap();
    }

    // A now lists B as trusted. Asserted against the DATABASE, which is what
    // this test means — it used to read `/v1/machines` unsigned over loopback,
    // which only worked because of an exemption that has since been removed
    // (see `auth_middleware`). The route was propping up the exemption and the
    // exemption was propping up the route; the fact being checked was always
    // the row.
    {
        let db = state_a.db.lock().unwrap();
        let trusted = tp_db::query::trusted_peers(db.conn()).unwrap();
        assert_eq!(trusted.len(), 1, "A must trust exactly B");
        assert_eq!(trusted[0].id, b.device_id);
    }
}

/// Loopback must NOT be able to mutate trust.
///
/// This test previously asserted the opposite, and its comment said so out
/// loud: "loopback is TRUSTED, so this write succeeds without a challenge."
/// That made the vulnerability the specification — any local process (a
/// postinstall script, a third-party MCP server, one poisoned transitive
/// dependency) could POST accept:true and make an attacker-controlled machine
/// permanently trusted, with no human and no signature. A green suite reported
/// it as correct for as long as it existed.
///
/// The route is gone now, so the assertion is that it is gone; the ORDER that
/// makes its return impossible — write routes refused before the loopback
/// exemption is consulted — is what this is really guarding.
#[tokio::test(flavor = "multi_thread")]
async fn loopback_cannot_mutate_trust() {
    let a = Identity::generate();
    let b = Identity::generate();
    let state_a = make_state(a.clone());
    let addr_a = serve(state_a.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url_a = base_url(addr_a);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    client
        .post(format!("{url_a}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": b.device_id, "name": "machine-b", "pubkey": hex_public(&b), "port": 47400
        }))
        .send()
        .await
        .unwrap();
    let res = client
        .post(format!("{url_a}/v1/pair/respond"))
        .json(&serde_json::json!({ "device_id": b.device_id, "accept": true }))
        .send()
        .await
        .unwrap();
    assert!(
        res.status() != 200,
        "a local process must not be able to grant permanent remote trust; got {}",
        res.status()
    );

    // And it did not take effect by some other path either.
    {
        let db = state_a.db.lock().unwrap();
        assert!(
            tp_db::query::trusted_peers(db.conn()).unwrap().is_empty(),
            "no machine may become trusted without a human running `tp pair approve`"
        );
    }
}

/// Regression: `/v1/pair/request` MUST be reachable by an untrusted peer over
/// the network. It is the only way a new device introduces itself; requiring a
/// trusted-peer signature there deadlocks pairing (401 forever, loopback-only).
/// Verified against a non-loopback bind so the loopback fast-path can't mask it.
#[tokio::test(flavor = "multi_thread")]
async fn pair_request_is_reachable_from_a_non_loopback_address() {
    let host = Identity::generate();
    let stranger = Identity::generate();

    // Bind on all interfaces so we can reach it via the LAN IP (non-loopback).
    let state = make_state(host.clone());
    let addr = serve(state.clone(), "0.0.0.0:0".parse().unwrap())
        .await
        .unwrap();
    let Some(lan_ip) = local_ipv4() else {
        eprintln!("no non-loopback IPv4 on this host; skipping");
        return;
    };
    let url = format!("https://{lan_ip}:{}", addr.port());
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    // A protected route from the same non-loopback address must be rejected —
    // this proves the request really is arriving as non-loopback.
    let protected = client
        .get(format!("{url}/v1/sessions"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        protected.status(),
        401,
        "non-loopback reads must require a signature"
    );

    // …but the bootstrap endpoint must still accept the introduction.
    let res = client
        .post(format!("{url}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": stranger.device_id, "name": "stranger", "pubkey": hex_public(&stranger), "port": 47400
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        200,
        "an untrusted peer must be able to introduce itself"
    );
    let json: serde_json::Value = res.json().await.unwrap();
    assert_eq!(json["status"].as_str().unwrap(), "PendingIn");

    // Crucially, it is only PENDING — it must NOT be trusted without approval.
    {
        let db = state.db.lock().unwrap();
        assert!(
            tp_db::query::trusted_peers(db.conn()).unwrap().is_empty(),
            "a self-introduced peer must NOT be trusted until a human approves it"
        );
    }
}

fn local_ipv4() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.255.255.255:1").ok()?;
    let ip = s.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip.to_string())
    }
}

/// Loopback must not read either.
///
/// The exemption this asserts the absence of was justified as serving "the
/// local CLI" — but `tp`, `tp mcp` and the panel all open SQLite directly and
/// have never made an HTTP request, so it guarded a caller that does not
/// exist. What it actually admitted was any local process, including one whose
/// filesystem sandbox denies `~/.teleport` while leaving loopback open (dsh's
/// `workspace-write` profile is exactly that). Transcripts are the whole
/// content of this database; an unauthenticated local read of them is not a
/// smaller hole than an unauthenticated write, only a quieter one.
#[tokio::test(flavor = "multi_thread")]
async fn loopback_cannot_read_without_a_signature() {
    let host = Identity::generate();
    let addr = serve(make_state(host), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url = base_url(addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    for route in ["/v1/sessions", "/v1/machines", "/v1/search?q=x"] {
        let res = client.get(format!("{url}{route}")).send().await.unwrap();
        assert_eq!(
            res.status(),
            401,
            "{route} must require a signature even from loopback"
        );
    }

    // The bootstrap routes stay open, or pairing deadlocks — they are exempted
    // ahead of this check on purpose, and that must not regress either.
    let res = client.get(format!("{url}/v1/ping")).send().await.unwrap();
    assert_eq!(res.status(), 200, "/v1/ping is the pairing bootstrap");
}

/// A name that can repaint the operator's terminal is the caller's fault, so
/// it must come back 400 — not the 500 that `upsert_peer`'s backstop would
/// produce if the handler let it through.
#[tokio::test(flavor = "multi_thread")]
async fn a_pair_request_with_a_terminal_escape_in_its_name_is_refused() {
    let host = Identity::generate();
    let stranger = Identity::generate();
    let state = make_state(host);
    let addr = serve(state.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url = base_url(addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client
        .post(format!("{url}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": stranger.device_id,
            "name": "innocent\u{1b}[2K\rmachine-b   trusted",
            "pubkey": hex_public(&stranger),
            "port": 47400
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        400,
        "the name is the caller's fault, not ours"
    );

    let db = state.db.lock().unwrap();
    assert!(
        tp_db::query::machine(db.conn(), &stranger.device_id)
            .unwrap()
            .is_none(),
        "nothing may be stored for a refused name"
    );
}

/// Answering a stranger costs an ed25519 decompression and then `st.db` — the
/// same mutex `lookup_trusted_pubkey` takes for every SIGNED request. Without a
/// limit, the endpoint that exists so strangers can introduce themselves is a
/// lever on the traffic of peers already trusted.
///
/// Note what this test can and cannot reach: every request here arrives from
/// 127.0.0.1, so the per-address bucket empties long before `MAX_PENDING_IN`
/// rows could accumulate. The two limits compose rather than overlap, and the
/// cap is exercised at the `pairing` level instead (`pairing_test.rs`).
#[tokio::test(flavor = "multi_thread")]
async fn a_flood_of_pair_requests_from_one_address_is_cut_off() {
    let host = Identity::generate();
    let state = make_state(host);
    let addr = serve(state.clone(), "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let url = base_url(addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let mut refused = 0;
    let mut accepted = 0;
    // Each one is a distinct keypair, because the table is keyed on the
    // fingerprint — repeating a single identity would be an upsert and would
    // prove nothing about growth.
    for _ in 0..20 {
        let s = Identity::generate();
        let res = client
            .post(format!("{url}/v1/pair/request"))
            .json(&serde_json::json!({
                "device_id": s.device_id, "name": "flood", "pubkey": hex_public(&s), "port": 47400
            }))
            .send()
            .await
            .unwrap();
        match res.status().as_u16() {
            200 => accepted += 1,
            429 => refused += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert!(refused > 0, "a burst of 20 must not be served in full");
    let db = state.db.lock().unwrap();
    let stored: usize = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM machine WHERE trust = 'pending_in'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap() as usize;
    assert_eq!(
        stored, accepted,
        "every accepted request should be exactly one row, and no refused one"
    );
}

/// The body cap on this route is easy to delete by accident, because the
/// signature middleware's `MAX_BODY_BYTES` looks like it already covers
/// everything — and this is the one route that returns before reaching it.
#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_pair_request_body_is_refused_before_it_is_parsed() {
    let host = Identity::generate();
    let stranger = Identity::generate();
    let state = make_state(host);
    let addr = serve(state, "127.0.0.1:0".parse().unwrap()).await.unwrap();
    let url = base_url(addr);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let res = client
        .post(format!("{url}/v1/pair/request"))
        .json(&serde_json::json!({
            "device_id": stranger.device_id,
            "name": "x".repeat(1024 * 1024),
            "pubkey": hex_public(&stranger),
            "port": 47400
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        413,
        "a megabyte from an unauthenticated caller must not be buffered"
    );
}
