//! Terminal backends, declared rather than compiled in.
//!
//! Injection is how teleport wakes an agent that offers no API of its own: it
//! types the control string into the terminal, exactly as a person would. That
//! is the whole reason it works on an UNCOOPERATIVE harness — the alternative,
//! a runtime-declared channel (`resolve::DeliveryChannel`, which dsh uses), is
//! the better mechanism but requires the harness to implement a receiving end.
//! One axis needs the agent's cooperation; this one needs the terminal's.
//!
//! Both terminals teleport could inject into were hardcoded — `tmux_ttys` and
//! an iTerm2 AppleScript literal — so a third meant a Rust change and a
//! release. `adapter::decl` already made exactly this move for agent runtimes,
//! after the same mistake ("teleport used to decide which runtimes exist by an
//! `if lower.contains("claude")` chain"), and left the migration path behind as
//! tests: write the descriptor, prove it reproduces the hand-written one, then
//! delete the hand-written one.
//!
//! # What a descriptor may and may not contain
//!
//! DATA, never a script. A descriptor names the application, the container
//! chain to walk, the property holding a tty, and a ONE-LINE send template —
//! teleport generates the AppleScript, does the escaping, and owns the parts
//! that have historically broken (`-2741` from `write text "…" to s`, and the
//! shell expanding a backtick inside a message body). Handing users a free-form
//! script would move exactly those failures into a file nothing tests.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// How a terminal is driven.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Backend {
    /// A macOS app with a scripting dictionary. Both Terminal.app and iTerm2
    /// are this; they differ only in the container chain and the send verb.
    ///
    /// Named explicitly: `snake_case` would spell this `apple_script` in the
    /// TOML, which is not what anyone writing the file would guess.
    #[serde(rename = "applescript")]
    AppleScript {
        /// The `tell application "…"` target.
        app: String,
        /// Nesting to walk, outermost first — iTerm2 is
        /// `windows → tabs → sessions`, Terminal.app has no `sessions` layer.
        containers: Vec<String>,
        /// Property on the innermost object holding its tty.
        #[serde(default = "default_tty_of")]
        tty_of: String,
        /// One line, with `{leaf}` (the matched object) and `{text}` (the
        /// escaped, already-quoted string). Everything around it is generated.
        send: String,
    },
    /// A terminal driven by its own CLI. `resolve` must print `<tty> <handle>`
    /// per line; `send` is run with `{handle}` and `{text}` substituted.
    Command {
        resolve: Vec<String>,
        send: Vec<String>,
    },
}

fn default_tty_of() -> String {
    "tty".to_string()
}

fn default_os() -> String {
    "any".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TerminalConfig {
    pub id: String,
    /// `macos`, `linux`, or `any`. Checked before anything is spawned, so a
    /// descriptor for another platform costs nothing but a file.
    #[serde(default = "default_os")]
    pub os: String,
    #[serde(flatten)]
    pub backend: Backend,
}

impl TerminalConfig {
    /// Whether this descriptor applies to the running platform.
    pub fn applies_here(&self) -> bool {
        match self.os.as_str() {
            "any" => true,
            "macos" => cfg!(target_os = "macos"),
            "linux" => cfg!(target_os = "linux"),
            "windows" => cfg!(target_os = "windows"),
            _ => false,
        }
    }

    /// Reject a descriptor that cannot produce a working script, at LOAD time
    /// rather than at wake time. A terminal backend is only ever exercised when
    /// someone is already waiting for a message, so a malformed one must not
    /// wait until then to say so.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty() {
            bail!("terminal descriptor has an empty id");
        }
        match &self.backend {
            Backend::AppleScript {
                app,
                containers,
                send,
                ..
            } => {
                if app.is_empty() {
                    bail!("{}: applescript backend needs an `app`", self.id);
                }
                if containers.is_empty() {
                    bail!("{}: `containers` must name at least one level", self.id);
                }
                if !send.contains("{text}") {
                    bail!("{}: `send` must use {{text}} or it sends nothing", self.id);
                }
                if !send.contains("{leaf}") {
                    bail!(
                        "{}: `send` must use {{leaf}} or it has no target to send to",
                        self.id
                    );
                }
            }
            Backend::Command { resolve, send } => {
                if resolve.is_empty() || send.is_empty() {
                    bail!(
                        "{}: command backend needs both `resolve` and `send`",
                        self.id
                    );
                }
                if !send.iter().any(|a| a.contains("{text}")) {
                    bail!("{}: `send` must use {{text}}", self.id);
                }
            }
        }
        Ok(())
    }

    /// The script that finds the object owning `tty` and sends `text` to it.
    /// Returns `ok` or `not-found` on stdout, which is the contract every
    /// caller of this module already expects.
    ///
    /// `None` for a non-AppleScript backend.
    pub fn applescript_send(&self, tty: &str, text: &str) -> Option<String> {
        let Backend::AppleScript {
            app,
            containers,
            tty_of,
            send,
        } = &self.backend
        else {
            return None;
        };
        Some(build_applescript(
            app,
            containers,
            tty_of,
            send,
            tty,
            Some(text),
        ))
    }

    /// Same walk, but it only reports whether this terminal owns `tty` —
    /// `resolve` needs the question without the side effect.
    pub fn applescript_probe(&self, tty: &str) -> Option<String> {
        let Backend::AppleScript {
            app,
            containers,
            tty_of,
            send,
        } = &self.backend
        else {
            return None;
        };
        Some(build_applescript(app, containers, tty_of, send, tty, None))
    }
}

