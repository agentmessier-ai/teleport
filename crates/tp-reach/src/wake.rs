//! The security-critical injection path (LLD §7.3): `wake()` only ever types
//! a fixed control string into another session's pane. Message content stays
//! in the DB; the target reads it via `/tp inbox`. A hostile peer can at most
//! make the target *check its inbox* — never execute arbitrary content.
//!
//! `type_raw()` is the escape hatch for targets with no `/tp inbox`
//! equivalent (a different agent CLI that just reads keyboard input) — see
//! its own doc. It has none of the above guarantee; use it deliberately.

use crate::resolve::Target;
use crate::terminal;
use anyhow::{bail, Context, Result};

/// The default phrase typed into a peer pane, for a runtime that declares none.
///
/// No longer the ONLY one, and the reason is worth stating: `/tp inbox` is a
/// slash command that Claude Code and pi both provide, so it read like a
/// property of teleport when it was really a property of those two runtimes.
/// codex has no such command and answers `Unrecognized command '/tp'` — the
/// wake arrives, and dies at the last inch. Found by watching a real codex
/// session receive one.
///
/// A runtime may now declare its own phrase in its descriptor, and the CALLER
/// resolves it: knowing what a runtime understands is runtime knowledge, and
/// `tp-reach` deliberately holds none — it depends only on `tp-db`. Threading
/// it in as a parameter is the seam saying so.
///
/// LLD §7.3 is unchanged. The invariant is that MESSAGE CONTENT never crosses a
/// pane. This value comes from an operator-controlled descriptor, is selected by
/// the target's runtime id, and no part of it derives from the request.
pub const CONTROL_STRING: &str = "/tp inbox";

/// Who's calling `wake()` — decides which backends are even attempted.
///
/// `Target::ITerm` requires the AppleScript-driving process to hold (or
/// inherit) the Automation TCC grant. A CLI process launched inside a
/// terminal — `tp ask`, or `tp mcp` spawned by Claude Code — inherits that
/// grant through the responsible-process chain (verified live: an
/// `osascript` call from a Bash tool inside iTerm2 controlling a DIFFERENT
/// iTerm2 session succeeded with no prompt). `tpd`, a bare LaunchAgent, has
/// no such chain and gets a hard, unpredictable `-1743` (LLD §7.4) — so that
/// path must never even attempt it. This is expressed here as a hard
/// precondition, not left to be discovered as a runtime failure in the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// A CLI process invoked directly or as a subprocess of the terminal
    /// (`tp`, `tp mcp`). May use ANY backend, including iTerm2 AppleScript.
    Cli,
    /// `tpd`, the background LaunchAgent. TCC-blocked from AppleScript by
    /// construction (LLD §7.4) — restricted to TCC-free backends only.
    Daemon,
}

/// Type the control string into the target. `Target::Unreachable` /
/// `Target::NotLive` are no-ops (mailbox-only delivery).
pub fn wake(target: &Target, session_id: &str, control: &str, caller: Caller) -> Result<()> {
    type_text(target, control, session_id, caller)
}

/// Type ARBITRARY text directly into the target pane. This is NOT the safe
/// path — it exists only for targets that have no equivalent of `/tp inbox`
/// to dereference through (e.g. another agent CLI that reads raw keyboard
/// input with no mailbox/control-string concept of its own). Whatever you
/// pass here lands in the target's input exactly as if typed, with none of
/// `wake()`'s guarantee that only a fixed, non-attacker-controlled string
/// ever crosses the pane boundary (LLD §7.3). Only call this for a target
/// you'd be comfortable handing a keyboard to directly — there is no
/// confirmation gate on the receiving end unless the target process
/// happens to provide its own.
pub fn type_raw(target: &Target, text: &str, caller: Caller) -> Result<()> {
    // No session id: `type_raw` is the pane-only escape hatch and never targets
    // a channel, whose whole purpose is addressing one session.
    type_text(target, text, "", caller)
}

