//! Stamps the build with the commit it came from.
//!
//! `0.1.0` cannot answer the only version question anyone actually asks here:
//! "is the thing running the thing I just built?" The workspace version has
//! been 0.1.0 across every rebuild this project has ever had, so a binary,
//! a daemon that has been up for a week, and a peer on another machine all
//! report the same string while being entirely different code.
//!
//! The commit answers it. Everything else in `version.rs` builds on these two
//! variables.

use std::process::Command;

fn main() {
    // Re-run when HEAD moves, so a rebuild after a commit does not keep
    // stamping the old sha. `.git/HEAD` covers commits and branch switches;
    // packed-refs and the ref file cover the rest.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");

    println!("cargo:rustc-env=TP_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=TP_BUILD_DATE={}", build_date());
}

/// The short commit, suffixed `-dirty` when the tree has uncommitted changes.
///
/// A build from a source tarball has no git at all, and that is not an error —
/// it reports `unknown` rather than failing the build, because refusing to
/// compile outside a git checkout would be a worse outcome than a vague
/// version string.
fn git_sha() -> String {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let Some(sha) = sha else {
        return "unknown".to_string();
    };

    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    if dirty {
        format!("{sha}-dirty")
    } else {
        sha
    }
}

/// UTC date of the build, `YYYY-MM-DD`. Deliberately a DATE and not a
/// timestamp: it is here to tell two builds of the same commit apart in a
/// human-readable way, not to be an audit record, and a full timestamp would
/// change on every rebuild and defeat build caching for no gain.
fn build_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    civil_from_days(secs / 86_400)
}

/// Howard Hinnant's `civil_from_days`, the standard proleptic-Gregorian
/// conversion. Written out rather than pulling `chrono` into a build script
/// that would then have to compile before anything else in the workspace.
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