/// A tty as AppleScript should match it.
///
/// `tty of s` always answers the `/dev/...` form, so the needle is compared
/// with `ends with` against a stripped tty — the same normalization
/// `resolve::iterm_session_exists_for_tty` documented before this module
/// existed.
fn needle(tty: &str) -> String {
    tty.trim_start_matches("/dev/").to_string()
}

/// AppleScript string escaping. Backslash FIRST — escaping quotes first would
/// then double the backslashes it just inserted.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Generate the nested walk. `text = None` builds the probe (match and report),
/// `Some` builds the send.
fn build_applescript(
    app: &str,
    containers: &[String],
    tty_of: &str,
    send: &str,
    tty: &str,
    text: Option<&str>,
) -> String {
    let needle = needle(tty);
    let mut out = format!("tell application \"{app}\"\n");

    // `v1 in windows`, then `v2 in tabs of v1`, … — each level scoped to the
    // one above it, which is what makes `leaf` unambiguous.
    for (i, container) in containers.iter().enumerate() {
        let indent = "  ".repeat(i + 1);
        if i == 0 {
            out.push_str(&format!("{indent}repeat with v1 in {container}\n"));
        } else {
            out.push_str(&format!(
                "{indent}repeat with v{} in {container} of v{}\n",
                i + 1,
                i
            ));
        }
    }

    let leaf = format!("v{}", containers.len());
    let depth = containers.len();
    let ind = "  ".repeat(depth + 1);
    // `try` per candidate: a window closing mid-walk raises, and one dead
    // object must not abort the search for the tty we actually want.
    out.push_str(&format!("{ind}try\n"));
    out.push_str(&format!(
        "{ind}  if ({tty_of} of {leaf}) ends with \"{needle}\" then\n"
    ));
    if let Some(t) = text {
        let body = send
            .replace("{leaf}", &leaf)
            .replace("{text}", &format!("\"{}\"", escape(t)));
        out.push_str(&format!("{ind}    {body}\n"));
    }
    out.push_str(&format!("{ind}    return \"ok\"\n"));
    out.push_str(&format!("{ind}  end if\n"));
    out.push_str(&format!("{ind}end try\n"));

    for i in (0..containers.len()).rev() {
        out.push_str(&format!("{}end repeat\n", "  ".repeat(i + 1)));
    }
    out.push_str("  return \"not-found\"\n");
    out.push_str("end tell");
    out
}

/// Where user-supplied terminal descriptors live, mirroring `runtimes.d`.
pub fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    Path::new(&home).join(".teleport").join("terminals.d")
}

/// The descriptors teleport ships, compiled in.
///
/// Built in rather than only installed as files, for the same reason
/// `adapter::decl` keeps built-ins: a fresh checkout, a `cargo install`, or an
/// install step that did not run must still be able to wake a session. Files in
/// `terminals.d/` add to these and OVERRIDE one with the same id.
const BUILTIN: &[(&str, &str)] = &[
    (
        "iterm2",
        include_str!("../../../install/terminals.d/iterm2.toml"),
    ),
    (
        "terminal_app",
        include_str!("../../../install/terminals.d/terminal_app.toml"),
    ),
];

/// Every terminal backend available on this machine, user files last so they
/// win, and filtered to the running platform.
pub fn all() -> Vec<TerminalConfig> {
    let mut out: Vec<TerminalConfig> = BUILTIN
        .iter()
        .filter_map(|(id, raw)| match toml::from_str::<TerminalConfig>(raw) {
            Ok(c) if c.validate().is_ok() => Some(c),
            // A broken BUILT-IN is a bug in teleport, not in the user's setup,
            // and it is caught by `the_shipped_descriptors_load_*` — never
            // silently, but also never fatally at runtime.
            _ => {
                tp_core::log_warn!("teleport: built-in terminal descriptor {id} is malformed");
                None
            }
        })
        .collect();

    for user in load_dir(&config_dir()) {
        match out.iter().position(|c| c.id == user.id) {
            Some(i) => out[i] = user,
            None => out.push(user),
        }
    }
    out.retain(|c| c.applies_here());
    out
}

