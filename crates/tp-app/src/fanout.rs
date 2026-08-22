//! Which peers a federated search actually queries, and what came back.
//!
//! This policy existed twice — `run_search_all` in the CLI and `search_all` in
//! the MCP server, neither going through this layer. The MCP copy says so in a
//! comment: "Same two guards the CLI applies, for the same reason". They had
//! already drifted: the CLI checked "is anything addressable" BEFORE resolving
//! named peers and the MCP server checked it after, so naming a peer that does
//! not exist, on a machine with no reachable peers, succeeded quietly on one
//! surface and errored on the other.
//!
//! Neither was right, and the split made that hard to see. Consider one trusted
//! peer whose row has no address (mDNS went stale, or it was paired by id), and
//! a caller who mistypes its name:
//!
//! * the CLI printed "no reachable trusted peers; showing local results only"
//!   and exited 0, never looking at the name — the caller asked one machine and
//!   got local results back with no way to tell "it answered nothing" from "it
//!   was never asked";
//! * MCP said "no trusted peer matches" — which is FALSE. The peer exists; it
//!   has no address. A caller acting on that error retypes the name forever
//!   instead of running discovery.
//!
//! So the rule here is neither of them: resolve names FIRST, keep "no such
//! name" and "named, but unusable" apart, and refuse only when nothing the
//! caller named can be reached — reporting the real reason. `--all` keeps the
//! CLI's behaviour, because with nobody named there is nothing to be wrong
//! about: zero reachable peers is a warning and local results stand.
//!
//! Every outcome below is returned as DATA. The two surfaces word their errors
//! differently on purpose (`tp peers` and `--peer` vs `teleport_peers` and
//! `peers`), and that is rendering, not policy.

use anyhow::Result;
use tp_db::Db;
use tp_net::{FanOutResult, PeerAddr, PeerHit};

/// What a fan-out should do, decided before any network call.
#[derive(Debug)]
pub enum Fanout {
    /// Query these. `no_address` is every trusted peer that could not be
    /// queried at all, reported so a caller is never quietly given a narrower
    /// answer than it asked for.
    Ready {
        peers: Vec<PeerAddr>,
        no_address: Vec<String>,
    },
    /// `--all` with nothing reachable. NOT an error: nobody was named, so the
    /// local results are a complete answer to the question that was asked.
    NothingReachable { no_address: Vec<String> },
    /// Peers were named and none of them can be queried, with the reason for
    /// each — the distinction the two old copies each got half of.
    NoneUsable {
        /// Named, matched nothing at all. A typo, or a machine never paired.
        unmatched: Vec<String>,
        /// Named, matched a trusted peer, but that peer has no address.
        /// Retyping the name will never fix this; discovery or re-pairing will.
        without_address: Vec<String>,
    },
    /// One name matched several peers. Reported rather than guessed: picking
    /// the first would search a machine the caller did not mean.
    Ambiguous { want: String, matched: Vec<String> },
    /// `--all` past the cap. A peer answers a search by scanning its whole
    /// corpus, so "search everywhere" past a handful is a decision to make on
    /// purpose rather than a default.
    TooMany { reachable: usize },
}

/// Decide who to query.
///
/// `only` empty means "all". Non-empty means the caller named them, which works
/// at any number of peers — the cap exists to stop an unbounded default, not to
/// stop a deliberate choice.
pub fn select(db: &Db, only: &[String]) -> Result<Fanout> {
    let rows = tp_db::query::trusted_peers(db.conn())?;
    let (addressable, no_addr): (Vec<_>, Vec<_>) = rows.into_iter().partition(|p| p.addr.is_some());
    let no_address: Vec<String> = no_addr.iter().map(|p| p.name.clone()).collect();

    let matches =
        |p: &tp_db::query::MachineRow, want: &str| p.id.starts_with(want) || p.name == want;

    if !only.is_empty() {
        let mut picked = Vec::new();
        let mut unmatched = Vec::new();
        let mut without_address = Vec::new();

        for want in only {
            let hits: Vec<_> = addressable.iter().filter(|p| matches(p, want)).collect();
            match hits.as_slice() {
                [p] => picked.push((*p).clone()),
                [..] if hits.len() > 1 => {
                    return Ok(Fanout::Ambiguous {
                        want: want.clone(),
                        matched: hits.iter().map(|p| p.name.clone()).collect(),
                    })
                }
                // Nothing addressable matched. Before calling it a bad name,
                // look at the peers we had to exclude — a trusted peer with no
                // address is a DIFFERENT failure, and the one the caller can
                // actually act on.
                _ => match no_addr.iter().find(|p| matches(p, want)) {
                    Some(p) => without_address.push(p.name.clone()),
                    None => unmatched.push(want.clone()),
                },
            }
        }

        if picked.is_empty() {
            return Ok(Fanout::NoneUsable {
                unmatched,
                without_address,
            });
        }
        // Some named peers are reachable and some are not: query the reachable
        // ones and report the rest, rather than failing the whole search.
        let mut reported = without_address;
        reported.extend(unmatched);
        return Ok(Fanout::Ready {
            peers: to_addrs(&picked),
            no_address: reported,
        });
    }

    if addressable.is_empty() {
        return Ok(Fanout::NothingReachable { no_address });
    }
    if addressable.len() > tp_net::peer::FANOUT_REFUSE_ABOVE {
        return Ok(Fanout::TooMany {
            reachable: addressable.len(),
        });
    }
    Ok(Fanout::Ready {
        peers: to_addrs(&addressable),
        no_address,
    })
}

