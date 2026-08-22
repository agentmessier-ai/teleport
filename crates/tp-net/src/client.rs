//! The outgoing half of pairing (LLD §8.2). `server.rs` handles what arrives;
//! this is what `tp pair <addr>` sends.
//!
//! Pairing is deliberately two-sided and neither side trusts anything yet:
//!   1. `ping` — learn the peer's device_id, name and pubkey.
//!   2. verify `device_id == fingerprint(pubkey)`. A peer that fails this is
//!      lying about its identity, and there is nothing to compare out of band.
//!   3. record it locally as `pending_out` (NOT trusted).
//!   4. `send_pair_request` — hand them my pubkey so they can record me as
//!      `pending_in`.
//!
//! Both sides then show a fingerprint a human compares before approving. No
//! step here grants trust; only a local `approve` does.

use crate::identity::{fingerprint, Identity};
use crate::server::{hex_decode_pub, hex_encode, PairRequest, PingResponse};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use std::time::Duration;

pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(5);

fn client() -> Result<reqwest::Client> {
    // Self-signed, unpinned: TLS here is encrypt-only (see `tls` module doc).
    // Peer identity is established afterwards by the device_id/pubkey check
    // below, not by the certificate.
    reqwest::Client::builder()
        .timeout(CLIENT_TIMEOUT)
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(Into::into)
}

/// Identify a peer. Returns its stated identity *and* the pubkey, already
/// checked to hash to the device_id it claims. Unauthenticated — see
/// `server.rs`'s `auth_middleware`: `/v1/ping` is the bootstrap route.
pub async fn ping(addr: &str) -> Result<(PingResponse, VerifyingKey)> {
    let res = client()?
        .get(format!("https://{addr}/v1/ping"))
        .send()
        .await
        .with_context(|| format!("cannot reach {addr}"))?;
    if !res.status().is_success() {
        bail!("{addr} answered {} to /v1/ping", res.status());
    }
    let ping: PingResponse = res.json().await.context("malformed /v1/ping response")?;

    let Some(bytes) = hex_decode_pub(&ping.pubkey) else {
        bail!("{addr} sent a malformed pubkey");
    };
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{addr} sent a pubkey of the wrong length"))?;
    let vk = VerifyingKey::from_bytes(&arr)
        .with_context(|| format!("{addr} sent an invalid ed25519 pubkey"))?;

    // The same binding the server enforces on inbound requests, applied to
    // what we RECEIVE. Skipping it here would mean the fingerprint a human
    // compares says nothing about the key that signs later traffic.
    if ping.device_id != fingerprint(&vk) {
        bail!(
            "{addr} claims device id {} but its pubkey hashes to {} — refusing to pair",
            ping.device_id,
            fingerprint(&vk)
        );
    }
    Ok((ping, vk))
}

/// Introduce myself to a peer so it can record me as `pending_in`.
/// `my_port` is where I serve, so the peer can call back.
pub async fn send_pair_request(
    addr: &str,
    me: &Identity,
    my_name: &str,
    my_port: u16,
) -> Result<String> {
    let body = PairRequest {
        device_id: me.device_id.clone(),
        name: my_name.to_string(),
        pubkey: hex_encode(me.verifying.as_bytes()),
        port: my_port,
    };
    let res = client()?
        .post(format!("https://{addr}/v1/pair/request"))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("cannot reach {addr}"))?;
    let status = res.status();
    if !status.is_success() {
        // A bare status code is a dead end here: the operator is standing at a
        // terminal having just run `tp pair request`, and the three refusals
        // this endpoint can produce have three different fixes — one of which
        // belongs to a person at the OTHER machine.
        let hint = match status.as_u16() {
            400 => {
                " — this machine's name is too long, or holds characters that \
                    cannot be shown safely (`hostname` is where it comes from)"
            }
            413 => " — the request body was rejected as oversized",
            429 => " — too many attempts from this address; wait a minute",
            503 => {
                " — that machine's pending list is full. Its operator has to \
                    clear it with `tp pair reject <device-id>` before anyone new \
                    can pair"
            }
            _ => "",
        };
        bail!("{addr} rejected the pairing request: {status}{hint}");
    }
    let v: serde_json::Value = res.json().await.unwrap_or_default();
    Ok(v["status"].as_str().unwrap_or("unknown").to_string())
}
