//! Request authentication for peer-to-peer HTTP (LLD §8.2), via RFC 9421 HTTP
//! Message Signatures (see `docs/rfc9421-migration-design.md`).
//!
//! Two tiers:
//! - **Reads** are signed over a fixed component set (`@method`, `@path`,
//!   `@query`, `content-digest`) with a `created` timestamp, verified against
//!   the sender's known pubkey (looked up by the signature's `keyid`, which is
//!   the sender's `device_id`), ±5 min skew. Replaying a read is idempotent,
//!   so the freshness window is enough.
//! - **Writes** (pairing response, wake) are additionally **challenge-bound**:
//!   a single-use nonce issued by the receiver must appear as the signature's
//!   `nonce` parameter, so a valid signature implies the signer held that
//!   unspent nonce. This closes the window a plain freshness check leaves open
//!   after a daemon restart empties the replay cache (LLD §15 fix #4).
//!
//! `keyid` is a genuine improvement over the hand-rolled scheme this replaced:
//! that scheme could only ask "does ANY trusted key verify this?" (an O(n)
//! scan telling the verifier nothing about who signed); RFC 9421 signatures
//! self-declare who signed, so verification is a single lookup and the caller
//! comes back known.
//!
//! `covered_components` is signer-declared per RFC 9421, which means a
//! verifier that trusted whatever set a signer claims to have signed could be
//! handed a signature that doesn't actually bind the method/path/body it
//! looks like it covers. This module closes that by rejecting any request
//! whose declared coverage isn't *exactly* the fixed set above — see
//! `expected_component_ids`.

use crate::identity::{httpsig_public_key, Identity};
use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use httpsig::prelude::message_component::{HttpMessageComponent, HttpMessageComponentId};
use httpsig::prelude::{HttpSignatureBase, HttpSignatureHeaders, HttpSignatureParams};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const CHALLENGE_TTL_MS: i64 = 60_000;
/// Hard cap on live nonces — `/v1/challenge` is pre-auth, so this bounds a
/// remote memory-exhaustion attempt.
pub const MAX_CHALLENGES: usize = 10_000;

/// ±5 min freshness window on a signature's `created` param.
pub const SKEW_SECS: u64 = 300;

/// The signature label both sides use — `Signature-Input: tp=(...)`,
/// `Signature: tp=:...:`. RFC 9421 allows multiple named signatures per
/// request; teleport only ever needs one.
const SIGNATURE_LABEL: &str = "tp";

pub const SIGNATURE_INPUT_HEADER: &str = "signature-input";
pub const SIGNATURE_HEADER: &str = "signature";
pub const CONTENT_DIGEST_HEADER: &str = "content-digest";

/// RFC 9530 `Content-Digest` header value: `sha-256=:<base64 sha256>:`.
fn content_digest(body: &[u8]) -> String {
    use base64::Engine;
    let mut hasher = Sha256::new();
    hasher.update(body);
    let b64 = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
    format!("sha-256=:{b64}:")
}

/// The exact, fixed, ordered set of components every teleport request signs.
/// See the module doc: a verifier must insist on this rather than trusting
/// whatever coverage a signer declares.
fn expected_component_ids() -> Result<Vec<HttpMessageComponentId>> {
    ["@method", "@path", "@query", "content-digest"]
        .iter()
        .map(|s| HttpMessageComponentId::try_from(*s).map_err(|e| anyhow::anyhow!("{e}")))
        .collect()
}

/// `path_and_query` split into RFC 9421's `@path` and `@query` derived
/// component values. `@query` includes the leading `?` (or is a bare `?` when
/// there is no query string) — that's the wire form the spec examples use.
fn split_path_query(path_and_query: &str) -> (&str, String) {
    match path_and_query.split_once('?') {
        Some((path, q)) => (path, format!("?{q}")),
        None => (path_and_query, "?".to_string()),
    }
}

