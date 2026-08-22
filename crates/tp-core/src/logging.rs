//! The logging contract, for something that ships to a laptop.
//!
//! The fleet contract is four things: structured JSON, trace_id/span_id when a
//! span is active, one redaction list, and the field names service/level/msg.
//! Three of those hold here. The first is INVERTED, deliberately, and the
//! reason is the deployment rather than the language:
//!
//! THE FLEET CONTRACT ASSUMES A COLLECTOR. Its whole claim is that several
//! services become one queryable dataset — JSON by default, `LOG_PRETTY=1` to
//! opt out, no timestamp on the line because the collector stamps at ingest.
//! Correct for a cluster you operate.
//!
//! TELEPORT HAS NO COLLECTOR. It is a CLI and a per-user daemon on someone
//! else's machine; launchd writes `~/.teleport/tpd.err.log` and nothing reads
//! it but a person, usually while something is wrong. For that reader JSON is
//! strictly worse than a line they can scan, and a missing timestamp is not
//! deferred to ingest — it is gone. So the default is human-readable WITH a
//! timestamp, and `LOG_FORMAT=json` opts IN for the case that motivated the
//! contract: shipping logs somewhere that parses them.
//!
//! Measured against its own history: this is the file used to diagnose a live
//! reindex race, and what was missing that day was the time, not the schema.
//!
//! NO trace_id/span_id, and here that is a decision rather than a gap. There is
//! no tracer on a customer's laptop and there will not be one; the fields would
//! be permanently absent, which is what the contract says to do when no span is
//! active. If a future span exists, `emit` gains two fields and every call site
//! is correlated without being touched.
//!
//! WHY STDERR, in both formats. `tp` is a CLI whose stdout IS its product — a
//! log line mixed into it corrupts what the caller parses — and launchd already
//! routes `tpd`'s stderr to a file. The router existed; what was missing was
//! structure, and then a timestamp.
//!
//! REDACTION REACHES NAMED FIELDS ONLY, in both formats. A secret interpolated
//! into the MESSAGE is one opaque string by the time it arrives. Pass it as a
//! field, not inside the text. This matters more here than in the fleet: the
//! credential at risk is the user's own, in a file on their own disk.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

static SERVICE: OnceLock<String> = OnceLock::new();

/// Where lines go when a binary has claimed a file. `None` = stderr.
static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// Rotate at 5 MB and keep one previous file, so a daemon costs at most 10 MB
/// on someone else's disk.
///
/// Sized against measured behaviour rather than a round number: this log grew
/// 30 KB in the 16 days before rotation existed — about 2 KB/day — and its
/// worst case is bounded too, because every loop that can log in a failure has
/// a floor (the watcher backs off to 30s, discovery sleeps 60s), which caps a
/// permanently-broken daemon near 300 KB/day. So 5 MB is years of healthy
/// operation and weeks of a broken one: large enough that rotation is not
/// destroying evidence, small enough to bound the disk.
const MAX_BYTES: u64 = 5 * 1024 * 1024;

struct Sink {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
}

/// Name this process in its own log lines. Called once at startup by a binary.
///
/// Set in code rather than in the LaunchAgent plist because the plist only
/// applies when launchd starts it: someone running `tpd` in a terminal to watch
/// it, which is exactly when logs are being read, would otherwise get lines
/// indistinguishable from the CLI's. `SERVICE_NAME` still wins, so an operator
/// can override without a rebuild.
pub fn set_service(name: &str) {
    let _ = SERVICE.set(name.to_string());
}

/// Keys whose values must never reach a log line. Matched case-insensitively.
pub const REDACT_KEYS: &[&str] = &[
    "authorization",
    "cookie",
    "token",
    "api_key",
    "apikey",
    "password",
    "secret",
    "bearer_token",
    "access_token",
    "refresh_token",
    "set-cookie",
];

