pub mod id;
pub mod logging;
pub mod retrieval;
pub mod turn;
pub mod version;

pub use id::SessionId;
pub use retrieval::{
    Capabilities, Coverage, Hit, Query, RawHit, RetrievalProvider, Retrieved, Scope, SessionRow,
    TurnCursor, TurnRef, DEFAULT_WINDOW,
};
pub use turn::{NormalizedTurn, ParseChunk, Role, SessionMeta, TitleSource, ToolCallDigest};
pub use version::{compare_builds, BuildMatch, BUILD_DATE, GIT_SHA, VERSION, VERSION_LINE};

/// Milliseconds since the unix epoch.
///
/// Existed six times across five crates when an Entrography scan found two of
/// them byte-identical — the scan reports hash equality, so the four that had
/// drifted in whitespace or error handling did not show up at all. Every copy
/// was the same three lines with the same `unwrap_or(0)` on a clock that cannot
/// realistically fail; the risk was never one of them being wrong, but of a
/// later fix landing in whichever copy the author happened to open.
///
/// `tp-core` because every crate that needs a timestamp already depends on it.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Exit quietly when the reader of our stdout goes away.
///
/// Rust's runtime sets SIGPIPE to SIG_IGN before `main`, so a write to a closed
/// pipe returns EPIPE instead of killing the process — and `println!` turns that
/// error into a PANIC. The result is that any command whose output outlives its
/// reader dies with a backtrace where every other CLI exits silently:
///
/// ```text
/// $ tp id | head -1
/// SWSQ-BCLE-…
/// thread 'main' panicked at library/std/src/io/stdio.rs:1166:9:
/// failed printing to stdout: Broken pipe (os error 32)
/// ```
///
/// `| head`, `| less` quit early, `| grep -q` — all of it. Reported from two
/// machines on the same day (a macOS install and a fresh Linux build), which is
/// what a default rather than a platform quirk looks like.
///
/// Restoring SIG_DFL hands the decision back to the kernel: the process is
/// killed by SIGPIPE at the first doomed write, which is precisely what `git`,
/// `rg` and every other well-behaved CLI do. Catching the error at each print
/// site instead would mean auditing every `println!` in the tree and every one
/// added afterwards.
///
/// Called from `main` in both binaries. It affects only THIS process, never a
/// child, so nothing the daemon spawns inherits it.
///
/// # Safety
/// `signal` with `SIG_DFL` on `SIGPIPE` touches no memory and cannot fail in a
/// way that matters here; the return value is the previous handler, discarded.
pub fn exit_quietly_on_broken_pipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
