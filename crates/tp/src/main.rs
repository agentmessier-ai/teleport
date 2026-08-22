use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod cmd;
mod mcp;

// The crate-root paths every command and `mcp.rs` were written against.
pub(crate) use cmd::net::*;
pub(crate) use cmd::reach::*;
pub(crate) use cmd::read::*;

#[derive(Parser)]
#[command(
    name = "tp",
    about = "Teleport — cross-agent session search and reach",
    version = tp_core::VERSION_LINE
)]
struct Cli {
    /// Use the SQLite/FTS5 index instead of the default on-demand scan.
    /// Requires `tp index` to have been run (LLD §16).
    #[arg(long, global = true)]
    index: bool,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build/refresh the optional index. Not required for search.
    Index,
    /// Re-read transcripts that are still on disk, so columns added after a
    /// session was first indexed stop being empty. Sessions whose transcript is
    /// gone are left untouched — for those, the index is the only copy.
    Reindex {
        /// Report what would be re-read and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Only this runtime (`pi`, `codex`, `claude_code`, …).
        ///
        /// A descriptor change usually affects ONE runtime, and re-reading the
        /// other 28,000 sessions to pick it up is work with no result. Left
        /// unset, every readable transcript is re-read.
        #[arg(long)]
        runtime: Option<String>,
    },
    /// Search across sessions. Returns coordinates + an excerpt, not full transcripts.
    Search {
        query: String,
        /// Also search (and return) `thinking`. Off by default at every layer.
        #[arg(long)]
        include_thinking: bool,
        /// Restrict to sessions whose path matches this folder.
        #[arg(long)]
        folder: Option<String>,
        /// Start of the window: a duration ago (6h, 3d) or an absolute local
        /// time (2026-08-04). Default 6h.
        #[arg(long, default_value = DEFAULT_SEARCH_SINCE)]
        since: String,
        /// End of the window, EXCLUSIVE — same spellings. Pair with an absolute
        /// `--since` to ask about ONE day rather than "the last N".
        #[arg(long)]
        until: Option<String>,
        #[arg(long)]
        regex: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Also query every trusted peer. Peers that fail or answer partially
        /// are always reported — a silent partial result would read as "this
        /// was never discussed anywhere".
        #[arg(long)]
        all: bool,
        /// Query ONE named peer (repeatable). An id prefix is enough — the same
        /// short form `tp peers` prints.
        ///
        /// Prefer this over `--all` once you have more than a handful of
        /// machines: a peer answers a search by scanning its whole corpus, so
        /// `--all` asks every trusted machine to do that. Naming them works at
        /// any number; `--all` refuses past a threshold.
        #[arg(long = "peer")]
        peers: Vec<String>,
    },
    /// List known sessions from the ARCHIVE, most-recently-active first —
    /// everything ever indexed, including sessions long ended and companion
    /// sessions other tools run (memory observers and the like), which on a
    /// busy machine can crowd out the ones you meant.
    ///
    /// To find a session you can MESSAGE right now, use `tp live` instead: it
    /// lists only what is currently running, with pid/tty/cwd. Reported by a
    /// session that reached for this command first and had its real work buried
    /// under twelve observer rows.
    Sessions {
        #[arg(long)]
        folder: Option<String>,
        /// Start of the window: a duration ago (7d) or an absolute local time
        /// (2026-08-04). Default 7d.
        #[arg(long, default_value = "7d")]
        since: String,
        /// End of the window, EXCLUSIVE — same spellings. With an absolute
        /// `--since`, this answers "which sessions were active THAT day".
        #[arg(long)]
        until: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Fetch turns for one session.
    Turns {
        /// Omit it with `--since`/`--folder` to read the most recent session in
        /// the window instead of naming one.
        session_id: Option<String>,
        /// Resume after this unix-ms timestamp (the universal cursor — see LLD §16 rule 1).
        /// Reads FORWARD and keeps the OLDEST turns if it overflows.
        #[arg(long)]
        after_ts: Option<i64>,
        /// Start of the window: a duration ago (`4h`, `2d`) or an absolute
        /// local time (`2026-08-04`, `2026-08-04T14:30`). Keeps the NEWEST
        /// turns if it overflows. Mutually exclusive with `--after-ts`.
        #[arg(long, conflicts_with = "after_ts")]
        since: Option<String>,
        /// End of the window, EXCLUSIVE — same spellings as `--since`. Pair it
        /// with an absolute `--since` to read one specific day; page BACKWARD by
        /// passing the earliest `ts` the previous page returned.
        #[arg(long, requires = "since")]
        until: Option<String>,
        /// Which session to read when `session_id` is omitted — folder name,
        /// path, or substring.
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        include_thinking: bool,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Enqueue a message into a session's mailbox (and wake it if reachable).
    Ask {
        session_id: String,
        /// The message body.
        ///
        /// QUOTING — use SINGLE quotes, or a quoted heredoc, whenever this
        /// contains code. Agents mostly send each other prose ABOUT code, so
        /// backticks, `$` and `!` are the common case, and inside a normal
        /// double-quoted argument the shell expands them BEFORE teleport sees
        /// anything: the message is then delivered in full, with a silently
        /// different body. Observed live — a backtick-quoted phrase became a
        /// command substitution, resolved to the empty string, and took the
        /// subject of its sentence with it. See README "Sending code in a
        /// message" for the heredoc form, which survives single quotes too.
        message: String,
        /// Don't attempt to wake the target pane — just park the message.
        #[arg(long)]
        no_wake: bool,
        /// THIS session's id, stamped on the message as the return address so
        /// the target can `tp reply` to it. Bare native id or composite both
        /// work. Defaults to `$CLAUDE_CODE_SESSION_ID` when set; without
        /// either, the message is one-way and the target is told so.
        #[arg(long)]
        from_session: Option<String>,
        /// Runtime of `--from-session` when that is a bare native id.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// Tell a session something WITHOUT asking it for anything.
    ///
    /// Same delivery as `ask` — it wakes the target, because a status update
    /// that arrives hours later is not much of an update — but the message is
    /// marked so the receiver is told plainly that no reply is expected, and
    /// this command does not tell you to wait for one.
    ///
    /// Use it for "I pushed the fix", "your build is green", "heads up, I
    /// changed X". Use `ask` when you need something back.
    Note {
        session_id: String,
        /// The message body — see `ask` for how to quote one containing code.
        message: String,
        /// Park it without waking the target.
        #[arg(long)]
        no_wake: bool,
        /// See `ask --from-session`.
        #[arg(long)]
        from_session: Option<String>,
        /// See `ask --runtime`.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// Answer a message from your inbox. The address comes from the original
    /// message, so a reply can't be misrouted the way a hand-addressed
    /// `tp ask` can — and it links the two, so a conversation is followable.
    Reply {
        /// Message id (the short form `tp inbox` prints is enough).
        message_id: String,
        /// The message body — see `ask` for how to quote one containing code.
        message: String,
        /// Don't attempt to wake the original sender.
        #[arg(long)]
        no_wake: bool,
        /// See `ask --from-session` — lets your answer be replied to in turn.
        #[arg(long)]
        from_session: Option<String>,
        /// See `ask --runtime`.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// Drain this session's mailbox (the `/tp inbox` control string, or a
    /// runtime extension's own equivalent command, triggers this).
    ///
    /// Draining marks a message READ, not ACKED — read means shown, ack means
    /// you confirm you finished acting on it (`tp ack`). A message shown but
    /// never acked is not gone: `--pending` recovers it, so a session
    /// interrupted mid-batch (context compaction, a crash) can find exactly
    /// what it left undone rather than having it vanish with the drain that
    /// showed it.
    Inbox {
        #[arg(long)]
        session_id: Option<String>,
        /// Runtime this session belongs to — only matters when `--session-id`
        /// is a BARE native id (needs composing); ignored for an
        /// already-composite id. Default matches the Claude Code hook path.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
        /// Show messages that were drained but never acked, instead of
        /// draining new ones. Read-only — checking this never counts as
        /// having handled anything, and it never marks anything itself.
        #[arg(long, conflicts_with = "history")]
        pending: bool,
        /// Show ACKED messages from the window instead of draining new ones —
        /// "what did that say again". Read-only.
        #[arg(long, conflicts_with = "pending")]
        history: bool,
        /// Window for `--history`: a duration ago (`4h`, `2d`) or an absolute
        /// local time. Ignored without `--history`.
        #[arg(long, default_value = "24h", requires = "history")]
        since: String,
    },
    /// Confirm you finished acting on a message from your inbox — the ack
    /// half of the read/ack split. `tp inbox` marks a message read the moment
    /// it is shown; that is NOT the same as having acted on it. Ack after you
    /// have actually done what the message asked (or decided a `[note]` needs
    /// no action) — an unacked message stays recoverable forever via
    /// `tp inbox --pending`, so acking is what tells teleport you are done
    /// with it, not what tells teleport you saw it.
    Ack {
        /// Message id (the short form `tp inbox` prints is enough).
        message_id: String,
    },
    /// Type text DIRECTLY into a tty's pane (tmux or iTerm2) — no mailbox, no
    /// control string, no confirmation gate on the receiving end unless the
    /// target process provides its own. This is NOT the safe `ask` path: use
    /// it only for a target with no `/tp inbox` (or runtime-native
    /// equivalent — see docs/pi-integration.md) to dereference through. Find
    /// the tty with `ps -o tty=,comm=` or the terminal's own window/tab
    /// title. Deliberately CLI-only — not exposed as an MCP tool, so this
    /// always requires a human to run it explicitly, never an agent invoking
    /// it unprompted.
    Type {
        /// e.g. ttys000 (with or without the /dev/ prefix)
        tty: String,
        message: String,
    },
    /// Register this session as live (SessionStart hook, or a runtime
    /// extension's session-start event). Writes pid + tty.
    Register {
        /// Native session id (the runtime's own id, NOT teleport's composite
        /// form — `tp` builds that itself). Required unless `--from-hook`.
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        /// Read `session_id`/`cwd` from the hook's stdin JSON payload instead
        /// of flags. Claude Code hooks pass event data ONLY on stdin, never
        /// as env vars — see docs/same-machine-poke-design.md §1a. Ignored
        /// with `--session-id` (used by non-Claude-Code runtime extensions,
        /// which have no stdin-JSON hook payload to read).
        #[arg(long)]
        from_hook: bool,
        /// Runtime this session belongs to — "claude_code" (default) or e.g.
        /// "pi" for a runtime extension registering itself directly.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
        /// Who owns this session's liveness. `scan` (default) lets teleport's
        /// process scan create and prune the row, which is right for a runtime
        /// it can see in `ps` with a tty. `declared` means the runtime owns it
        /// and renews by heartbeat — required for a harness the scan cannot
        /// observe (a web GUI with no tty, a host multiplexing many sessions
        /// onto one pid), whose rows the scan would otherwise prune within one
        /// interval. See docs/reach-provider.md.
        #[arg(long, default_value = "scan")]
        presence: String,
        /// This session's own process id. A `declared` runtime knows which
        /// process hosts it; teleport does not, and its fallback — walk up
        /// from `tp`'s own pid to the nearest registered ancestor — finds
        /// whatever launched the runtime instead. Observed live: a dsh host
        /// started from a Claude Code session registered every dsh session
        /// under the CLAUDE process's pid, which then made `tp ask` stamp the
        /// wrong sender and routed replies back to dsh itself.
        #[arg(long)]
        pid: Option<i32>,
        /// How to deliver a wake to this session, when there is no pane to type
        /// into: `exec:<argv>` (spawned, control string on stdin) or a LOOPBACK
        /// `http://127.0.0.1:<port>/<path>` (POSTed). Omit to infer a tmux pane
        /// or iTerm2 tty from the process, which is today's behaviour. Only the
        /// same fixed control string ever crosses either — never message
        /// content (LLD §7.3).
        #[arg(long)]
        deliver: Option<String>,
    },
    /// Accept turns a runtime PUSHES, for a harness teleport cannot usefully
    /// read from disk.
    ///
    /// The third ingest mode beside the declarative adapter (config) and a Rust
    /// `Adapter` (code): a runtime whose format is expensive to parse from
    /// outside — but which can host a plugin — hands normalized turns in
    /// instead. Reads a JSON array of turns on stdin.
    ///
    /// Its limit, stated rather than discovered: push only ever sees sessions
    /// created after the plugin was installed. It supplements disk reading; it
    /// does not replace it.
    Ingest {
        #[arg(long)]
        session_id: String,
        #[arg(long, default_value = "claude_code")]
        runtime: String,
        #[arg(long)]
        cwd: Option<String>,
        /// A title the RUNTIME states. Say which kind with `--title-source`;
        /// teleport's own truncated-first-message fallback is separate and is
        /// never written here.
        #[arg(long)]
        title: Option<String>,
        /// Who named the session: `user` (a person ran /rename, /name, or set
        /// session_info) or `ai` (a model generated it). Required to be explicit
        /// because the two have different precedence on read, and a pusher is
        /// the only one that knows which it has.
        #[arg(long, default_value = "user", value_parser = ["user", "ai"])]
        title_source: String,
    },
    /// Renew a `--presence declared` session's liveness. A runtime that owns its
    /// own presence calls this on a timer; miss enough of them and the session
    /// is marked stale, then evicted. Writes only the timestamp — never cwd,
    /// tty or channel, which registration owns.
    Heartbeat {
        #[arg(long)]
        session_id: String,
        /// See `register --runtime`.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// Unregister a live session (SessionEnd hook, or a runtime extension's
    /// session-shutdown event).
    Unregister {
        #[arg(long)]
        session_id: Option<String>,
        /// See `register --from-hook`.
        #[arg(long)]
        from_hook: bool,
        /// See `register --runtime`.
        #[arg(long, default_value = "claude_code")]
        runtime: String,
    },
    /// List Claude Code sessions currently live on THIS machine — reconciled
    /// by `tpd`'s active scan (docs/same-machine-poke-design.md follow-up),
    /// not just hook registrations, so it reflects reality even for sessions
    /// opened before the hooks existed. Requires `tpd` running (this command
    /// only reads what it's already reconciled — it doesn't scan itself).
    Live,
    /// Show this machine's identity — the fingerprint peers compare.
    Id,
    /// List machines we have a relationship with, and their trust state.
    Peers,
    /// Ask a host whether it runs a teleport daemon. Read-only: answering is
    /// not knowing, so nothing is trusted or stored for strangers.
    Discover {
        /// Host to probe, optionally `host:port`. A bare host is tried across
        /// the default port and its next few neighbours.
        host: String,
    },
    /// Pairing. Trust is only ever granted by `approve`, locally, by a human.
    #[command(subcommand)]
    Pair(PairCmd),
    /// Run an MCP server over stdio, exposing search/reach/federation as tools
    /// for an MCP client (e.g. Claude Code) to call directly.
    Mcp,
    /// This binary's build AND the daemon's, which are not the same question:
    /// installing binaries does not restart the LaunchAgent.
    Version,
    /// Move old sessions into an archive database, keeping this one small.
    ///
    /// Nothing is deleted: the archive is an ordinary teleport index, readable
    /// with `TP_DB=<path> tp search --index`.
    Archive {
        /// Archive sessions last active before this: a duration ago (30d) or an
        /// absolute local date (2026-07-01).
        #[arg(long)]
        before: String,
        /// Where to write it (default `~/.teleport/archive.db`). Reused and
        /// appended to if it already exists.
        #[arg(long)]
        to: Option<PathBuf>,
        /// Report what would move and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Check the index for damage, and say how much of it is irreplaceable.
    ///
    /// The index started as a cache of the transcript files. It is no longer
    /// only that: runtimes delete their transcripts (Claude Code after ~30
    /// days), so on this machine a third of all sessions exist nowhere else.
    Verify {
        /// Full page-by-page check instead of the fast structural one. Slower
        /// (~12s per GB here) and the only one that reads every page.
        #[arg(long)]
        full: bool,
    },
    /// Copy the index somewhere durable, consistently, while it is in use.
    ///
    /// `VACUUM INTO` rather than `cp`: copying a live SQLite file can capture a
    /// torn page set plus an unapplied WAL, which restores as a corrupt or
    /// silently stale database. This writes one consistent, compacted snapshot.
    Backup {
        /// Destination file. Refused if it already exists — an overwrite here
        /// destroys the previous backup, which is the thing being protected.
        dest: PathBuf,
    },
}

#[derive(Subcommand)]
enum PairCmd {
    /// Introduce this machine to a peer at `host:port`. Records it as
    /// pending on BOTH sides; neither is trusted until each approves.
    Request { addr: String },
    /// Pending and trusted peers, with the fingerprint to compare.
    List,
    /// Trust a peer. Compare its fingerprint out of band FIRST — this is the
    /// step that decides who may read this machine's sessions.
    Approve { device_id: String },
    /// Refuse a peer that has not been trusted yet, removing it. For a peer
    /// that IS trusted, use `revoke` — same effect, different mistake.
    Reject { device_id: String },
    /// Take back trust from a peer that currently has it, removing it.
    /// Purely local and immediate: the very next signed request from that
    /// device is refused, but it is not notified — there is no route for that.
    Revoke { device_id: String },
}

/// Accepts `5s`, `30m`, `6h`, `3d`, `2w`. A bare number is hours, which is the
/// useful default for a search window.
/// A point in time, written the way a person would.
///
/// `--since 4h` answers "recently" but its lower bound is always relative to
/// now, so asking for a specific past day meant pairing it with a hand-computed
/// `--before-ts` — and then the lower bound was still `now - 30d`, not that
/// day's start. A quiet day therefore returned the previous busy day's turns
/// with nothing to say the day itself was empty: a silently WRONG answer, which
/// is the failure this codebase treats as worse than an error.
///
/// So a bound accepts, in order of specificity:
///   `2026-08-04`         → local midnight that day
///   `2026-08-04T14:30`   → local wall-clock
///   `1786437984823`      → unix ms, for machine paging
///   `4h` / `2d`          → that long before `now`
fn parse_time_bound(s: &str, now_ms: i64) -> Result<i64> {
    let s = s.trim();
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return local_ms(d.and_hms_opt(0, 0, 0).unwrap(), s);
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return local_ms(dt, s);
        }
    }
    // Unix ms: only a bare integer this large is unambiguous against `4h`.
    if s.len() >= 10 && s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse().with_context(|| format!("bad timestamp {s:?}"));
    }
    Ok(now_ms - parse_duration(s)?.as_millis() as i64)
}

/// Interpreted in the machine's zone, matching how `fmt_ts` PRINTS timestamps —
/// a date typed after reading local-time output must mean the same day.
fn local_ms(dt: chrono::NaiveDateTime, orig: &str) -> Result<i64> {
    use chrono::TimeZone;
    match chrono::Local.from_local_datetime(&dt).earliest() {
        Some(t) => Ok(t.timestamp_millis()),
        // A DST spring-forward gap has no such local time. Saying so beats
        // silently shifting the window by an hour.
        None => anyhow::bail!("{orig:?} does not exist in this timezone (DST gap)"),
    }
}

fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: u64 = num.parse().with_context(|| format!("bad duration {s:?}"))?;
    // Checked, and the overflow is an ERROR rather than a clamp. Unchecked,
    // release builds (which is what install.sh ships) wrapped silently and
    // handed back an arbitrary window — a search that looks like it covered
    // "everything" and covered whatever the wrap landed on. Debug builds
    // panicked instead, so the two profiles disagreed about the same input.
    //
    // Clamping to a huge window would be the other wrong answer: a caller that
    // typed a nonsense duration wants to be told, not quietly given a different
    // question's results.
    let mul = |per: u64| {
        n.checked_mul(per)
            .with_context(|| format!("duration {s:?} is too large"))
    };
    let secs = match unit {
        "s" => n,
        "m" => mul(60)?,
        "h" | "" => mul(3600)?,
        "d" => mul(86400)?,
        "w" => mul(604800)?,
        other => anyhow::bail!("unknown duration unit {other:?} (use s/m/h/d/w)"),
    };
    Ok(std::time::Duration::from_secs(secs))
}

#[cfg(test)]
mod duration_tests {
    /// Release builds ship with overflow checks OFF, so an unchecked multiply
    /// here wrapped silently and answered a different question than the one
    /// asked, while debug builds panicked on the same input — two profiles
    /// disagreeing about one string. It is an error now, in both.
    #[test]
    fn a_duration_too_large_to_represent_is_refused_not_wrapped() {
        for s in [
            "999999999999999999w",
            "999999999999999999d",
            "18446744073709551615m",
        ] {
            let err = parse_duration(s).unwrap_err().to_string();
            assert!(err.contains("too large"), "{s}: {err}");
        }
    }

    /// And the range a person could mean still works — the guard must not have
    /// been bought by refusing legitimate windows.
    #[test]
    fn ordinary_durations_still_parse() {
        assert_eq!(parse_duration("30s").unwrap().as_secs(), 30);
        assert_eq!(parse_duration("4h").unwrap().as_secs(), 4 * 3600);
        assert_eq!(
            parse_duration("100000d").unwrap().as_secs(),
            100_000 * 86400
        );
    }

    use super::parse_duration;

    #[test]
    fn units() {
        assert_eq!(parse_duration("5s").unwrap().as_secs(), 5);
        assert_eq!(parse_duration("30m").unwrap().as_secs(), 1800);
        assert_eq!(parse_duration("6h").unwrap().as_secs(), 21600);
        assert_eq!(parse_duration("3d").unwrap().as_secs(), 259200);
        assert_eq!(parse_duration("2w").unwrap().as_secs(), 1209600);
        assert_eq!(
            parse_duration("6").unwrap().as_secs(),
            21600,
            "bare number is hours"
        );
        assert!(parse_duration("5y").is_err());
        assert!(parse_duration("abc").is_err());
    }
}

fn db_path() -> PathBuf {
    tp_db::default_db_path()
}

/// This machine's id — the address prefix every `session.id` here carries
/// (LLD §4), AND the device identity peers verify signatures against (§8.2).
///
/// These are deliberately ONE value. An earlier version had two: a random
/// UUID minted here for session addressing, and `tp-net`'s blake3 key
/// fingerprint for peer identity. Nothing connected them, so both would write
/// `is_self = 1` into `machine` and every cross-machine `session.id` would be
/// unresolvable — the two ids named the same Mac but never agreed on what it
/// was called. The fingerprint wins because it is *derived from the keypair*:
/// a peer that verifies a signature has thereby verified the id, and a human
/// comparing fingerprints out of band is comparing something load-bearing.
/// A random UUID can prove nothing about itself.
///
/// Safe as a `SessionId` segment: base32 (`A-Z2-7`) plus `-` grouping never
/// contains the `/` that `SessionId::parse` splits on (asserted in tests).
fn machine_id() -> Result<String> {
    Ok(identity()?.device_id)
}

fn identity() -> Result<tp_net::Identity> {
    tp_net::Identity::load_or_create(&tp_net::identity::default_key_path())
        .context("load or create device identity")
}

fn hostname() -> String {
    tp_net::identity::hostname()
}

/// Render a stored timestamp in the LOCAL timezone.
///
/// Storage is unambiguous already — every `ts` is unix milliseconds, so there
/// is nothing to fix there. The bug was purely presentational:
/// `from_timestamp_millis` yields a `DateTime<Utc>`, and formatting it without
/// a timezone marker printed UTC in a shape indistinguishable from local time.
/// On a machine 7 hours off UTC that made `tp live` report a session "last
/// seen" at a wall-clock time that hadn't happened yet, and cost a real
/// debugging detour: an `--after-ts` computed from local time silently matched
/// nothing because it was 7 hours in the future.
///
/// Local without a marker is deliberate, matching `ls -l` and every other
/// local-machine tool: the timestamps exist to be cross-referenced against the
/// user's own clock, and a `PDT` suffix on every row of a listing is noise.
fn fmt_ts(ts: Option<i64>) -> String {
    match ts {
        Some(ms) => chrono::DateTime::from_timestamp_millis(ms)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| "-".to_string()),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod readme_tests {
    use super::{window_scope, Cli};
    use clap::Parser;

    /// Every `tp …` line in the README must at least PARSE.
    ///
    /// The README shipped `tp turns --since 2026-08-04 --until 2026-08-05`,
    /// which failed at runtime with "bad duration". It survived a hand check
    /// because the check ran the neighbouring examples — the duration form of
    /// this one, and the absolute form of the other two commands — but not this
    /// exact combination. Enumerating from the file removes the judgement call
    /// about which combinations are worth trying.
    fn readme_tp_commands() -> Vec<Vec<String>> {
        let readme = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../README.md"),
        )
        .expect("README.md");
        readme
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("tp ") && !l.contains('|'))
            .map(|l| {
                // Drop trailing `# comment`, then split on whitespace.
                let cmd = l.split('#').next().unwrap_or(l).trim();
                shell_words(cmd)
            })
            .collect()
    }

    /// Minimal splitter: honours double quotes, which is all the README uses.
    fn shell_words(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_q = false;
        for c in s.chars() {
            match c {
                '"' => in_q = !in_q,
                c if c.is_whitespace() && !in_q => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    #[test]
    fn every_readme_example_parses() {
        let cmds = readme_tp_commands();
        assert!(cmds.len() >= 8, "expected README examples, found {cmds:?}");
        for argv in cmds {
            if let Err(e) = Cli::try_parse_from(&argv) {
                panic!("README example does not parse: `{}`\n{e}", argv.join(" "));
            }
        }
    }

    /// Parsing is not enough — `--since 2026-08-04` parsed fine as a String and
    /// then blew up converting it. Anything the README spells as a time bound
    /// must survive the conversion the command actually performs.
    #[test]
    fn readme_time_bounds_convert() {
        for argv in readme_tp_commands() {
            let val = |flag: &str| {
                argv.iter()
                    .position(|a| a == flag)
                    .and_then(|i| argv.get(i + 1))
                    .cloned()
            };
            let (Some(since), until) = (val("--since"), val("--until")) else {
                continue;
            };
            window_scope(None, &since, until.as_deref()).unwrap_or_else(|e| {
                panic!("README bound rejected: --since {since} --until {until:?}\n{e}")
            });
        }
    }
}

#[cfg(test)]
mod time_bound_tests {
    use super::parse_time_bound;

    const NOW: i64 = 1_786_437_984_823;

    #[test]
    fn duration_is_relative_to_now() {
        assert_eq!(parse_time_bound("4h", NOW).unwrap(), NOW - 4 * 3_600_000);
        assert_eq!(parse_time_bound("2d", NOW).unwrap(), NOW - 2 * 86_400_000);
    }

    /// A bare date must mean LOCAL midnight, because that is the zone the
    /// timestamps are printed in — a date typed after reading the output has to
    /// select the day the reader saw.
    #[test]
    fn a_date_is_local_midnight() {
        use chrono::TimeZone;
        let got = parse_time_bound("2026-08-04", NOW).unwrap();
        let want = chrono::Local
            .with_ymd_and_hms(2026, 8, 4, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(got, want);
    }

    /// The regression this whole flag exists for: a day is a RANGE. Without an
    /// upper bound a quiet day returns an earlier busy day's turns and looks
    /// like an answer.
    #[test]
    fn a_day_is_a_bounded_range() {
        let start = parse_time_bound("2026-08-04", NOW).unwrap();
        let end = parse_time_bound("2026-08-05", NOW).unwrap();
        assert_eq!(end - start, 86_400_000, "one day apart");
        assert!(start < end);
    }

    #[test]
    fn wall_clock_and_epoch_ms_both_parse() {
        let day = parse_time_bound("2026-08-04", NOW).unwrap();
        let noon = parse_time_bound("2026-08-04T12:00", NOW).unwrap();
        assert_eq!(noon - day, 12 * 3_600_000);
        assert_eq!(parse_time_bound("1786437984823", NOW).unwrap(), NOW);
    }

    /// `4h` and `1786437984823` must not be confusable — the digit-length rule
    /// is what keeps a duration from being read as an epoch.
    #[test]
    fn short_digits_are_not_mistaken_for_epoch_ms() {
        // "6" with no unit is hours (parse_duration's default), not 6ms.
        assert_eq!(parse_time_bound("6", NOW).unwrap(), NOW - 6 * 3_600_000);
    }

    #[test]
    fn garbage_is_an_error_not_a_silent_default() {
        assert!(parse_time_bound("last tuesday", NOW).is_err());
        assert!(parse_time_bound("2026-13-99", NOW).is_err());
    }
}

#[cfg(test)]
mod fmt_ts_tests {
    use super::fmt_ts;

    /// The regression: rendering must follow the machine's clock, not UTC.
    /// Comparing against a locally-computed expectation rather than a hardcoded
    /// string keeps this true on any runner, in any zone, including CI's UTC —
    /// where the bug is invisible because local IS UTC.
    #[test]
    fn renders_in_local_time_not_utc() {
        let ms = 1_786_150_093_693i64;
        let expected = chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert_eq!(fmt_ts(Some(ms)), expected);

        // And on a machine that is actually offset from UTC, the two must
        // differ — otherwise this test would pass against the old bug too.
        let utc = chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let offset_secs = chrono::Local::now().offset().local_minus_utc();
        if offset_secs != 0 {
            assert_ne!(fmt_ts(Some(ms)), utc, "off-UTC machine must not render UTC");
        }
    }

    #[test]
    fn missing_timestamp_is_a_dash() {
        assert_eq!(fmt_ts(None), "-");
    }
}

fn main() -> Result<()> {
    // Before any output: a CLI whose stdout is piped into `head` must exit, not
    // panic. Rust ignores SIGPIPE by default and turns the failed write into a
    // panic; this hands the decision back to the kernel.
    tp_core::exit_quietly_on_broken_pipe();
    // Names this process in any log line a library emits below. The CLI's own
    // output stays `println!` on stdout — that is the product, not logging.
    tp_core::logging::set_service("tp");
    let cli = Cli::parse();
    match cli.cmd {
        Command::Index => run_index(),
        Command::Reindex { dry_run, runtime } => run_reindex(dry_run, runtime),
        Command::Search {
            query,
            include_thinking,
            folder,
            since,
            until,
            regex,
            limit,
            all,
            peers,
        } => run_search(
            cli.index,
            &query,
            include_thinking,
            folder,
            &since,
            until.as_deref(),
            regex,
            limit,
            all,
            &peers,
        ),
        Command::Live => run_live(),
        Command::Id => run_id(),
        Command::Peers => run_peers(),
        Command::Discover { host } => run_discover(&host),
        Command::Pair(p) => match p {
            PairCmd::Request { addr } => run_pair_request(&addr),
            PairCmd::List => run_pair_list(),
            PairCmd::Approve { device_id } => run_pair_decide(&device_id, true),
            PairCmd::Reject { device_id } => run_pair_decide(&device_id, false),
            PairCmd::Revoke { device_id } => run_pair_revoke(&device_id),
        },
        Command::Sessions {
            folder,
            since,
            until,
            limit,
        } => run_sessions(cli.index, folder, &since, until.as_deref(), limit),
        Command::Turns {
            session_id,
            after_ts,
            since,
            until,
            folder,
            include_thinking,
            limit,
        } => run_turns(
            cli.index,
            session_id,
            after_ts,
            since,
            until,
            folder,
            include_thinking,
            limit,
        ),
        Command::Ask {
            session_id,
            message,
            no_wake,
            from_session,
            runtime,
        } => run_ask(
            &session_id,
            &message,
            no_wake,
            from_session.as_deref(),
            &runtime,
            tp_app::Kind::Ask,
        ),
        Command::Note {
            session_id,
            message,
            no_wake,
            from_session,
            runtime,
        } => run_ask(
            &session_id,
            &message,
            no_wake,
            from_session.as_deref(),
            &runtime,
            tp_app::Kind::Note,
        ),
        Command::Reply {
            message_id,
            message,
            no_wake,
            from_session,
            runtime,
        } => run_reply(
            &message_id,
            &message,
            no_wake,
            from_session.as_deref(),
            &runtime,
        ),
        Command::Inbox {
            session_id,
            runtime,
            pending,
            history,
            since,
        } => run_inbox(session_id, &runtime, pending, history, &since),
        Command::Ack { message_id } => run_ack(&message_id),
        Command::Type { tty, message } => run_type(&tty, &message),
        Command::Register {
            session_id,
            cwd,
            from_hook,
            runtime,
            presence,
            deliver,
            pid,
        } => run_register(
            session_id,
            cwd,
            from_hook,
            &runtime,
            &presence,
            deliver.as_deref(),
            pid,
        ),
        Command::Ingest {
            session_id,
            runtime,
            cwd,
            title,
            title_source,
        } => run_ingest(
            &session_id,
            &runtime,
            cwd.as_deref(),
            title.as_deref(),
            &title_source,
        ),
        Command::Heartbeat {
            session_id,
            runtime,
        } => run_heartbeat(&session_id, &runtime),
        Command::Unregister {
            session_id,
            from_hook,
            runtime,
        } => run_unregister(session_id, from_hook, &runtime),
        Command::Mcp => mcp::serve(),
        Command::Version => run_version(),
        Command::Archive {
            before,
            to,
            dry_run,
        } => run_archive(before, to, dry_run),
        Command::Verify { full } => run_verify(full),
        Command::Backup { dest } => run_backup(&dest),
    }
}
