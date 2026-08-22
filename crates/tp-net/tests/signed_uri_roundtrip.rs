use tp_db::Db;
use tp_net::peer::{query_peers, PeerAddr};
use tp_net::Identity;

fn empty_retrieval() -> tp_search::Retrieval {
    tp_search::Retrieval::new(Box::new(tp_search::ScanProvider::new(
        "m",
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![("claude_code".to_string(), "/nonexistent-probe-root".into())],
    )))
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

/// The server rebuilds path_and_query from the parsed URI; the client signed a
/// string it built itself. A non-ASCII term is where those could diverge.
#[tokio::test(flavor = "multi_thread")]
async fn non_ascii_query_verifies_end_to_end_over_the_network() {
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
    let addr = tp_net::server::serve(
        tp_net::default_state(db, host.clone(), empty_retrieval()),
        "0.0.0.0:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let Some(lan_ip) = non_loopback_ipv4() else {
        return;
    };

    let peers = vec![PeerAddr {
        device_id: host.device_id.clone(),
        name: "host".into(),
        addr: format!("{lan_ip}:{}", addr.port()),
        pubkey: Some(host.verifying.to_bytes().to_vec()),
    }];
    // Non-ASCII + a space: multi-byte percent-encoding on the signed path.
    let fan = query_peers(&caller, &peers, "naïve € search", 3_600_000, 10)
        .await
        .unwrap();
    assert!(
        fan.failed.is_empty(),
        "non-ASCII query must not break signature verification: {:?}",
        fan.failed
    );
    assert_eq!(fan.answered.len(), 1);
}