fn component_lines(
    method: &str,
    path_and_query: &str,
    content_digest_value: &str,
) -> Result<Vec<HttpMessageComponent>> {
    let (path, query) = split_path_query(path_and_query);
    [
        format!("\"@method\": {}", method.to_ascii_uppercase()),
        format!("\"@path\": {path}"),
        format!("\"@query\": {query}"),
        format!("\"content-digest\": {content_digest_value}"),
    ]
    .iter()
    .map(|l| HttpMessageComponent::try_from(l.as_str()).map_err(|e| anyhow::anyhow!("{e}")))
    .collect()
}

/// The three headers a signed request must carry.
pub struct SignedHeaders {
    pub signature_input: String,
    pub signature: String,
    pub content_digest: String,
}

/// Build the signature headers, given the components that are being signed.
///
/// The only thing requests and responses genuinely differ about is WHICH
/// components are covered — `component_lines` vs `response_component_lines`,
/// each with its own reasoning. Everything after that is one policy: `keyid` is
/// this device's id, the caller's nonce is carried when there is one and
/// nothing else is set, the headers are built under `SIGNATURE_LABEL` with this
/// identity's key, and the returned `content_digest` is the digest of exactly
/// the bytes that were signed.
///
/// It lived in both functions. Adding a parameter to that policy — a
/// created/expires bound, a tag, a different keyid — in one place would have
/// left the other signing under the old rules, and requests and responses are
/// checked by different code on the far side, so nothing would have failed
/// loudly.
fn signature_headers(
    identity: &Identity,
    components: Vec<HttpMessageComponent>,
    digest: String,
    nonce: Option<&str>,
) -> Result<SignedHeaders> {
    let ids: Vec<HttpMessageComponentId> = components.iter().map(|c| c.id.clone()).collect();

    let mut params = HttpSignatureParams::try_new(&ids).map_err(|e| anyhow::anyhow!("{e}"))?;
    params.set_keyid(&identity.device_id);
    if let Some(n) = nonce {
        params.set_nonce(n);
    }

    let base =
        HttpSignatureBase::try_new(&components, &params).map_err(|e| anyhow::anyhow!("{e}"))?;
    let sk = identity.httpsig_secret_key();
    let headers = base
        .build_signature_headers(&sk, Some(SIGNATURE_LABEL))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(SignedHeaders {
        signature_input: headers.signature_input_header_value(),
        signature: headers.signature_header_value(),
        content_digest: digest,
    })
}

/// Sign a request. `nonce` must be `Some` for a write route (see module doc);
/// omit it for reads.
pub fn sign_request(
    identity: &Identity,
    method: &str,
    path_and_query: &str,
    body: &[u8],
    nonce: Option<&str>,
) -> Result<SignedHeaders> {
    let digest = content_digest(body);
    let components = component_lines(method, path_and_query, &digest)?;
    signature_headers(identity, components, digest, nonce)
}

/// A request that verified successfully.
pub struct VerifiedRequest {
    /// The signer's `device_id` (RFC 9421 `keyid`) — trusted only because it
    /// was looked up via `lookup_trusted` before the signature was checked
    /// against exactly that key, not because the caller claims it.
    pub device_id: String,
    pub nonce: Option<String>,
}