/// Load every `*.toml` in `dir`.
///
/// A malformed or inapplicable file is SKIPPED WITH A WARNING, never fatal —
/// the same fault isolation `decl::load_dir` uses, and for the same reason: one
/// bad descriptor must not cost the terminals that are fine. Sorted by id so
/// the resolution order is stable rather than filesystem-dependent.
pub fn load_dir(dir: &Path) -> Vec<TerminalConfig> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match parse_file(&path) {
            Ok(cfg) => out.push(cfg),
            Err(e) => tp_core::log_warn!("teleport: skipping {}: {e:#}", path.display()),
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn parse_file(path: &Path) -> Result<TerminalConfig> {
    let raw = std::fs::read_to_string(path).context("read")?;
    let cfg: TerminalConfig = toml::from_str(&raw).context("parse")?;
    cfg.validate()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iterm() -> TerminalConfig {
        TerminalConfig {
            id: "iterm2".into(),
            os: "macos".into(),
            backend: Backend::AppleScript {
                app: "iTerm2".into(),
                containers: vec!["windows".into(), "tabs".into(), "sessions".into()],
                tty_of: "tty".into(),
                send: "tell {leaf} to write text {text}".into(),
            },
        }
    }

    fn terminal_app() -> TerminalConfig {
        TerminalConfig {
            id: "terminal_app".into(),
            os: "macos".into(),
            backend: Backend::AppleScript {
                app: "Terminal".into(),
                containers: vec!["windows".into(), "tabs".into()],
                tty_of: "tty".into(),
                send: "do script {text} in {leaf}".into(),
            },
        }
    }

    fn squash(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// THE migration test, in the shape `decl.rs` established with
    /// `a_toml_config_reproduces_the_builtin_adapter`: the descriptor must
    /// generate the script that was hand-written and known to work, so the
    /// mechanism is proven against a WORKING backend before any new terminal
    /// depends on it.
    ///
    /// The literal below is `wake::iterm_write_text`'s script verbatim.
    #[test]
    fn the_iterm2_descriptor_reproduces_the_handwritten_script() {
        let handwritten = r#"tell application "iTerm2"
            repeat with w in windows
                repeat with t in tabs of w
                    repeat with s in sessions of t
                        try
                            if (tty of s) ends with "ttys013" then
                                tell s to write text "hello"
                                return "ok"
                            end if
                        end try
                    end repeat
                end repeat
            end repeat
            return "not-found"
        end tell"#;
        let generated = iterm().applescript_send("ttys013", "hello").unwrap();

        // Whitespace and loop-variable names are the only permitted difference,
        // so normalize both and rename v1/v2/v3 to the hand-written w/t/s.
        let generated = generated
            .replace("v1", "w")
            .replace("v2", "t")
            .replace("v3", "s");
        assert_eq!(squash(&generated), squash(handwritten));
    }

    /// The other shipped descriptor, whose whole point is that it differs only
    /// in data: one fewer container and a different verb.
    #[test]
    fn the_terminal_app_descriptor_walks_tabs_and_uses_do_script() {
        let s = terminal_app().applescript_send("ttys013", "hi").unwrap();
        assert!(s.contains(r#"tell application "Terminal""#), "{s}");
        assert!(s.contains("repeat with v1 in windows"), "{s}");
        assert!(s.contains("repeat with v2 in tabs of v1"), "{s}");
        assert!(!s.contains("sessions"), "Terminal.app has no sessions: {s}");
        assert!(s.contains(r#"do script "hi" in v2"#), "{s}");
        assert!(s.contains(r#"(tty of v2) ends with "ttys013""#), "{s}");
    }

    /// Escaping stays in Rust, where it is tested — the reason the descriptor
    /// carries a template and not a script.
    #[test]
    fn quotes_and_backslashes_in_the_message_cannot_break_out() {
        let s = iterm()
            .applescript_send("ttys001", r#"say "hi" \ now"#)
            .unwrap();
        assert!(s.contains(r#"write text "say \"hi\" \\ now""#), "{s}");
    }

    /// The probe is the same walk with the send omitted — `resolve` must be
    /// able to ask "is this yours?" without typing anything.
    #[test]
    fn the_probe_matches_without_sending() {
        let s = iterm().applescript_probe("ttys001").unwrap();
        assert!(s.contains("ends with \"ttys001\""), "{s}");
        assert!(s.contains("return \"ok\""), "{s}");
        assert!(!s.contains("write text"), "a probe must not type: {s}");
    }

    /// `/dev/` is stripped because `tty of s` always answers the long form.
    #[test]
    fn a_dev_prefixed_tty_matches_the_same_object() {
        let a = iterm().applescript_send("/dev/ttys004", "x").unwrap();
        let b = iterm().applescript_send("ttys004", "x").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_descriptor_that_could_not_work_is_refused_at_load_time() {
        let mut bad = iterm();
        bad.backend = Backend::AppleScript {
            app: "iTerm2".into(),
            containers: vec!["windows".into()],
            tty_of: "tty".into(),
            send: "tell {leaf} to beep".into(), // no {text}
        };
        let err = bad.validate().unwrap_err().to_string();
        assert!(err.contains("{text}"), "{err}");

        let mut empty = iterm();
        empty.backend = Backend::AppleScript {
            app: "iTerm2".into(),
            containers: vec![],
            tty_of: "tty".into(),
            send: "tell {leaf} to write text {text}".into(),
        };
        assert!(empty.validate().is_err());
    }

    #[test]
    fn descriptors_parse_from_toml_with_defaults() {
        let cfg: TerminalConfig = toml::from_str(
            r#"
            id = "terminal_app"
            os = "macos"
            kind = "applescript"
            app = "Terminal"
            containers = ["windows", "tabs"]
            send = "do script {text} in {leaf}"
        "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.id, "terminal_app");
        let Backend::AppleScript { tty_of, .. } = &cfg.backend else {
            panic!("wrong kind");
        };
        assert_eq!(tty_of, "tty", "tty_of should default");
    }

    fn shipped_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/terminals.d")
    }

    /// The descriptors teleport SHIPS must load, validate, and — for iTerm2 —
    /// still reproduce the hand-written script. Mirrors
    /// `decl.rs::shipped_configs_match_their_builtin_adapters`: the file on
    /// disk is the thing that runs, so the file on disk is what gets asserted,
    /// not a struct built in the test.
    #[test]
    fn the_shipped_descriptors_load_and_iterm2_still_matches_the_original() {
        let loaded = load_dir(&shipped_dir());
        let ids: Vec<&str> = loaded.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"iterm2"), "{ids:?}");
        assert!(ids.contains(&"terminal_app"), "{ids:?}");

        let shipped_iterm = loaded.iter().find(|c| c.id == "iterm2").unwrap();
        assert_eq!(
            shipped_iterm.applescript_send("ttys013", "hello"),
            iterm().applescript_send("ttys013", "hello"),
            "the shipped iterm2.toml must generate exactly what the verified \
             in-code descriptor does — otherwise the equivalence proof covers \
             a file nobody ships"
        );
    }

    /// The built-ins must be valid, because nothing else guarantees a machine
    /// with no `terminals.d/` can wake anything at all.
    #[test]
    fn the_builtin_descriptors_are_valid_and_cover_both_mac_terminals() {
        let all = all();
        let ids: Vec<&str> = all.iter().map(|c| c.id.as_str()).collect();
        if cfg!(target_os = "macos") {
            assert!(ids.contains(&"iterm2"), "{ids:?}");
            assert!(ids.contains(&"terminal_app"), "{ids:?}");
        }
        for c in &all {
            c.validate().unwrap();
        }
    }

    /// A malformed descriptor costs itself and nothing else — one bad file
    /// must not take out the terminals that are fine (`decl::load_dir`'s rule).
    #[test]
    fn one_bad_descriptor_does_not_hide_the_good_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.toml"), "id = \"nope\"\n").unwrap();
        std::fs::write(
            dir.path().join("ok.toml"),
            r#"
            id = "terminal_app"
            kind = "applescript"
            app = "Terminal"
            containers = ["windows", "tabs"]
            send = "do script {text} in {leaf}"
        "#,
        )
        .unwrap();

        let loaded = load_dir(dir.path());
        assert_eq!(loaded.len(), 1, "{loaded:?}");
        assert_eq!(loaded[0].id, "terminal_app");
    }

    #[test]
    fn a_command_backend_parses_and_is_os_agnostic() {
        // `r##` because tmux's own format string contains `"#`, which would
        // otherwise end the raw literal — the same quoting hazard the
        // descriptor exists to keep out of user files.
        let cfg: TerminalConfig = toml::from_str(
            r##"
            id = "tmux"
            kind = "command"
            resolve = ["tmux", "list-panes", "-a", "-F", "#{pane_tty} #{pane_id}"]
            send = ["tmux", "send-keys", "-t", "{handle}", "{text}", "Enter"]
        "##,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.os, "any", "a CLI-driven terminal is not macOS-specific");
        assert!(cfg.applies_here(), "`any` must apply on every platform");
        assert!(
            cfg.applescript_send("ttys001", "x").is_none(),
            "a command backend has no AppleScript"
        );
    }
}
