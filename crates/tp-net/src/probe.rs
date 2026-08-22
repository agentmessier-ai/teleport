//! Finding a peer by asking it, rather than by listening for it.
//!
//! This replaced mDNS. The trade is deliberate: teleport no longer learns which
//! machines exist, and instead requires you to name a host — which is a cost
//! only if you don't know your own machines, and the people running this do.
//!
//! What that bought:
//!
//! * **No 5353 binding.** `tpd` used to run a full mDNS responder inside the
//!   daemon, alongside the platform's own (`mDNSResponder` on macOS, Avahi
//!   where installed). One fewer multicast listener, one fewer thing to explain.
//! * **It works where multicast doesn't.** mDNS is link-local (TTL 1): it does
//!   not cross a router, a VPN, a cloud VPC, or Docker's default bridge. Those
//!   were exactly the cases the old design pushed onto "static peers", i.e. onto
//!   typing an address — which is now simply the only path, and one path that
//!   always works beats two where the automatic one usually doesn't.
//! * **No firewall rule for inbound multicast**, which server Linux does not
//!   grant by default.
//!
//! It is NOT a subnet sweep, and the distinction is the whole reason this is
//! acceptable where LLD §8.1 rejected scanning. Sweeping a `/24` for open ports
//! is reconnaissance and reads as such to any IDS on the network; probing a
//! HOST YOU NAMED across a handful of ports is what every client that takes a
//! `--port` flag does. teleport never contacts an address the operator did not
//! type.

use crate::client;
use anyhow::Result;

/// Where a teleport daemon listens unless told otherwise.
pub const DEFAULT_PORT: u16 = 47400;

/// How many consecutive ports a bare host is probed across.
///
/// Small on purpose. The only reason a daemon is not on `DEFAULT_PORT` is that
/// something else took it or a second daemon runs beside it, and both are
/// near-misses — a wide range would turn "check the obvious neighbours" into
/// the port scan this design exists to avoid.
pub const PROBE_PORTS: u16 = 8;

/// A daemon that answered `/v1/ping`.
///
/// Same shape the mDNS browse returned, and for a better reason than
/// compatibility: this is evidence rather than an advertisement. mDNS reported
/// whatever a machine broadcast about itself, unverified and unsolicited; every
/// field here comes from a response to a request we made, and `client::ping`
/// has already checked that `device_id` is the fingerprint of the key that
/// answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    pub device_id: String,
    pub name: String,
    pub addr: String, // host:port
    /// The build it reported. Free here — the response already carries it —
    /// and it is the one place two machines' versions can be compared.
    pub version: String,
}

/// Probe one host across `DEFAULT_PORT..DEFAULT_PORT+PROBE_PORTS`.
///
/// `host` may carry an explicit `:port`, in which case exactly that one is
/// tried: an operator who names a port has answered the question this function
/// otherwise guesses at, and guessing past their answer would be wrong.
///
/// A port that refuses, times out, or answers something that is not a teleport
/// daemon is simply absent from the result. Nothing is stored — see
/// `tp_app::peers::classify_discovered` for why appearing on a network is not
/// a relationship.
pub async fn probe(host: &str) -> Result<Vec<DiscoveredPeer>> {
    if let Some((h, p)) = split_host_port(host) {
        return Ok(ping_one(&h, p).await.into_iter().collect());
    }
    // Concurrent: a filtered port costs the full client timeout, and doing
    // eight of those in series would make a firewalled host take most of a
    // minute to report nothing.
    let mut set = tokio::task::JoinSet::new();
    for offset in 0..PROBE_PORTS {
        let host = host.to_string();
        set.spawn(async move { ping_one(&host, DEFAULT_PORT + offset).await });
    }
    let mut found = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Some(peer)) = res {
            found.push(peer);
        }
    }
    // Ports are probed concurrently, so arrival order is a race. Sort so the
    // same network answers the same way twice.
    found.sort_by(|a, b| a.addr.cmp(&b.addr));
    Ok(found)
}

async fn ping_one(host: &str, port: u16) -> Option<DiscoveredPeer> {
    let addr = format!("{host}:{port}");
    let (resp, _key) = client::ping(&addr).await.ok()?;
    Some(DiscoveredPeer {
        device_id: resp.device_id,
        name: resp.name,
        version: resp.version,
        addr,
    })
}

/// `10.0.0.4:47400` → `("10.0.0.4", 47400)`. `None` when no explicit port.
///
/// Splits from the RIGHT so a bare IPv6 literal is not mistaken for a
/// host:port — `::1` has colons that are not a port separator.
fn split_host_port(host: &str) -> Option<(String, u16)> {
    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:47400`
        let (h, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?.parse().ok()?;
        return Some((format!("[{h}]"), port));
    }
    let (h, p) = host.rsplit_once(':')?;
    if h.contains(':') {
        // A bare IPv6 literal, not host:port.
        return None;
    }
    Some((h.to_string(), p.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_port_is_taken_literally() {
        assert_eq!(
            split_host_port("10.0.0.4:47411"),
            Some(("10.0.0.4".into(), 47411))
        );
        assert_eq!(
            split_host_port("mac.local:47400"),
            Some(("mac.local".into(), 47400))
        );
    }

    #[test]
    fn a_bare_host_has_no_port_and_gets_the_range() {
        assert_eq!(split_host_port("10.0.0.4"), None);
        assert_eq!(split_host_port("mac.local"), None);
    }

    /// An IPv6 literal is full of colons and none of them is a port. Splitting
    /// from the right would read `::1` as host `:` port `1`.
    #[test]
    fn an_ipv6_literal_is_not_mistaken_for_a_port() {
        assert_eq!(split_host_port("::1"), None);
        assert_eq!(split_host_port("fe80::1"), None);
        assert_eq!(
            split_host_port("[::1]:47400"),
            Some(("[::1]".into(), 47400))
        );
    }

    #[test]
    fn a_garbage_port_is_not_a_port() {
        assert_eq!(split_host_port("host:notaport"), None);
    }

    /// The range stays small deliberately — see `PROBE_PORTS`. Asserted as a
    /// range rather than an equality so tuning it stays possible, but turning
    /// it into a scanner does not.
    #[test]
    fn the_probe_range_is_neighbours_not_a_scan() {
        let ports: u32 = PROBE_PORTS.into();
        assert!(
            (1..=16).contains(&ports),
            "probing {ports} ports is a scan, not a check of the obvious neighbours"
        );
    }
}