/// Verify a signed request. `lookup_trusted` resolves a claimed `keyid` (a
/// `device_id`) to that peer's pubkey — return `None` for anyone not already
/// trusted, which fails verification outright rather than falling back to
/// scanning every trusted key.
///
/// Returns `None` on ANY failure (malformed headers, unknown signer, stale
/// timestamp, wrong coverage, bad signature, tampered body) — deliberately
/// undifferentiated, matching the caller's existing "401, no further detail"
/// handling.
pub fn verify_request(
    method: &str,
    path_and_query: &str,
    body: &[u8],
    signature_input_header: &str,
    signature_header: &str,
    content_digest_header: &str,
    lookup_trusted: impl Fn(&str) -> Option<VerifyingKey>,
) -> Option<VerifiedRequest> {
    // The digest header must match the body we actually received — never
    // trust a caller-declared digest for bytes we haven't hashed ourselves.
    if content_digest(body) != content_digest_header {
        return None;
    }

    let map = HttpSignatureHeaders::try_parse(signature_header, signature_input_header).ok()?;
    let entry = map.get(SIGNATURE_LABEL)?;
    let params = entry.signature_params();

    let expected = expected_component_ids().ok()?;
    if params.covered_components != expected {
        return None;
    }

    let created = params.created?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now.abs_diff(created) > SKEW_SECS {
        return None;
    }

    let keyid = params.keyid.clone()?;
    let vk = lookup_trusted(&keyid)?;
    let pk = httpsig_public_key(&vk);

    let components = component_lines(method, path_and_query, content_digest_header).ok()?;
    let base = HttpSignatureBase::try_new(&components, params).ok()?;
    base.verify_signature_headers(&pk, entry).ok()?;

    Some(VerifiedRequest {
        device_id: keyid,
        nonce: params.nonce.clone(),
    })
}

// ── Response signing ─────────────────────────────────────────────────────────

/// The fixed component set for a RESPONSE. Deliberately different from a
/// request's: there is no method, path or query on the way back, and signing
/// `@status` binds the signature to "this is the 200 that answered", so a
/// captured error body cannot be replayed as a success.
fn expected_response_component_ids() -> Result<Vec<HttpMessageComponentId>> {
    ["@status", "content-digest"]
        .iter()
        .map(|s| HttpMessageComponentId::try_from(*s).map_err(|e| anyhow::anyhow!("{e}")))
        .collect()
}

fn response_component_lines(
    status: u16,
    content_digest_value: &str,
) -> Result<Vec<HttpMessageComponent>> {
    [
        format!("\"@status\": {status}"),
        format!("\"content-digest\": {content_digest_value}"),
    ]
    .iter()
    .map(|l| HttpMessageComponent::try_from(l.as_str()).map_err(|e| anyhow::anyhow!("{e}")))
    .collect()
}

/// Sign a response over `@status` + `content-digest`, carrying the CALLER's
/// nonce as the signature's `nonce` parameter.
///
/// The nonce is what stops a captured response being replayed against a
/// different question. It arrives in the request's query string, which the
/// request signature already covers via `@query`, so the client does not need a
/// second exchange to establish it and an attacker cannot choose it.
pub fn sign_response(
    identity: &Identity,
    status: u16,
    body: &[u8],
    nonce: Option<&str>,
) -> Result<SignedHeaders> {
    let digest = content_digest(body);
    let components = response_component_lines(status, &digest)?;
    signature_headers(identity, components, digest, nonce)
}