/// Hand the control string to a runtime-declared channel.
///
/// Deliberately hand-rolled rather than pulling an HTTP client into this crate:
/// the request is one fixed shape to a loopback address, and `reqwest` would
/// drag an async runtime into a synchronous crate for it. `DeliveryChannel::parse`
/// has already refused anything non-loopback.
fn deliver_via_channel(
    chan: &crate::resolve::DeliveryChannel,
    text: &str,
    session_id: &str,
) -> Result<()> {
    use crate::resolve::DeliveryChannel;
    use std::io::Write;

    match chan {
        DeliveryChannel::Exec(argv) => {
            let mut child = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                // The receiver has to know WHICH session to drain; a
                // multiplexed host serves many at once.
                .arg(session_id)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .with_context(|| format!("spawning delivery channel {:?}", argv[0]))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes())?;
            }
            // Wait: a channel that fails must surface as a failed wake, not as a
            // silently orphaned process reported as success.
            let status = child.wait()?;
            if !status.success() {
                bail!("delivery channel {:?} exited with {status}", argv[0]);
            }
            Ok(())
        }
        DeliveryChannel::Http(url) => {
            let (host, port, path) = split_loopback_url(url)?;
            let body = format!(
                "{{\"session_id\":{},\"control\":{}}}",
                json_string(session_id),
                json_string(text)
            );
            let req = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let addr = format!("{host}:{port}");
            let mut stream = std::net::TcpStream::connect(&addr)
                .with_context(|| format!("connecting to delivery channel {addr}"))?;
            // Bounded: a wedged listener must not hang the daemon's delivery loop.
            stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
            stream.write_all(req.as_bytes())?;
            let mut head = [0u8; 15]; // enough for "HTTP/1.1 XXX "
            use std::io::Read;
            let n = stream.read(&mut head).unwrap_or(0);
            let status_line = String::from_utf8_lossy(&head[..n]);
            // Treat any 2xx as delivered; the body is not part of the contract.
            if !status_line
                .split(' ')
                .nth(1)
                .is_some_and(|c| c.starts_with('2'))
            {
                bail!("delivery channel {url} answered {:?}", status_line.trim());
            }
            Ok(())
        }
    }
}

/// `http://127.0.0.1:8125/path` → `("127.0.0.1", 8125, "/path")`.
fn split_loopback_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| anyhow::anyhow!("not an http url: {url}"))?;
    let (hostport, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = hostport
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("delivery channel needs an explicit port: {url}"))?;
    Ok((
        host.to_string(),
        port.parse().with_context(|| format!("bad port in {url}"))?,
        format!("/{path}"),
    ))
}

/// Minimal JSON string escaping — the payload is a compile-time control string,
/// but escaping it is one line and removes the question entirely.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn type_text(target: &Target, text: &str, session_id: &str, caller: Caller) -> Result<()> {
    match target {
        Target::Tmux(pane) => {
            std::process::Command::new("tmux")
                .args(["send-keys", "-t", pane, text, "Enter"])
                .status()?;
        }
        // A declared channel has no TCC dimension — it is a subprocess spawn or a
        // loopback socket write, not AppleScript — so unlike `ITerm` it is
        // reachable from `tpd` as well as from a CLI process. That is a
        // capability gain for harnesses that declare one, not a loophole in the
        // guard below.
        Target::Channel(chan) => deliver_via_channel(chan, text, session_id)?,
        Target::Terminal { id, tty } => {
            if caller == Caller::Daemon {
                // Fail loud, not silently-degrade: a daemon-side caller
                // reaching this branch is a bug in target resolution or
                // caller wiring, not a normal degraded outcome.
                bail!("refusing AppleScript injection from the daemon path (TCC-blocked by design — see docs/same-machine-poke-design.md Gap 2)");
            }
            terminal_write_text(id, tty, text)?;
        }
        _ => {}
    }
    Ok(())
}

/// Type `text` into whichever object the `id` terminal owns for `tty`.
///
/// The script is GENERATED from that terminal's descriptor
/// (`terminal::TerminalConfig::applescript_send`) rather than written here —
/// this used to be an iTerm2 literal, which is why iTerm2 was the only
/// terminal that could ever be woken. The quoting hazards it carried live in
/// `terminal` now, with tests: `tell s to write text "…"` and NOT
/// `write text "…" to s`, which is a syntax error (`-2741`, LLD §7.2).
///
/// Accepts either `ttysNNN` or `/dev/ttysNNN` — `terminal::needle` normalizes,
/// because `tty of s` always answers the `/dev/...` form.
fn terminal_write_text(id: &str, tty: &str, text: &str) -> Result<()> {
    let Some(cfg) = terminal::all().into_iter().find(|c| c.id == id) else {
        // The descriptor was there at resolve time and is not now — a config
        // file edited mid-flight. Say which one, rather than reporting this as
        // the terminal having closed.
        bail!("terminal backend {id:?} is no longer configured");
    };
    let Some(script) = cfg.applescript_send(tty, text) else {
        bail!("terminal backend {id:?} cannot inject (not an applescript backend)");
    };
    let out = std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()?;
    if !out.status.success() {
        bail!("osascript failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    if String::from_utf8_lossy(&out.stdout).trim() != "ok" {
        bail!("no {id} window found with tty {tty} (closed since resolve?)");
    }
    Ok(())
}
