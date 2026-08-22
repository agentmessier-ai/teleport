//! HTTP server (LLD §9). Trust tiers:
//! - loopback (127.0.0.1 / ::1) is trusted — the local CLI.
//! - anything else must present a valid RFC 9421 HTTP Message Signature from a
//!   trusted peer (see `auth.rs`, verified by `keyid` against that peer's
//!   stored pubkey); writes additionally require a single-use challenge bound
//!   into the signature's `nonce` parameter.
//!
//! The signing key never crosses the wire; only the signature itself, over a
//! fixed set of components including a `created` timestamp, so a captured
//! request is replayable only within the ±5 min window (reads) or not at all
//! (writes, challenge-bound).

use crate::auth::{verify_request, ChallengeStore};
use crate::identity::{hostname, Identity};
use crate::pairing::{self, Incoming};
use crate::ratelimit::RateLimiter;
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::routing::{get, post};
use axum::{Json, Router};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tp_core::retrieval::{Query, Scope};
use tp_db::{query, Db};
use tp_search::Retrieval;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<Db>>,
    pub identity: Arc<Identity>,
    pub challenges: Arc<ChallengeStore>,
    pub retrieval: Arc<Retrieval>,
    /// Guards `/v1/pair/request`, the one route that answers strangers and
    /// then takes `db` — the same mutex every signed request needs.
    pub pair_limiter: Arc<RateLimiter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResponse {
    pub device_id: String,
    pub name: String,
    pub version: String,
    /// Hex ed25519 public key. The initiator needs this to record who it is
    /// pairing with; it is public by definition, and the caller must check it
    /// hashes to `device_id` before storing either.
    pub pubkey: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub device_id: String,
    pub name: String,
    pub pubkey: String, // hex
    /// The port the requester SERVES on. The TCP source port is ephemeral and
    /// useless for calling back, so the peer states its listen port while the
    /// IP is taken from the observed connection (which it cannot forge on an
    /// established TCP session).
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PairRespond {
    pub device_id: String,
    pub accept: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub session_id: String,
    pub ts: Option<i64>,
    pub excerpt: String,
    pub role: String,
    /// `default` on both: a peer running an older build sends neither, and the
    /// honest reading of its silence is "not a subagent" / "surface unknown" —
    /// never "current".
    #[serde(default)]
    pub sidechain: bool,
    #[serde(default)]
    pub surface: tp_core::turn::Surface,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub machine_id: String,
    pub hits: Vec<SearchHit>,
    pub degraded: Option<String>,
}

/// Routes that mutate peer state.
///
/// EMPTY today, and deliberately still here. `/v1/pair/respond` used to be the
/// one entry, and it had no legitimate caller in the entire tree: `tp pair
/// approve` calls `pairing::approve` against the database directly, and only
/// tests ever spoke to the endpoint. What it did have was the ability to make
/// an attacker-controlled machine permanently `trusted` for anyone who could
/// open a socket to 127.0.0.1 — so it is gone rather than guarded.
///
/// The predicate stays because the ORDER around it is the actual control: a
/// write route is refused before the loopback exemption is consulted. Adding
/// one back means adding it here, which is the moment to decide how a human —
/// not a socket — authorises it.
fn is_write_route(path: &str) -> bool {
    path.starts_with("/v1/pair/respond")
}

/// The auth middleware — the ONLY gate for non-loopback requests.
async fn auth_middleware(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let path = req.uri().path().to_string();
    // The SIGNED target includes the query string. `path` alone is only used
    // for route classification below — signing it would leave `?q=…&limit=…`
    // (the entire input of a GET) unauthenticated.
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| path.clone());
    // Unauthenticated by necessity, not by convenience:
    //   /v1/ping       — identity handshake, must work before any trust exists
    //   /v1/challenge  — a nonce is worthless without a key that can sign it
    //   /v1/pair/request — THE bootstrap. A peer introducing itself is by
    //     definition not yet trusted, so requiring a trusted-peer signature
    //     here would make pairing over the network impossible (it would 401
    //     forever, and only loopback could ever pair — a deadlock).
    //     Safety does not rest on this endpoint: it can only create a
    //     `pending_in` row, which does nothing until a human runs
    //     `tp pair approve` — a CLI command that writes to the database
    //     directly. There is no network path to approval at all, which is a
    //     stronger guarantee than the one this comment used to claim.
    //
    //     It previously said approval went "via the challenge-bound
    //     /v1/pair/respond". That endpoint is gone, and it was never
    //     challenge-bound: `challenges.consume` is only reachable from the
    //     signature path, so a comment asserting a control the code did not
    //     implement was the cover under which any local process could grant
    //     permanent remote trust. Kept as a note because the lesson is the
    //     comment, not the endpoint — a doc comment describing intent rather
    //     than behaviour is how the hole stayed invisible through review.
    if path == "/v1/ping" || path == "/v1/challenge" || path == "/v1/pair/request" {
        return Ok(next.run(req).await);
    }
    // Write routes are denied FIRST — before the loopback exemption, not after
    // it. The old order made "is this request local?" answer "may this request
    // mutate trust?", which is identity-by-network-location: any process on the
    // machine (a postinstall script, a third-party MCP server, one poisoned
    // transitive dependency in any project) inherited the operator's authority
    // by virtue of connecting over 127.0.0.1. Reported by the devops session
    // reading this through zero trust, and it was exactly right.
    //
    // There is no write route today (see `is_write_route`); this ordering is
    // what keeps the next one from re-opening the hole by existing.
    if is_write_route(&path) {
        return Err(StatusCode::FORBIDDEN);
    }

    // There is NO loopback exemption. There used to be, justified as granting
    // "READ access to the local CLI — which already has the database file open
    // anyway, so this concedes nothing it did not have".
    //
    // Both halves were false. `tp`, `tp mcp` and the panel all open SQLite
    // directly and have never made an HTTP request; the exemption guarded a
    // caller that does not exist. And "already has the file open" describes the
    // CLI, not the arbitrary local process the exemption actually admitted — a
    // sandboxed one may be denied `~/.teleport` and still reach loopback. dsh's
    // own `workspace-write` sandbox is exactly that shape (see
    // `integrations/dsh/index.ts`), so the exemption handed a process teleport's
    // entire transcript history through a door its filesystem sandbox had shut.
    //
    // Same lesson as `/v1/pair/respond`, one route down: presence on an
    // interface is not identity. Every route past this point requires a
    // signature from a trusted device, whatever address it arrives from.

    // Peer: require a valid RFC 9421 signature (see `auth.rs`) from a trusted
    // device. Buffer the body first — `verify_request` hashes it itself and
    // checks that against the `Content-Digest` header, so a captured signed
    // request can't be replayed with a substituted body.
    let headers = req.headers();
    let sig_input = headers
        .get(crate::auth::SIGNATURE_INPUT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let sig = headers
        .get(crate::auth::SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let digest = headers
        .get(crate::auth::CONTENT_DIGEST_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (Some(sig_input), Some(sig), Some(digest)) = (sig_input, sig, digest) else {
        // A peer still on the pre-RFC9421 build sends `x-tp-sig` instead of
        // `Signature`/`Signature-Input` — that's otherwise indistinguishable
        // from any other malformed/unsigned request, and "just a 401" sends
        // whoever's debugging this down the wrong path entirely. This is a
        // migration-window diagnostic, not a fallback: the legacy request is
        // still rejected either way.
        if headers.get("x-tp-sig").is_some() {
            tp_core::log_warn!("tp-net: rejected a request bearing legacy x-tp-sig headers from {addr} — that peer needs `tpd` reinstalled (RFC 9421 migration)");
        }
        return Err(StatusCode::UNAUTHORIZED);
    };
    let method = req.method().as_str().to_string();

    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;

    let verified = verify_request(
        &method,
        &path_and_query,
        &bytes,
        &sig_input,
        &sig,
        &digest,
        |kid| lookup_trusted_pubkey(&st, kid),
    );
    let Some(verified) = verified else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    // Writes bind a single-use challenge into the signature's `nonce`
    // parameter, so a valid signature implies the signer held that unspent
    // nonce. Order matters: the signature is verified FIRST (above), and only
    // now is the nonce consumed — consuming before verifying would let any
    // unauthenticated caller who observed a nonce burn it with a garbage
    // signature, denying the legitimate peer its pairing write. (Unreachable
    // today since `is_write_route` above already rejects every write for a
    // remote caller — kept as defense in depth if that ever changes.)
    if is_write_route(&path) {
        let Some(nonce) = &verified.nonce else {
            return Err(StatusCode::UNAUTHORIZED);
        };
        if !st.challenges.consume(nonce) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    let req = Request::from_parts(parts, Body::from(bytes));
    Ok(next.run(req).await)
}

/// Cap on a buffered request body (the auth middleware must read it to verify
/// the digest). Well above any legitimate pairing payload.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// Bodies on `/v1/pair/request`, which is not covered by `MAX_BODY_BYTES`.
const MAX_PAIR_BODY_BYTES: usize = 4 * 1024;

/// Resolve a claimed `keyid` (a `device_id`) to that ONE trusted peer's
/// pubkey — `None` for anyone not already trusted. This is the whole benefit
/// of RFC 9421's `keyid` over the old scheme's "try every trusted key": a
/// single lookup instead of a scan, and the caller's identity falls out for
/// free instead of being discarded.
fn lookup_trusted_pubkey(st: &AppState, device_id: &str) -> Option<VerifyingKey> {
    let db = st.db.lock().unwrap();
    let row = query::machine(db.conn(), device_id).ok().flatten()?;
    if row.trust != "trusted" {
        return None;
    }
    let bytes: [u8; 32] = row.pubkey?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn ping(State(st): State<AppState>) -> Json<PingResponse> {
    Json(PingResponse {
        device_id: st.identity.device_id.clone(),
        name: hostname(),
        // The build, not just the semver: this is what lets a human compare
        // two machines and see that one is running week-old code. `/v1/ping`
        // is unauthenticated, so this does tell a LAN stranger the exact
        // build — accepted deliberately, because diagnosing "why does fan-out
        // to that peer fail" is the whole reason the field exists, and the
        // endpoint already discloses device_id, hostname and public key.
        version: tp_core::VERSION_LINE.to_string(),
        pubkey: hex_encode(st.identity.verifying.as_bytes()),
    })
}

async fn pair_request(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<PairRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Before the ed25519 work below, which is the expensive part of answering
    // a stranger. Loopback is NOT exempt: this is a resource decision, not a
    // trust one, and five requests a minute is more than the local CLI ever
    // makes — one per `tp pair request`, and that goes to the REMOTE peer.
    // An exemption here would be a special case earning nothing.
    if !st.pair_limiter.allow(peer.ip()) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    // A name that can repaint the operator's terminal is a 400, not a 500:
    // `upsert_peer` refuses it too, but only as an unskippable backstop, and
    // an `Err` there would tell the caller this machine had a fault when the
    // fault is in what it sent.
    if !pairing::name_is_displayable(&body.name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let Some(bytes) = hex_decode(&body.pubkey) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let arr: [u8; 32] = bytes.try_into().map_err(|_| StatusCode::BAD_REQUEST)?;
    let pubkey = VerifyingKey::from_bytes(&arr).map_err(|_| StatusCode::BAD_REQUEST)?;
    // The claimed device_id MUST be the pubkey's own fingerprint. Without this,
    // an attacker can register an arbitrary (device_id, pubkey) pair — picking
    // a device_id that collides with (or spoofs) a real machine while
    // supplying a key they control — which defeats out-of-band fingerprint
    // comparison during human approval (A3).
    if body.device_id != crate::identity::fingerprint(&pubkey) {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Observed IP + stated listen port. Recording it here is what makes an
    // approved peer immediately reachable instead of waiting for mDNS.
    let addr = format!("{}:{}", peer.ip(), body.port);
    let db = st.db.lock().unwrap();
    let res =
        pairing::record_incoming(db.conn(), &body.device_id, &body.name, &pubkey, Some(&addr))
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    drop(db);
    match res {
        Incoming::Recorded(res) => Ok(Json(
            serde_json::json!({ "status": format!("{:?}", res.status) }),
        )),
        Incoming::ListFull => {
            // The operator has to be told, because the fix is theirs and
            // nothing else will surface it: to the peer this is a 503, and to
            // this machine it is a request that silently never appeared in
            // `tp pair list`. Which is precisely the outcome an attacker
            // filling the list is going for.
            tp_core::log_warn!(
                "tp-net: refused a pairing request from {addr} — {} pending requests already, \
                 the cap. Clear them with `tp pair reject <device-id>`; until then no new peer \
                 can pair with this machine.",
                pairing::MAX_PENDING_IN
            );
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

async fn challenge(State(st): State<AppState>) -> Json<serde_json::Value> {
    let nonce = st.challenges.issue();
    Json(serde_json::json!({ "challenge": nonce }))
}

async fn search(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<SearchResponse>, StatusCode> {
    let q_text = params.get("q").cloned().unwrap_or_default();
    let since_ms: u64 = params
        .get("since_ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(6 * 3600 * 1000);
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let query = Query {
        text: q_text.clone(),
        regex: false,
        include_thinking: false,
        limit,
    };
    let scope = Scope {
        folder: None,
        since: Duration::from_millis(since_ms),
        runtimes: vec![],
        until: None,
    };
    // `Retrieval::search` is a synchronous walk of the whole transcript corpus
    // — walkdir plus read_to_string, seconds of it — so running it inline
    // parked a tokio worker for the duration. tpd already states this rule for
    // itself (bin/tpd.rs: the watcher and the discovery scan get plain threads
    // "because [their] work is blocking filesystem + SQLite, which would stall
    // a tokio worker"); the two HTTP handlers that do the same kind of work
    // were the ones that did not get it.
    //
    // Measured against the live daemon before this change: one /v1/search took
    // 10.6s; with 16 concurrent searches /v1/ping — the endpoint `tp pair` uses
    // to find a machine at all, with a 5s client timeout — returned nothing
    // within 20s. An aborted request made it worse rather than better: the task
    // is stuck inside sync code, so it cannot be cancelled on disconnect and
    // keeps burning a worker after the peer has already given up and retried.
    let retrieval = st.retrieval.clone();
    let (q2, s2) = (query.clone(), scope.clone());
    let got = tokio::task::spawn_blocking(move || retrieval.search(&q2, &s2))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let hits = got
        .items
        .into_iter()
        .map(|h| {
            let excerpt = h.excerpt().to_string();
            SearchHit {
                session_id: h.at.session_id,
                ts: h.at.ts,
                excerpt,
                role: format!("{:?}", h.role).to_lowercase(),
                sidechain: h.sidechain,
                surface: h.surface,
            }
        })
        .collect();
    Ok(Json(SearchResponse {
        machine_id: st.identity.device_id.clone(),
        hits,
        degraded: got.coverage.degraded,
    }))
}

/// Known peers + trust state (drives `tp pair list`).
async fn machines(State(st): State<AppState>) -> Json<serde_json::Value> {
    let db = st.db.lock().unwrap();
    let peers = query::trusted_peers(db.conn()).unwrap_or_default();
    drop(db);
    Json(serde_json::json!({
        "machines": peers.iter().map(|p| {
            serde_json::json!({
                "id": p.id, "name": p.name, "trust": p.trust, "last_seen_at": p.last_seen_at
            })
        }).collect::<Vec<_>>()
    }))
}

/// MUST go through `Retrieval`, not `tp_db::query` directly: the funnel is what
/// scrubs session titles (user prompt text) before they leave the machine, and
/// it is what honours the configured scan/index strategy. Querying the DB here
/// would both bypass redaction and hardcode the index provider.
async fn sessions(
    State(st): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let since_ms: u64 = params
        .get("since_ms")
        .and_then(|s| s.parse().ok())
        .unwrap_or(7 * 24 * 3600 * 1000);
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let scope = Scope {
        folder: None,
        since: Duration::from_millis(since_ms),
        runtimes: vec![],
        until: None,
    };
    // Same reasoning as `search` above: `sessions` reads and parses every
    // candidate transcript, which is blocking work and does not belong on an
    // async worker.
    let retrieval = st.retrieval.clone();
    let s2 = scope.clone();
    let got = tokio::task::spawn_blocking(move || retrieval.sessions(&s2, limit))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "sessions": got.items.iter().map(|s| {
            serde_json::json!({
                "id": s.id,
                "cwd": s.cwd,
                "title": s.title,          // scrubbed by the Retrieval funnel
                "last_turn_at": s.last_turn_at,
                "turn_count": s.turn_count
            })
        }).collect::<Vec<_>>(),
        "degraded": got.coverage.degraded,
    })))
}

// ── Construction / serve ─────────────────────────────────────────────────────

/// Parse untrusted hex. MUST NOT panic: its input is the `pubkey` field of
/// `/v1/pair/request`, which is remote-controlled. Slicing `&s[i..i+2]` panics
/// on odd length (out of bounds) and on any multi-byte UTF-8 char straddling an
/// even offset (not a char boundary).
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn hex_decode_pub(s: &str) -> Option<Vec<u8>> {
    hex_decode(s)
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.is_ascii() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}

/// Sign every response this server sends.
///
/// A middleware rather than per-handler code, because "which responses are
/// signed" must not be a per-route decision anyone can forget. Requests were
/// signed from the start and responses were not — and the consumer of a fan-out
/// response is a coding agent's context (`mcp.rs`), so an unverified `hits`
/// array is a prompt-injection channel, not merely a wrong search result.
///
/// The signature carries the CALLER's `nonce` query parameter, which the
/// request signature already covered via `@query`. That binds this answer to
/// that question: a captured response cannot be replayed against a different
/// one.
async fn sign_response_middleware(
    State(st): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    // Read the nonce BEFORE the request is consumed. Absent is fine — an older
    // client does not send one, and the response is then signed without it;
    // such a client is not verifying either.
    let nonce = req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("nonce=").map(str::to_string))
    });

    let res = next.run(req).await;
    let (mut parts, body) = res.into_parts();
    let status = parts.status.as_u16();
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };

    if let Ok(signed) = crate::auth::sign_response(&st.identity, status, &bytes, nonce.as_deref()) {
        for (name, value) in [
            (crate::auth::SIGNATURE_INPUT_HEADER, signed.signature_input),
            (crate::auth::SIGNATURE_HEADER, signed.signature),
            (crate::auth::CONTENT_DIGEST_HEADER, signed.content_digest),
        ] {
            if let Ok(v) = axum::http::HeaderValue::from_str(&value) {
                parts.headers.insert(name, v);
            }
        }
    }
    Ok(axum::response::Response::from_parts(
        parts,
        Body::from(bytes),
    ))
}

pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/ping", get(ping))
        .route("/v1/challenge", get(challenge))
        .route(
            "/v1/pair/request",
            // The signature middleware caps bodies at MAX_BODY_BYTES, and this
            // route deliberately returns before reaching it — so its only
            // bound would otherwise be axum's 2 MB default, on the single
            // endpoint that reads a body from someone unauthenticated. A
            // well-formed PairRequest is under 500 bytes.
            post(pair_request).layer(DefaultBodyLimit::max(MAX_PAIR_BODY_BYTES)),
        )
        .route("/v1/search", get(search))
        .route("/v1/sessions", get(sessions))
        .route("/v1/machines", get(machines))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        // Outside the auth layer, so it also signs the 401s and 403s that layer
        // produces — a client must be able to tell "this peer refused me" from
        // "something in the middle refused for it".
        .layer(middleware::from_fn_with_state(
            state.clone(),
            sign_response_middleware,
        ))
        .with_state(state)
}

/// Bind and spawn the server over TLS (see `tls` module). Returns the actual
/// bound address.
pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<SocketAddr> {
    let app = build_router(state);
    let (cert_pem, key_pem) =
        crate::tls::self_signed_pem().map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let bound = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum_server::from_tcp_rustls(listener, config)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await;
    });
    Ok(bound)
}

/// Convenience for building the default local state (used by the CLI).
pub fn default_state(db: Db, identity: Identity, retrieval: Retrieval) -> AppState {
    AppState {
        db: Arc::new(Mutex::new(db)),
        identity: Arc::new(identity),
        challenges: Arc::new(ChallengeStore::new()),
        retrieval: Arc::new(retrieval),
        pair_limiter: Arc::new(RateLimiter::new()),
    }
}
