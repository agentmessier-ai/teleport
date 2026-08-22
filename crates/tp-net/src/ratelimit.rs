//! Per-address token bucket for the endpoints that must answer strangers.
//!
//! `/v1/pair/request` is unauthenticated BY NECESSITY (see `server.rs`), which
//! means anyone who can open a TLS connection can make this machine decompress
//! an ed25519 point and then take `st.db` — the same mutex
//! `lookup_trusted_pubkey` needs on the hot path of every *signed* request.
//! Left unbounded, the endpoint that exists so strangers can introduce
//! themselves becomes a lever on the traffic of peers already trusted.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

/// Requests one address may make back to back.
///
/// Pairing is a human-driven act: the client sends exactly one request per
/// `tp pair request`, so five is already several retries' worth. The number is
/// small on purpose — a limit set high enough to never inconvenience anyone is
/// a limit that never stops anyone either.
const CAPACITY: f64 = 5.0;

/// One token back every 12 s, i.e. the burst above refills over a minute.
const REFILL_PER_MS: f64 = CAPACITY / 60_000.0;

/// Ceiling on the map itself. The bucket table is allocated from unauthenticated
/// input, so without this the rate limiter would be the memory-exhaustion
/// primitive it was added to prevent — the same lesson `MAX_CHALLENGES` records
/// one module over.
const MAX_BUCKETS: usize = 4096;

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_ms: i64,
}

pub struct RateLimiter {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Spend a token for `ip`. `false` means refuse the request.
    ///
    /// Keyed on the IP rather than the socket: the source port is fresh for
    /// every connection, so a per-socket bucket would count to one and start
    /// over, which is not a limit.
    pub fn allow(&self, ip: IpAddr) -> bool {
        self.allow_at(ip, tp_core::now_ms())
    }

    /// The mechanism, with the clock passed in so a test can drive it. Tying
    /// refill to wall time would leave the recovery half of this untestable,
    /// and recovery is the half a legitimate peer depends on.
    pub(crate) fn allow_at(&self, ip: IpAddr, now_ms: i64) -> bool {
        let mut buckets = self.buckets.lock().unwrap();

        // A bucket that has refilled to capacity says exactly what an absent
        // one says. Dropping those here — on the same path that creates
        // entries — is what keeps the map proportional to CURRENT load rather
        // than to every address that has ever connected.
        buckets.retain(|_, b| refill(b, now_ms) < CAPACITY);

        if !buckets.contains_key(&ip) {
            if buckets.len() >= MAX_BUCKETS {
                // Everything still here is below capacity, so there is no
                // harmless entry left to evict. Refuse the NEW address rather
                // than displace an existing one: displacing hands the flood a
                // fresh budget at a legitimate peer's expense, which is the
                // outcome this whole file exists to prevent. The failure is
                // loud (429) rather than silent, and it clears itself as
                // buckets refill.
                return false;
            }
            buckets.insert(
                ip,
                Bucket {
                    tokens: CAPACITY,
                    last_ms: now_ms,
                },
            );
        }

        let b = buckets.get_mut(&ip).expect("just inserted");
        if b.tokens < 1.0 {
            return false;
        }
        b.tokens -= 1.0;
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

/// Bring a bucket up to `now_ms` and report what it holds.
fn refill(b: &mut Bucket, now_ms: i64) -> f64 {
    // `max(0)` because the clock can step backwards (NTP, sleep/wake). A
    // negative elapsed would DRAIN the bucket, turning a time correction into
    // an outage for whoever happened to be talking to us.
    let elapsed = (now_ms - b.last_ms).max(0) as f64;
    b.tokens = (b.tokens + elapsed * REFILL_PER_MS).min(CAPACITY);
    b.last_ms = now_ms;
    b.tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn a_burst_is_allowed_and_then_refused() {
        let rl = RateLimiter::new();
        for i in 0..CAPACITY as usize {
            assert!(
                rl.allow_at(ip(1), 0),
                "request {i} should be within the burst"
            );
        }
        assert!(!rl.allow_at(ip(1), 0), "the burst must not be exceeded");
    }

    #[test]
    fn tokens_come_back_over_time() {
        let rl = RateLimiter::new();
        for _ in 0..CAPACITY as usize {
            rl.allow_at(ip(1), 0);
        }
        assert!(!rl.allow_at(ip(1), 0));
        // Half the refill window buys back part of the burst, not all of it.
        assert!(rl.allow_at(ip(1), 30_000));
        // And a full window restores it completely.
        for _ in 0..CAPACITY as usize {
            assert!(rl.allow_at(ip(1), 120_000));
        }
    }

    #[test]
    fn one_address_cannot_spend_anothers_budget() {
        let rl = RateLimiter::new();
        for _ in 0..CAPACITY as usize {
            rl.allow_at(ip(1), 0);
        }
        assert!(!rl.allow_at(ip(1), 0), "the flooder is cut off");
        assert!(
            rl.allow_at(ip(2), 0),
            "a different peer must be unaffected — otherwise one address can \
             deny pairing to the whole network"
        );
    }

    #[test]
    fn a_clock_that_steps_backwards_does_not_drain_a_bucket() {
        let rl = RateLimiter::new();
        assert!(rl.allow_at(ip(1), 100_000));
        assert!(
            rl.allow_at(ip(1), 0),
            "an earlier timestamp must not penalise"
        );
    }

    #[test]
    fn the_map_cannot_grow_without_bound() {
        let rl = RateLimiter::new();
        // Every address spends one token, so no bucket is full and none can be
        // reaped — the worst case for the map.
        for n in 0..=u8::MAX {
            for m in 0..=u8::MAX {
                rl.allow_at(IpAddr::from([10, 0, n, m]), 0);
            }
        }
        assert!(
            rl.len() <= MAX_BUCKETS,
            "65536 distinct addresses left {} buckets allocated",
            rl.len()
        );
    }

    #[test]
    fn a_bucket_that_refilled_is_reaped() {
        let rl = RateLimiter::new();
        rl.allow_at(ip(1), 0);
        assert_eq!(rl.len(), 1);
        // Long enough to refill to capacity, at which point the entry carries
        // no information the absent case does not.
        rl.allow_at(ip(2), 600_000);
        assert!(
            !rl.buckets.lock().unwrap().contains_key(&ip(1)),
            "a full bucket must not be kept"
        );
    }
}
