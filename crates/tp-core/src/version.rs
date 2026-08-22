//! One build identity, shared by every binary and reported over the wire.
//!
//! Three separate things had to agree on "what version is this" and none of
//! them could: `tp` had no `--version` at all, `tpd` printed a banner to a log
//! nobody reads, and `/v1/ping` answered `0.1.0` — the same `0.1.0` it had
//! answered through every rebuild, so it could not distinguish a daemon that
//! had been up for a week from one restarted a minute ago. The question a
//! person actually asks is "is what is RUNNING what I just installed", and a
//! semver that never changes cannot answer it. The commit can.

/// Semantic version, from the workspace. Changes on release, not on build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short commit this was built from, `-dirty` if the tree had uncommitted
/// changes, or `unknown` outside a git checkout. THIS is the field that
/// distinguishes two builds; see `build.rs`.
pub const GIT_SHA: &str = env!("TP_GIT_SHA");

/// UTC date of the build, `YYYY-MM-DD`.
pub const BUILD_DATE: &str = env!("TP_BUILD_DATE");

/// The one-line form every surface prints: `0.1.0 (33349da, 2026-08-16)`.
pub const VERSION_LINE: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("TP_GIT_SHA"),
    ", ",
    env!("TP_BUILD_DATE"),
    ")"
);

/// Whether two build lines describe the same code.
///
/// Three outcomes, not a bool, because a caller acts on each differently:
/// `Different` is worth interrupting someone over ("restart the daemon"),
/// `Unknown` is worth staying quiet about, and collapsing them would either
/// nag on every rebuild of a working tree or hide a genuinely stale daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildMatch {
    /// Same commit. A rebuild of it on another day still counts — it is the
    /// same code.
    Same,
    /// Different commits. Someone should act.
    Different,
    /// Not comparable. A `-dirty` tree does not identify its code, so two
    /// `-dirty` builds of one commit may be entirely different source; a
    /// build from outside a git checkout says `unknown` for the same reason.
    /// Saying "stale" here would be a guess presented as a fact.
    Unknown,
}

pub fn compare_builds(a: &str, b: &str) -> BuildMatch {
    let (Some(x), Some(y)) = (sha_of(a), sha_of(b)) else {
        return BuildMatch::Unknown;
    };
    if x.contains("dirty") || y.contains("dirty") || x == "unknown" || y == "unknown" {
        return BuildMatch::Unknown;
    }
    if x == y {
        BuildMatch::Same
    } else {
        BuildMatch::Different
    }
}

/// The commit out of `0.1.0 (33349da, 2026-08-16)`.
fn sha_of(line: &str) -> Option<String> {
    line.split_once('(')
        .and_then(|(_, rest)| rest.split_once([',', ')']))
        .map(|(sha, _)| sha.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_carries_the_commit() {
        assert!(
            VERSION_LINE.contains(VERSION) && VERSION_LINE.contains(GIT_SHA),
            "{VERSION_LINE}"
        );
    }

    #[test]
    fn the_same_commit_built_on_different_days_is_the_same_build() {
        assert_eq!(
            compare_builds("0.1.0 (33349da, 2026-08-16)", "0.1.0 (33349da, 2026-08-17)"),
            BuildMatch::Same
        );
    }

    #[test]
    fn different_commits_are_different_builds() {
        assert_eq!(
            compare_builds("0.1.0 (33349da, 2026-08-16)", "0.1.0 (a781e94, 2026-08-16)"),
            BuildMatch::Different
        );
    }

    /// Two dirty trees at one commit can hold completely different source, so
    /// the honest answer is "cannot tell" — NOT `Different`, which would nag
    /// on every rebuild during development, and NOT `Same`, which would hide a
    /// daemon that really is stale.
    #[test]
    fn a_dirty_tree_is_not_comparable_either_way() {
        assert_eq!(
            compare_builds(
                "0.1.0 (33349da-dirty, 2026-08-16)",
                "0.1.0 (33349da-dirty, 2026-08-16)"
            ),
            BuildMatch::Unknown
        );
        assert_eq!(
            compare_builds(
                "0.1.0 (33349da-dirty, 2026-08-16)",
                "0.1.0 (a781e94, 2026-08-16)"
            ),
            BuildMatch::Unknown
        );
        assert_eq!(
            compare_builds("0.1.0 (unknown, 2026-08-16)", "0.1.0 (unknown, 2026-08-16)"),
            BuildMatch::Unknown
        );
    }

    #[test]
    fn a_malformed_line_is_unknown_not_a_match() {
        assert_eq!(compare_builds("0.1.0", "0.1.0"), BuildMatch::Unknown);
        assert_eq!(compare_builds("", ""), BuildMatch::Unknown);
    }
}