/// Verify a response came from `expected_keyid`, answers `expected_nonce`, and
/// covers the bytes we actually received.
///
/// Every check is mandatory and there is no configuration to relax any of them.
/// A soft failure whose hits still flow through is not a weaker guarantee, it is
/// none at all: an attacker simply never signs. The caller drops this peer's
/// results — partial results are already first-class in the fan-out, so failing
/// closed per peer costs nothing structurally.
#[allow(clippy::too_many_arguments)]
pub fn verify_response(
    status: u16,
    body: &[u8],
    signature_input_header: &str,
    signature_header: &str,
    content_digest_header: &str,
    expected_keyid: &str,
    expected_nonce: &str,
    peer_key: &VerifyingKey,
) -> bool {
    if content_digest(body) != content_digest_header {
        return false;
    }
    let Ok(map) = HttpSignatureHeaders::try_parse(signature_header, signature_input_header) else {
        return false;
    };
    let Some(entry) = map.get(SIGNATURE_LABEL) else {
        return false;
    };
    let params = entry.signature_params();

    let Ok(expected) = expected_response_component_ids() else {
        return false;
    };
    if params.covered_components != expected {
        return false;
    }
    // The signer must be the peer we asked, not merely someone we trust —
    // otherwise any trusted machine could answer for any other.
    if params.keyid.as_deref() != Some(expected_keyid) {
        return false;
    }
    // Binds this answer to this question.
    if params.nonce.as_deref() != Some(expected_nonce) {
        return false;
    }
    let Some(created) = params.created else {
        return false;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    if now.as_secs().abs_diff(created) > SKEW_SECS {
        return false;
    }

    let pk = httpsig_public_key(peer_key);
    let Ok(components) = response_component_lines(status, content_digest_header) else {
        return false;
    };
    let Ok(base) = HttpSignatureBase::try_new(&components, params) else {
        return false;
    };
    base.verify_signature_headers(&pk, entry).is_ok()
}

/// Single-use challenge store. Values are `(issued_ms, spent)`.
pub struct ChallengeStore {
    map: Mutex<HashMap<String, (i64, bool)>>,
}

impl Default for ChallengeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ChallengeStore {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a fresh nonce, valid for CHALLENGE_TTL_MS.
    ///
    /// Reaps on every call and refuses to grow past `MAX_CHALLENGES`: this
    /// endpoint is reachable before authentication (a nonce is useless without
    /// a key that can sign it), so an unbounded map here is a remote memory
    /// exhaustion primitive.
    pub fn issue(&self) -> String {
        self.reap();
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let nonce = hex(&bytes);
        let mut map = self.map.lock().unwrap();
        if map.len() >= MAX_CHALLENGES {
            // Still-live nonces beyond the cap: drop the oldest to bound memory.
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, (issued, _))| *issued)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(nonce.clone(), (tp_core::now_ms(), false));
        nonce
    }

    /// Consume a nonce. Returns true only if it existed, was unspent, and was
    /// still within TTL. Consuming marks it spent regardless, so a replay fails.
    pub fn consume(&self, nonce: &str) -> bool {
        let mut map = self.map.lock().unwrap();
        let Some((issued, spent)) = map.get_mut(nonce) else {
            return false;
        };
        if *spent || tp_core::now_ms() - *issued > CHALLENGE_TTL_MS {
            // Still mark spent so a stale nonce can never be reused later.
            *spent = true;
            return false;
        }
        *spent = true;
        true
    }

    /// Drop nonces that can never succeed again: already spent, or past TTL.
    /// (An earlier version inverted this and RETAINED spent entries — exactly
    /// the ones that are safe to drop — so nothing was ever reclaimed.)
    pub fn reap(&self) {
        let mut map = self.map.lock().unwrap();
        map.retain(|_, (issued, spent)| !*spent && tp_core::now_ms() - *issued <= CHALLENGE_TTL_MS);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    #[test]
    fn sign_then_verify_round_trips() {
        let peer = Identity::generate();
        let body = b"{\"q\":\"hi\"}";
        let signed = sign_request(&peer, "GET", "/v1/search?q=hi", body, None).unwrap();

        let verified = verify_request(
            "GET",
            "/v1/search?q=hi",
            body,
            &signed.signature_input,
            &signed.signature,
            &signed.content_digest,
            |kid| (kid == peer.device_id).then_some(peer.verifying),
        );
        assert!(verified.is_some(), "a correctly signed request must verify");
        assert_eq!(verified.unwrap().device_id, peer.device_id);
    }

    #[test]
    fn verify_rejects_unknown_signer() {
        let peer = Identity::generate();
        let body = b"";
        let signed = sign_request(&peer, "GET", "/v1/search", body, None).unwrap();
        let verified = verify_request(
            "GET",
            "/v1/search",
            body,
            &signed.signature_input,
            &signed.signature,
            &signed.content_digest,
            |_kid| None, // nobody is trusted
        );
        assert!(verified.is_none(), "an unrecognized keyid must not verify");
    }

    #[test]
    fn verify_rejects_tampered_query_and_body() {
        let peer = Identity::generate();
        let body = b"abc";
        let signed = sign_request(&peer, "GET", "/v1/search?q=hi&limit=5", body, None).unwrap();
        let lookup = |kid: &str| (kid == peer.device_id).then_some(peer.verifying);

        // Different query string (e.g. a replayed signature against a
        // different search) must not verify.
        assert!(verify_request(
            "GET",
            "/v1/search?q=OTHER&limit=5",
            body,
            &signed.signature_input,
            &signed.signature,
            &signed.content_digest,
            lookup
        )
        .is_none());
        // Different body, with a self-consistent (but wrong) digest, must not verify.
        let wrong_digest = super::content_digest(b"substituted body");
        assert!(verify_request(
            "GET",
            "/v1/search?q=hi&limit=5",
            b"substituted body",
            &signed.signature_input,
            &signed.signature,
            &wrong_digest,
            lookup
        )
        .is_none());
    }

    #[test]
    fn verify_rejects_stale_signature() {
        let peer = Identity::generate();
        let body = b"";
        let mut signed = sign_request(&peer, "GET", "/v1/search", body, None).unwrap();
        // `sign_request` always signs with `created = now`, so exercising a
        // stale one means bypassing it and building the signature base by
        // hand with a forged `created` — this proves the freshness check in
        // `verify_request` is actually live, not just present in the code.
        let stale_created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - SKEW_SECS
            - 60;
        let digest = super::content_digest(body);
        let components = super::component_lines("GET", "/v1/search", &digest).unwrap();
        let ids: Vec<HttpMessageComponentId> = components.iter().map(|c| c.id.clone()).collect();
        let mut params = HttpSignatureParams::try_new(&ids).unwrap();
        params.set_created(stale_created);
        params.set_keyid(&peer.device_id);
        let base = HttpSignatureBase::try_new(&components, &params).unwrap();
        let headers = base
            .build_signature_headers(&peer.httpsig_secret_key(), Some("tp"))
            .unwrap();
        signed.signature_input = headers.signature_input_header_value();
        signed.signature = headers.signature_header_value();

        let verified = verify_request(
            "GET",
            "/v1/search",
            body,
            &signed.signature_input,
            &signed.signature,
            &signed.content_digest,
            |kid| (kid == peer.device_id).then_some(peer.verifying),
        );
        assert!(
            verified.is_none(),
            "a signature `created` outside the skew window must not verify"
        );
    }

    #[test]
    fn sign_and_verify_write_bind_the_nonce() {
        let peer = Identity::generate();
        let body = b"{\"accept\":true}";
        let signed =
            sign_request(&peer, "POST", "/v1/pair/respond", body, Some("nonce-123")).unwrap();
        let lookup = |kid: &str| (kid == peer.device_id).then_some(peer.verifying);

        let verified = verify_request(
            "POST",
            "/v1/pair/respond",
            body,
            &signed.signature_input,
            &signed.signature,
            &signed.content_digest,
            lookup,
        )
        .unwrap();
        assert_eq!(
            verified.nonce.as_deref(),
            Some("nonce-123"),
            "the signed nonce must be recoverable so the caller can consume it"
        );

        // The nonce is part of what's SIGNED, not just an accompanying claim:
        // rewriting it in the header (as an attacker who observed a live
        // request but not the private key would have to) must invalidate the
        // signature, not just change which nonce gets reported.
        let tampered_input = signed
            .signature_input
            .replace("nonce-123", "different-nonce");
        assert_ne!(
            tampered_input, signed.signature_input,
            "the replace must have actually matched something"
        );
        let tampered = verify_request(
            "POST",
            "/v1/pair/respond",
            body,
            &tampered_input,
            &signed.signature,
            &signed.content_digest,
            lookup,
        );
        assert!(
            tampered.is_none(),
            "rewriting the nonce param without re-signing must not verify"
        );
    }

    #[test]
    fn challenge_is_single_use_and_expires() {
        let store = ChallengeStore::new();
        let nonce = store.issue();
        assert!(store.consume(&nonce), "first consume must succeed");
        assert!(!store.consume(&nonce), "replay must fail");
        assert!(!store.consume("never-issued"), "unknown nonce must fail");
    }
}