fn to_addrs(rows: &[tp_db::query::MachineRow]) -> Vec<PeerAddr> {
    rows.iter()
        .map(|p| PeerAddr {
            device_id: p.id.clone(),
            name: p.name.clone(),
            addr: p.addr.clone().expect("partitioned on is_some"),
            // Needed to verify this peer's RESPONSE. A trusted row without a
            // stored key is reported as that peer failing, never as an empty
            // answer — the fan-out drops what it cannot check.
            pubkey: p.pubkey.clone(),
        })
        .collect()
}

/// One remote hit, with the peer named rather than identified.
pub struct Remote {
    /// The peer's name when we know it, its device id otherwise.
    pub machine: String,
    pub hit: PeerHit,
}

/// Everything a caller needs to report a fan-out.
pub struct Merged {
    /// Remote hits only, newest first. Local hits are folded in for ORDERING
    /// and then removed: both surfaces have already printed them, and leaving
    /// them in would list every local hit twice.
    pub remote: Vec<Remote>,
    /// Coverage across the whole fan-out.
    pub degraded: Option<String>,
    pub answered: Vec<(String, usize)>,
    pub failed: Vec<(String, String)>,
    pub peer_degraded: Vec<(String, String)>,
}

/// Order local and remote hits together, then hand back only the remote ones.
///
/// The local hits must take part in the sort — they are the caller's own most
/// recent results and a merged list that appends them is not a merged list —
/// which is why they are tagged with this machine's device id and filtered out
/// again afterwards. Both surfaces wrote that dance out; getting the filter
/// wrong duplicates every local hit, and getting the tag wrong drops them.
pub fn merge(
    me: &str,
    peers: &[PeerAddr],
    local: &tp_core::Retrieved<tp_core::Hit>,
    fan: FanOutResult,
) -> Merged {
    let local_tagged: Vec<(String, PeerHit)> = local
        .items
        .iter()
        .map(|h| {
            (
                me.to_string(),
                PeerHit {
                    session_id: h.at.session_id.clone(),
                    ts: h.at.ts,
                    excerpt: h.excerpt().to_string(),
                    role: format!("{:?}", h.role).to_lowercase(),
                    sidechain: h.sidechain,
                    surface: h.surface,
                },
            )
        })
        .collect();

    let answered = fan.answered.clone();
    let failed = fan.failed.clone();
    let peer_degraded = fan.peer_degraded.clone();
    let (all, degraded) = tp_net::merge_hits(local_tagged, fan);

    let by_id: std::collections::HashMap<&str, &str> = peers
        .iter()
        .map(|p| (p.device_id.as_str(), p.name.as_str()))
        .collect();

    let remote = all
        .into_iter()
        .filter(|(m, _)| m != me)
        .map(|(machine, hit)| Remote {
            machine: by_id
                .get(machine.as_str())
                .map(|n| n.to_string())
                .unwrap_or(machine),
            hit,
        })
        .collect();

    Merged {
        remote,
        degraded,
        answered,
        failed,
        peer_degraded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("me", "TestMac").unwrap();
        db
    }

    fn peer(db: &Db, id: &str, name: &str, addr: Option<&str>) {
        db.conn()
            .execute(
                "INSERT INTO machine(id, name, trust, addr, created_at)
                 VALUES (?1, ?2, 'trusted', ?3, unixepoch())",
                [Some(id), Some(name), addr],
            )
            .unwrap();
    }

    fn names(f: &Fanout) -> Vec<String> {
        match f {
            Fanout::Ready { peers, .. } => peers.iter().map(|p| p.name.clone()).collect(),
            _ => panic!("expected Ready, got {f:?}"),
        }
    }

    /// The divergence this module was written to settle, from the side the CLI
    /// got wrong: naming a peer that cannot be reached must not quietly
    /// succeed. The caller asked one machine a question.
    #[test]
    fn naming_an_unreachable_peer_is_not_a_silent_local_only_search() {
        let db = db();
        peer(&db, "aaa", "laptop-b", None);

        match select(&db, &["laptop-b".into()]).unwrap() {
            Fanout::NoneUsable {
                unmatched,
                without_address,
            } => {
                assert_eq!(without_address, ["laptop-b"]);
                assert!(unmatched.is_empty());
            }
            other => panic!("expected NoneUsable, got {other:?}"),
        }
    }

    /// And from the side MCP got wrong: the reason must be true. "No trusted
    /// peer matches" sends the caller back to retype a name that was right.
    #[test]
    fn a_named_peer_without_an_address_is_not_reported_as_a_bad_name() {
        let db = db();
        peer(&db, "aaa", "laptop-b", None);

        match select(&db, &["laptop-b".into()]).unwrap() {
            Fanout::NoneUsable { unmatched, .. } => assert!(
                unmatched.is_empty(),
                "the name matched — the peer has no address, which is a different fix"
            ),
            other => panic!("expected NoneUsable, got {other:?}"),
        }
    }

    #[test]
    fn a_name_that_matches_nothing_is_reported_as_unmatched() {
        let db = db();
        peer(&db, "aaa", "laptop-b", Some("10.0.0.1:47401"));

        match select(&db, &["typo".into()]).unwrap() {
            Fanout::NoneUsable {
                unmatched,
                without_address,
            } => {
                assert_eq!(unmatched, ["typo"]);
                assert!(without_address.is_empty());
            }
            other => panic!("expected NoneUsable, got {other:?}"),
        }
    }

    /// Naming several peers where only some are reachable searches the ones
    /// that are and reports the rest. Failing the whole search would punish a
    /// caller for casting a wider net.
    #[test]
    fn a_partly_reachable_selection_queries_what_it_can_and_says_what_it_could_not() {
        let db = db();
        peer(&db, "aaa", "here", Some("10.0.0.1:47401"));
        peer(&db, "bbb", "gone", None);

        let f = select(&db, &["here".into(), "gone".into()]).unwrap();
        assert_eq!(names(&f), ["here"]);
        match f {
            Fanout::Ready { no_address, .. } => assert_eq!(no_address, ["gone"]),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// With nobody named there is nothing to be wrong about: local results are
    /// a complete answer to the question that was asked.
    #[test]
    fn all_with_nothing_reachable_is_a_warning_not_an_error() {
        let db = db();
        peer(&db, "aaa", "laptop-b", None);

        match select(&db, &[]).unwrap() {
            Fanout::NothingReachable { no_address } => assert_eq!(no_address, ["laptop-b"]),
            other => panic!("expected NothingReachable, got {other:?}"),
        }
    }

    /// The cap applies to `--all` only. Naming peers is a deliberate choice and
    /// works at any number.
    #[test]
    fn the_cap_bounds_the_default_and_not_a_deliberate_selection() {
        let db = db();
        for i in 0..=tp_net::peer::FANOUT_REFUSE_ABOVE {
            peer(&db, &format!("id{i}"), &format!("m{i}"), Some("10.0.0.1:1"));
        }

        match select(&db, &[]).unwrap() {
            Fanout::TooMany { reachable } => {
                assert_eq!(reachable, tp_net::peer::FANOUT_REFUSE_ABOVE + 1)
            }
            other => panic!("expected TooMany, got {other:?}"),
        }

        let named: Vec<String> = (0..=tp_net::peer::FANOUT_REFUSE_ABOVE)
            .map(|i| format!("m{i}"))
            .collect();
        assert_eq!(names(&select(&db, &named).unwrap()).len(), named.len());
    }

    /// An ambiguous prefix is reported, never guessed — picking the first would
    /// search a machine the caller did not mean.
    #[test]
    fn an_ambiguous_name_is_reported_with_what_it_matched() {
        let db = db();
        peer(&db, "aaa1", "mac-one", Some("10.0.0.1:1"));
        peer(&db, "aaa2", "mac-two", Some("10.0.0.2:1"));

        match select(&db, &["aaa".into()]).unwrap() {
            Fanout::Ambiguous { want, matched } => {
                assert_eq!(want, "aaa");
                assert_eq!(matched.len(), 2);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    /// A peer is selectable by id prefix or by exact name — the two forms
    /// `tp peers` prints.
    #[test]
    fn a_peer_is_found_by_id_prefix_or_exact_name() {
        let db = db();
        peer(&db, "ABCD-EFGH", "laptop-b", Some("10.0.0.1:1"));
        assert_eq!(names(&select(&db, &["ABCD".into()]).unwrap()), ["laptop-b"]);
        assert_eq!(
            names(&select(&db, &["laptop-b".into()]).unwrap()),
            ["laptop-b"]
        );
    }
}