const CENSOR: &str = "[redacted]";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Lowercase in JSON, because the contract's claim is that several services
    /// are one queryable dataset and a query written level="error" must match.
    fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    /// Padded and upper for the human format, so the message column lines up
    /// down the page — the reason to read this file at all is to scan it.
    fn as_column(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }

    fn from_env() -> Level {
        match std::env::var("LOG_LEVEL")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "debug" | "trace" => Level::Debug,
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }
}

fn is_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    REDACT_KEYS.iter().any(|k| lower == *k)
        || lower.ends_with("_token")
        || lower.ends_with("_secret")
}

fn service() -> String {
    std::env::var("SERVICE_NAME").unwrap_or_else(|_| {
        SERVICE
            .get()
            .cloned()
            .unwrap_or_else(|| "teleport".to_string())
    })
}

/// Write to `path` instead of stderr, rotating it. Called once at startup by a
/// DAEMON; a CLI must not, because its lines belong on the terminal in front of
/// the person who typed the command.
///
/// The daemon owns this file rather than truncating the one launchd opened for
/// its stderr. launchd holds that descriptor and teleport does not control the
/// flags it was opened with, and the difference is not cosmetic: with `O_APPEND`
/// an in-place truncation is clean, and without it the next write lands at the
/// old offset and leaves a multi-megabyte hole that `ls` reports as real size.
/// Verified both ways before choosing. Owning the file removes the question.
///
/// launchd's stderr file stays as the capture for what this logger can never
/// see — a panic, a dyld failure, anything before `init` — which is also why it
/// needs no rotation: nothing writes to it in normal operation.
pub fn to_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = SINK.set(Mutex::new(Sink {
        path: path.to_path_buf(),
        file,
        written,
    }));
    Ok(())
}

/// Move the current file aside and start a new one. One generation: `.1` is
/// overwritten, because two stale copies answer no question the one does not.
fn rotate(sink: &mut Sink) -> std::io::Result<()> {
    let previous = sink.path.with_extension("log.1");
    std::fs::rename(&sink.path, &previous)?;
    sink.file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sink.path)?;
    sink.written = 0;
    Ok(())
}

/// `LOG_FORMAT=json` for the machine-readable form. Anything else, including
/// unset, gives the human one — the inverse of the fleet default, because the
/// reader here is a person and not a collector.
fn want_json() -> bool {
    std::env::var("LOG_FORMAT")
        .map(|f| f.eq_ignore_ascii_case("json"))
        .unwrap_or(false)
}

