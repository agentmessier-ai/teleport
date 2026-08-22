//! Encrypt-only TLS for the peer channel.
//!
//! Previously the whole peer protocol ran as plaintext HTTP: session content
//! (queries, excerpts) was readable by anyone on the same LAN segment. This
//! closes that.
//!
//! Peer AUTHENTICITY is unaffected and unchanged: it comes entirely from the
//! ed25519 signature on each request, checked against the pinned trust store
//! (`auth::verify_signed`), exactly as before. The certificate here is a
//! fresh self-signed cert with no CA and no pinning of its own — pinning the
//! same identity a second time, via the TLS layer, would add real complexity
//! (storing/exchanging a second key per peer, reasoning about two trust
//! stores) without changing what an attacker can do: a captured or replayed
//! TLS session still can't produce a valid signature for a route the caller
//! isn't already trusted on. Its only job is to stop a passive listener from
//! reading traffic in flight.

use anyhow::{Context, Result};
use std::sync::Once;

/// rustls 0.23 needs one process-wide default `CryptoProvider` explicitly
/// selected (both `aws-lc-rs` and `ring` end up in the dependency graph via
/// axum-server/reqwest, so it can't infer one). Idempotent — safe to call
/// from every `serve()`, including the several per-process servers the test
/// suite spins up.
fn ensure_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// A fresh self-signed cert + key, PEM-encoded. Generated once per process
/// (see `server::serve`) — it doesn't need to be stable across restarts,
/// since nothing pins to it.
pub fn self_signed_pem() -> Result<(Vec<u8>, Vec<u8>)> {
    ensure_crypto_provider();
    let cert = rcgen::generate_simple_self_signed(vec!["teleport-peer".to_string()])
        .context("generate self-signed TLS cert")?;
    let cert_pem = cert.cert.pem().into_bytes();
    let key_pem = cert.key_pair.serialize_pem().into_bytes();
    Ok((cert_pem, key_pem))
}
