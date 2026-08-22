//! The operations, with no opinion about how to report them.
//!
//! Everything teleport DOES lived in `tp/src/main.rs` — 515 lines of it, mixed
//! with clap declarations, prose formatting and the process entry point. Three
//! consequences, all measured rather than assumed:
//!
//! * **It could not be unit tested.** `main.rs` has 15 tests and every one
//!   covers a pure helper — `parse_duration`, `fmt_ts`, `turn_body`. Not one
//!   covers a `run_*`. The only way to exercise the actual behaviour was to
//!   spawn the binary, which is why `address_honesty_e2e` and
//!   `turns_window_e2e` exist. Compare `tp-reach`: 60 unit tests, same kind of
//!   logic, reachable because it lives in a library.
//! * **It could not be reused.** `mcp.rs` needed the same work and had no
//!   function to call, so it wrote a second copy. Every divergence between the
//!   two copies has been a bug: `teleport_ask` hardcoding `claude_code` as the
//!   sender's runtime left a codex reply unanswerable, and an `unreachable!()`
//!   the CLI had already removed would have panicked on any dsh session. An
//!   Entrography scan put a number on the coupling — git records the pair
//!   edited together 37% of the time.
//! * **It could not be depended on.** A binary is not a library. Nothing else
//!   in the workspace could build on it even where that was the obvious move.
//!
//! So an operation here takes what it needs and RETURNS A VALUE. It does not
//! print, does not serialise, and does not decide what a result means. The CLI
//! renders prose for a person; the MCP server serialises structured data for a
//! model; both are adapters over the same behaviour, and behaviour cannot
//! diverge between them because there is one copy of it.
//!
//! The domain crates below (`tp-db`, `tp-reach`, `tp-search`, `tp-ingest`) are
//! unchanged and remain where the rules live. This layer composes them into the
//! operations a user or an agent actually asks for — it is a coordinator, not a
//! second home for domain logic.

pub mod fanout;
pub mod inbox;
pub mod ingest;
pub mod pair;
pub mod peers;
pub mod read;
pub mod send;
pub mod session;

/// Re-exported because it is a public FIELD of `Sent`. A caller could not name
/// the type it was handed without adding a direct dependency on tp-db and
/// reaching two crates down — so both surfaces did exactly that, and neither
/// used the `Sent.addressability` it already had. Each re-queried the database
/// and re-implemented the four-way classification instead (Item 24: re-export
/// the dependencies that appear in your API).
pub use tp_db::reach::Addressability;

pub use fanout::Fanout;
pub use inbox::{ack, drain, history, own_session, pending, Drained};
pub use pair::{Direction, Pairings, Pending};
pub use peers::{classify_discovered, discover, live, Discovered, LiveSession, Probed};
pub use read::{is_partial, resolve_session, Resolution};
pub use send::{reply, send, Kind, Sent};
pub use session::{parse_presence, Host, Registered};

/// Reject an address that is not structurally an address, BEFORE anything is
/// enqueued.
///
/// A mailbox deliberately has no FK to `session` — it must accept an id for a
/// session teleport has not indexed yet (migration 0001) — so "unknown" cannot
/// be an error. Malformed is different: `<machine>/<runtime>/<native>` is the
/// wire address (LLD §4), and a string that cannot be parsed as one is not an
/// address that any future registration could make valid.
///
/// This is not hypothetical. All four never-deliverable messages found on a real
/// install failed exactly this check: two bare native ids with no prefix at all,
/// one bare machine id with no session part, and one id truncated mid-runtime
/// (`…/claud`). Each was accepted, stored, and silently never delivered.
pub fn validate_address(target: &str) -> anyhow::Result<()> {
    if tp_core::SessionId::parse(target).is_some() {
        return Ok(());
    }
    anyhow::bail!(
        "{target:?} is not a session address — expected `<machine>/<runtime>/<native>`.\n\
         A bare session id, a bare machine id, or a truncated one cannot be delivered to, \
         and teleport would otherwise accept it and drop it silently.\n\
         Run `tp live` and copy an address from there; never assemble one yourself."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every malformed address observed in the wild. These used to be reachable
    /// only by spawning the binary — the whole point of this layer is that a
    /// rule like this is now testable where it is defined.
    #[test]
    fn malformed_addresses_are_refused() {
        for bad in [
            "62407be6-b2f2-4595-a8c8-03d12cb817e6", // bare native id
            "SWSQ-BCLE-LXKA-MP2R",                  // bare machine id
            "SWSQ-BCLE-LXKA-MP2R/claud",            // truncated before the native id
            "machine//native",                      // empty runtime segment
        ] {
            let err = validate_address(bad).unwrap_err().to_string();
            assert!(err.contains("is not a session address"), "{bad:?}: {err}");
            assert!(err.contains("tp live"), "{bad:?} must say where to get one");
        }
    }

    #[test]
    fn a_well_formed_address_passes_even_when_unknown() {
        // Unknown is not malformed: a mailbox accepts an id before its session
        // is indexed, by design.
        validate_address("m/claude_code/never-seen-before").unwrap();
        // A conversation address is well-formed too — it is what `tp live`
        // publishes.
        validate_address("m/claude_code/conv-0000-1111").unwrap();
    }
}