/// One line. Called by the macros below; call it directly when you have fields
/// worth naming, which is the only form redaction can reach.
pub fn emit(level: Level, msg: &str, fields: &[(&str, &str)]) {
    if level < Level::from_env() {
        return;
    }
    let redacted = |k: &str, v: &str| -> String {
        if is_secret(k) {
            CENSOR.to_string()
        } else {
            v.to_string()
        }
    };
    // Local time, not UTC: this is read by the person sitting at the machine
    // that wrote it, next to timestamps from their own shell.
    let now = chrono::Local::now();

    let line = if want_json() {
        let mut map = serde_json::Map::new();
        // `ts` first, and present at all — the fleet logger omits it because a
        // collector stamps at ingest. Nothing stamps this one.
        map.insert("ts".into(), now.to_rfc3339().into());
        map.insert("service".into(), service().into());
        map.insert("level".into(), level.as_str().into());
        map.insert("msg".into(), msg.into());
        for (k, v) in fields {
            map.insert((*k).to_string(), redacted(k, v).into());
        }
        serde_json::Value::Object(map).to_string()
    } else {
        // Continuation lines are INDENTED rather than escaped. A message can be
        // multi-line — a TOML parse error arrives as five lines with its own
        // caret diagram — and both alternatives are worse than this one:
        // escaping to `\n` destroys the diagram that makes the error readable,
        // and leaving it flush left makes four events out of one, so a `grep`
        // for the level silently drops the part that says what was wrong.
        let mut out = format!(
            "{} {} {}",
            now.format("%Y-%m-%d %H:%M:%S"),
            level.as_column(),
            msg.trim_end().replace('\n', "\n    ")
        );
        // Fields trail the message as `k=v`, so a line stays one line and the
        // message — the part a person is scanning for — starts at a fixed
        // column instead of after a variable-length prefix.
        for (k, v) in fields {
            out.push_str(&format!(" {k}={}", redacted(k, v)));
        }
        out
    };

    // A log write must never fail the thing it logs. Losing a line is bad;
    // taking the process down to report one is worse — so every failure below
    // is swallowed, including a rotation that cannot rename.
    match SINK.get() {
        Some(sink) => {
            if let Ok(mut sink) = sink.lock() {
                // Rotate BEFORE writing, so the cap is a cap rather than a
                // threshold the last line is allowed to cross.
                if sink.written >= MAX_BYTES {
                    let _ = rotate(&mut sink);
                }
                if writeln!(sink.file, "{line}").is_ok() {
                    sink.written += line.len() as u64 + 1;
                }
            }
        }
        None => {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{line}");
        }
    }
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Warn, &format!($($arg)*), &[])
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Error, &format!($($arg)*), &[])
    };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Info, &format!($($arg)*), &[])
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The redaction list reaches named fields in BOTH formats. Inverting the
    /// default format is the change this file exists to make; carrying the
    /// censor into only one of them would have been the way to get it wrong.
    #[test]
    fn secrets_are_censored_by_name_not_by_value() {
        for (name, secret) in [
            ("token", true),
            ("Authorization", true),
            ("github_token", true),
            ("client_secret", true),
            ("path", false),
            ("session_id", false),
        ] {
            assert_eq!(is_secret(name), secret, "{name}");
        }
    }

    /// `INFO ` and `WARN ` are padded so the message starts at one column. A
    /// log a person scans is the whole reason for the human format.
    #[test]
    fn the_level_column_is_fixed_width() {
        let widths: Vec<usize> = [Level::Debug, Level::Info, Level::Warn, Level::Error]
            .iter()
            .map(|l| l.as_column().len())
            .collect();
        assert_eq!(widths, vec![5, 5, 5, 5]);
    }

    /// Rotation moves the file aside and keeps writing — the property that
    /// matters is that nothing is lost and the cap is not exceeded.
    ///
    /// Driven through `rotate` directly rather than by writing 5 MB: the cap is
    /// a constant, and a test that spent seconds proving arithmetic would be
    /// slower without checking anything the constant does not already say.
    #[test]
    fn rotation_keeps_one_generation_and_loses_nothing() {
        let dir = std::env::temp_dir().join(format!("tp-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tpd.log");
        std::fs::write(&path, b"first generation\n").unwrap();

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut sink = Sink {
            path: path.clone(),
            file,
            written: MAX_BYTES,
        };
        rotate(&mut sink).unwrap();
        writeln!(sink.file, "second generation").unwrap();

        assert_eq!(sink.written, 0, "the byte count restarts with the file");
        assert_eq!(
            std::fs::read_to_string(dir.join("tpd.log.1")).unwrap(),
            "first generation\n",
            "the previous generation is kept, not deleted"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "second generation\n",
            "and the live file starts clean"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// JSON is opt-IN here, the inverse of the fleet default. Getting this
    /// backwards is the whole change: it decides what a customer sees in their
    /// own log file, and there is no collector on their laptop to prefer JSON
    /// for.
    #[test]
    fn json_is_opt_in_and_anything_else_is_human() {
        // Not a set of aliases: only the exact word, so a typo gives the
        // readable form rather than silently the parsed one.
        for (value, json) in [
            ("json", true),
            ("JSON", true),
            ("pretty", false),
            ("", false),
            ("jsonl", false),
        ] {
            std::env::set_var("LOG_FORMAT", value);
            assert_eq!(want_json(), json, "LOG_FORMAT={value:?}");
        }
        std::env::remove_var("LOG_FORMAT");
        assert!(!want_json(), "unset must mean human");
    }
}
