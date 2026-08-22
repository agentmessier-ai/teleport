//! The seam between the two things that were once separate identities:
//! `device_id` (crypto, tp-net) is ALSO the `machine_id` segment of every
//! `session.id` (addressing, tp-core). Nothing enforced that compatibility
//! before, because the two systems never met.

use tp_core::SessionId;
use tp_net::Identity;

/// `SessionId::parse` splits on `/`, so a device_id containing one would make
/// every session on this machine unaddressable — the id would parse with a
/// truncated machine segment and the rest bleeding into `runtime_id`.
#[test]
fn device_id_survives_session_id_roundtrip() {
    for _ in 0..64 {
        let id = Identity::generate();
        assert!(
            !id.device_id.contains('/'),
            "device_id must not contain the SessionId separator: {}",
            id.device_id
        );

        let sid = SessionId::new(&id.device_id, "claude_code", "abc-123");
        let parsed = SessionId::parse(&sid.to_string())
            .unwrap_or_else(|| panic!("session id must reparse: {sid}"));
        assert_eq!(
            parsed.machine_id, id.device_id,
            "machine segment must survive intact"
        );
        assert_eq!(parsed.runtime_id, "claude_code");
        assert_eq!(parsed.native_id, "abc-123");
    }
}

/// The id is base32 + `-` grouping. Pinning the alphabet keeps a future change
/// to `fingerprint()` from silently introducing a character that breaks
/// addressing (`/`), shell quoting, or URL paths.
#[test]
fn device_id_alphabet_is_url_and_path_safe() {
    let id = Identity::generate();
    for c in id.device_id.chars() {
        assert!(
            c.is_ascii_uppercase() || ('2'..='7').contains(&c) || c == '-',
            "unexpected char {c:?} in device_id {}",
            id.device_id
        );
    }
}

/// Same key → same id, across a reload. This is what makes the id usable as a
/// persistent address: session rows written today must still resolve tomorrow.
#[test]
fn device_id_is_stable_across_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key");
    let first = Identity::load_or_create(&path).unwrap().device_id;
    let second = Identity::load_or_create(&path).unwrap().device_id;
    assert_eq!(
        first, second,
        "the session-id prefix must not change on restart"
    );
}
