//! End-to-end: run the real `tp` binary against a fixture corpus.
//!
//! Written because the unit tests could not have caught the bug that prompted
//! them. `tp turns --since 2026-08-04 --until 2026-08-05` failed with "bad
//! duration" while `window_scope` — the function a unit test would reach for —
//! handled absolute dates correctly the whole time. The defect was that the
//! no-session-id path called `parse_duration` INSTEAD of it. Testing the
//! function that was already right proves nothing about the caller that wasn't.
//!
//! So this spawns the built binary with a fixture `HOME` and asserts on what a
//! user actually sees.

use std::path::Path;
use std::process::Command;

const UUID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

/// Two sessions in different folders, both with turns on 2026-08-04 local.
fn fixture(home: &Path) {
    let day = |h: u32, m: u32| {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 8, 4, h, m, 0)
            .unwrap()
            .to_rfc3339()
    };
    let write = |dir: &str, uuid: &str, cwd: &str, text: &str, ts: String| {
        let proj = home.join(".claude/projects").join(dir);
        std::fs::create_dir_all(&proj).unwrap();
        let line = serde_json::json!({
            "type": "user", "cwd": cwd, "timestamp": ts,
            "message": {"content": text}
        });
        std::fs::write(proj.join(format!("{uuid}.jsonl")), format!("{line}\n")).unwrap();
    };
    write(
        "-Users-test-dev-demo",
        UUID,
        "/Users/test/dev/demo",
        "the demo session said this on the fourth",
        day(10, 0),
    );
    // A second, MORE RECENT session elsewhere — so "just pick the newest" is
    // observably the wrong answer when no folder narrows it.
    write(
        "-Users-test-dev-other",
        "11111111-2222-3333-4444-555555555555",
        "/Users/test/dev/other",
        "unrelated project, same day",
        day(23, 0),
    );
}

fn tp(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tp"))
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run tp");
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), s)
}

/// The exact shape the README documents, and the exact shape that was broken:
/// an absolute day, no session id, narrowed by folder.
#[test]
fn a_specific_day_reads_without_a_session_id() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());

    let (ok, out) = tp(
        home.path(),
        &[
            "turns",
            "--since",
            "2026-08-04",
            "--until",
            "2026-08-05",
            "--folder",
            "/Users/test/dev/demo",
        ],
    );
    assert!(ok, "should succeed, got:\n{out}");
    assert!(
        out.contains("the demo session said this on the fourth"),
        "must read that day's turns, got:\n{out}"
    );
    assert!(
        !out.contains("unrelated project"),
        "--folder must exclude the other session, got:\n{out}"
    );
}

/// Ambiguity must be refused, not resolved by guessing. Reading one arbitrary
/// session and presenting it as "that day" answers a different question than
/// the one asked.
#[test]
fn an_ambiguous_window_lists_candidates_instead_of_picking_one() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());

    let (ok, out) = tp(
        home.path(),
        &["turns", "--since", "2026-08-04", "--until", "2026-08-05"],
    );
    assert!(!ok, "ambiguous read must fail, got:\n{out}");
    assert!(
        out.contains("2 sessions were active"),
        "must say how many matched, got:\n{out}"
    );
    assert!(
        out.contains("--folder"),
        "must say how to narrow, got:\n{out}"
    );
    assert!(
        out.contains("/Users/test/dev/demo") && out.contains("/Users/test/dev/other"),
        "must list the candidates by cwd, got:\n{out}"
    );
}

/// The upper bound has to reach the session CHOICE, not just the turn filter.
/// It didn't: candidates were "sessions since the 4th", which on a later date
/// is today's session — then read through the 4th's window and found nothing.
/// An empty result for a day that has content is the silent-wrong-answer shape.
#[test]
fn the_window_bounds_which_sessions_are_candidates() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());

    let (ok, out) = tp(
        home.path(),
        &[
            "turns",
            "--since",
            "2026-08-05",
            "--until",
            "2026-08-06",
            "--folder",
            "/Users/test/dev/demo",
        ],
    );
    assert!(!ok, "a day with no activity must not silently succeed");
    assert!(
        out.contains("no sessions active between 2026-08-05 and 2026-08-06"),
        "must name the window it found nothing in, got:\n{out}"
    );
}

/// A duration bound still works, and still auto-picks when a folder narrows it.
#[test]
fn a_relative_window_still_works() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());
    let (ok, out) = tp(
        home.path(),
        &["sessions", "--since", "2026-08-04", "--until", "2026-08-05"],
    );
    assert!(ok, "sessions should succeed, got:\n{out}");
    assert!(out.contains("/Users/test/dev/demo"), "got:\n{out}");
    assert!(out.contains("/Users/test/dev/other"), "got:\n{out}");
}

/// `subagent` and `thinking_opaque` must survive to BOTH output surfaces.
///
/// Everything below the surface already had tests: ingest reads the flags,
/// the writer stores them, both providers carry them through `prov`. What had
/// none was the last inch — the CLI marked subagent turns while the MCP JSON
/// (the surface agents actually read) said nothing, and an opaque turn fell
/// through to "(no indexed content)", which for codex's encrypted reasoning is
/// precisely the "no reasoning happened" claim `thinking_state = 'opaque'`
/// exists to prevent.
///
/// The codex half installs the SHIPPED descriptor into the fixture home —
/// codex has no built-in adapter, and a config invented here would pass while
/// the file teleport actually ships stayed broken.
#[test]
fn subagent_and_opaque_reach_both_output_surfaces() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    fixture(home);

    // A subagent transcript, where Claude Code really writes one.
    let sub = home
        .join(".claude/projects/-Users-test-dev-demo")
        .join(UUID)
        .join("subagents");
    std::fs::create_dir_all(&sub).unwrap();
    let line = serde_json::json!({
        "type": "user", "isSidechain": true, "cwd": "/Users/test/dev/demo",
        "timestamp": "2026-08-04T10:05:00-07:00",
        "message": {"content": "the subagent reporting in"}
    });
    std::fs::write(sub.join("agent-e2e01.jsonl"), format!("{line}\n")).unwrap();

    // A codex rollout whose only reasoning is an encrypted blob.
    let codex_dir = home.join(".codex/sessions/2026/08/04");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let msg = serde_json::json!({
        "timestamp": "2026-08-04T17:00:00.000Z", "type": "response_item",
        "payload": {"type": "message", "role": "assistant",
                     "content": [{"type": "output_text", "text": "an answer"}]}
    });
    let reasoning = serde_json::json!({
        "timestamp": "2026-08-04T17:00:01.000Z", "type": "response_item",
        "payload": {"type": "reasoning", "summary": [],
                     "encrypted_content": "gAAAAABqgTJt-not-readable"}
    });
    std::fs::write(
        codex_dir.join("rollout-2026-08-04T17-00-00-019d6df0-7a4b-7120-a4fd-51736a970ed6.jsonl"),
        format!("{msg}\n{reasoning}\n"),
    )
    .unwrap();
    let rt = home.join(".teleport/runtimes.d");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/codex.toml"),
        rt.join("codex.toml"),
    )
    .unwrap();

    let (ok, _) = tp(home, &["index"]);
    assert!(ok);
    // `tp id` prints a labelled block for humans; the id is one field of it.
    let machine = tp(home, &["id"])
        .1
        .lines()
        .find_map(|l| l.strip_prefix("device id : ").map(str::to_string))
        .unwrap();
    let sub_sid = format!("{machine}/claude_code/agent-e2e01");
    let codex_sid = format!("{machine}/codex/019d6df0-7a4b-7120-a4fd-51736a970ed6");

    // CLI surface.
    let (ok, out) = tp(home, &["turns", &sub_sid, "--index"]);
    assert!(ok, "{out}");
    assert!(out.contains("[subagent]"), "CLI must mark the turn: {out}");
    let (ok, out) = tp(home, &["turns", &codex_sid, "--index"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("encrypted by the runtime"),
        "an opaque turn must not read as \"no indexed content\": {out}"
    );

    // MCP surface — the same two sessions through `tp mcp` over stdio.
    let call = |sid: &str, include_thinking: bool| -> serde_json::Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tp"))
            .arg("mcp")
            .env("HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{}\n{}\n",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"teleport_turns",
                "arguments":{"session_id": sid, "index": true,
                              "include_thinking": include_thinking}}}),
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        let last = String::from_utf8_lossy(&out.stdout);
        let last = last.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(last).unwrap();
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };

    let turns = call(&sub_sid, false);
    assert_eq!(
        turns["turns"][0]["subagent"],
        serde_json::json!(true),
        "MCP must say whose words these are: {turns}"
    );

    let turns = call(&codex_sid, true);
    let opaque: Vec<_> = turns["turns"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["thinking_opaque"] == serde_json::json!(true))
        .collect();
    assert_eq!(opaque.len(), 1, "the encrypted reasoning record: {turns}");
    // And WITHOUT the opt-in the key stays absent — no thinking keys at all
    // means nothing is being claimed either way.
    let turns = call(&codex_sid, false);
    assert!(
        turns["turns"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t.get("thinking_opaque").is_none()),
        "{turns}"
    );
}

/// A superseded turn must SAY so on both output surfaces, from both providers.
///
/// The column had been written for 143,683 turns while every read path stayed
/// silent — search, turns, MCP all presented compacted-away history exactly
/// like live context. This drives the shipped claude_code descriptor (the
/// built-in adapter deliberately carries no compaction rules) through index and
/// scan, and checks the one line that differs: the kept turn carries no marker.
#[test]
fn superseded_turns_say_so_on_both_surfaces_from_both_providers() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let msg = |text: &str, ts: &str| {
        serde_json::json!({
            "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": ts,
            "message": {"content": text}
        })
        .to_string()
    };
    std::fs::write(
        proj.join(format!("{UUID}.jsonl")),
        format!(
            "{}\n{}\n{}\n{}\n",
            msg("before the cut one", "2026-08-04T10:00:00-07:00"),
            msg("before the cut two", "2026-08-04T10:01:00-07:00"),
            serde_json::json!({
                "type": "system", "subtype": "compact_boundary",
                "timestamp": "2026-08-04T10:02:00-07:00"
            }),
            msg("kept after the cut", "2026-08-04T10:03:00-07:00"),
        ),
    )
    .unwrap();
    let rt = home.join(".teleport/runtimes.d");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/claude_code.toml"),
        rt.join("claude_code.toml"),
    )
    .unwrap();

    let (ok, _) = tp(home, &["index"]);
    assert!(ok);
    let machine = tp(home, &["id"])
        .1
        .lines()
        .find_map(|l| l.strip_prefix("device id : ").map(str::to_string))
        .unwrap();
    let sid = format!("{machine}/claude_code/{UUID}");

    // CLI, both providers — the same session must read the same way.
    for args in [vec!["turns", &sid, "--index"], vec!["turns", &sid]] {
        let (ok, out) = tp(home, &args);
        assert!(ok, "{out}");
        assert_eq!(
            out.matches("[superseded]").count(),
            2,
            "args {args:?}: {out}"
        );
        let kept = out
            .lines()
            .find(|l| l.contains("kept after the cut"))
            .unwrap();
        assert!(!kept.contains("[superseded]"), "{kept}");
    }

    // MCP (always the scan provider).
    let mut child = Command::new(env!("CARGO_BIN_EXE_tp"))
        .arg("mcp")
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    write!(
        child.stdin.take().unwrap(),
        "{}\n{}\n",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"teleport_turns","arguments":{"session_id": sid}}}),
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    let last = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(last.lines().last().unwrap()).unwrap();
    let turns: serde_json::Value =
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let turns = turns["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 3, "{turns:?}");
    assert_eq!(turns[0]["surface"], serde_json::json!("superseded"));
    assert_eq!(turns[1]["surface"], serde_json::json!("superseded"));
    assert!(
        turns[2].get("surface").is_none(),
        "absent means current — the kept turn makes that claim: {:?}",
        turns[2]
    );
}

/// Two ends of the same claim, driven through real `tp mcp` processes:
/// a search hit says whose words matched and whether they are still context,
/// and a session whose transcript is GONE is served from the index instead of
/// reading as empty — 14,301 sessions on the machine this was written on exist
/// only there, and `[]` for them is indistinguishable from "nothing happened".
#[test]
fn mcp_search_carries_flags_and_turns_falls_back_to_the_index() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    // Recent timestamps: MCP search defaults to a 6h window.
    let now = chrono::Utc::now().timestamp_millis();
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp(ms / 1000, 0)
            .unwrap()
            .to_rfc3339()
    };
    let msg = |text: &str, ms: i64| {
        serde_json::json!({
            "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": iso(ms),
            "message": {"content": text}
        })
        .to_string()
    };
    std::fs::write(
        proj.join(format!("{UUID}.jsonl")),
        format!(
            "{}\n{}\n{}\n",
            msg("harpoon before the cut", now - 300_000),
            serde_json::json!({
                "type": "system", "subtype": "compact_boundary",
                "timestamp": iso(now - 240_000)
            }),
            msg("harpoon after the cut", now - 180_000),
        ),
    )
    .unwrap();
    let rt = home.join(".teleport/runtimes.d");
    std::fs::create_dir_all(&rt).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d/claude_code.toml"),
        rt.join("claude_code.toml"),
    )
    .unwrap();
    let (ok, _) = tp(home, &["index"]);
    assert!(ok);
    let machine = tp(home, &["id"])
        .1
        .lines()
        .find_map(|l| l.strip_prefix("device id : ").map(str::to_string))
        .unwrap();
    let sid = format!("{machine}/claude_code/{UUID}");

    let mcp = |body: serde_json::Value| -> serde_json::Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_tp"))
            .arg("mcp")
            .env("HOME", home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        write!(
            child.stdin.take().unwrap(),
            "{}\n{}\n",
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":body}),
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        let last = String::from_utf8_lossy(&out.stdout);
        let v: serde_json::Value = serde_json::from_str(last.lines().last().unwrap()).unwrap();
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };

    // Search: the superseded match says so, the kept one claims current by absence.
    let found = mcp(serde_json::json!({"name":"teleport_search","arguments":{"query":"harpoon"}}));
    let items = found["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{found}");
    let by = |needle: &str| {
        items
            .iter()
            .find(|i| i["excerpt"].as_str().unwrap().contains(needle))
            .unwrap()
    };
    assert_eq!(by("before")["surface"], serde_json::json!("superseded"));
    assert!(by("after").get("surface").is_none(), "{found}");

    // Delete the transcript: the scan now finds nothing, and before the
    // fallback this returned `turns: []` with no hint the index knew better.
    std::fs::remove_file(proj.join(format!("{UUID}.jsonl"))).unwrap();
    let got = mcp(serde_json::json!({"name":"teleport_turns","arguments":{"session_id": sid}}));
    let turns = got["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 2, "served from the index: {got}");
    assert!(
        got["note_source"].as_str().unwrap().contains("index"),
        "the caller must be told which source answered: {got}"
    );
    assert_eq!(turns[0]["surface"], serde_json::json!("superseded"));
}

/// `tp version` is the drift surface, and descriptor staleness is a drift:
/// a file in ~/.teleport/runtimes.d wins over the embedded descriptor, and a
/// stale one made two different rebuilt binaries run "byte-identical wrong" in
/// one session. Version must name the file; a clean home must say nothing.
#[test]
fn version_names_descriptor_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    // Clean: no runtimes.d at all → no descriptor lines.
    let (ok, out) = tp(home, &["version"]);
    assert!(ok, "{out}");
    assert!(!out.contains("descriptor override"), "{out}");

    // A stale-or-customized override and a redundant identical copy.
    let rt = home.join(".teleport/runtimes.d");
    std::fs::create_dir_all(&rt).unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d");
    std::fs::copy(repo.join("pi.toml"), rt.join("pi.toml")).unwrap();
    let cc = std::fs::read_to_string(repo.join("claude_code.toml")).unwrap();
    std::fs::write(
        rt.join("claude_code.toml"),
        cc.replace("subagents", "helpers"),
    )
    .unwrap();

    let (ok, out) = tp(home, &["version"]);
    assert!(ok, "{out}");
    let differs = out
        .lines()
        .find(|l| l.contains("DIFFERS"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(
        differs.contains("claude_code.toml") && differs.contains("embedded claude_code"),
        "{differs}"
    );
    let redundant = out
        .lines()
        .find(|l| l.contains("byte-identical"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(redundant.contains("pi.toml"), "{redundant}");
}

/// A scan cannot read a session whose transcript is gone, and for a quarter of
/// this machine's corpus that is the situation. Before this, `tp search` said
/// `no matches` and blamed its file budget — a true sentence about the wrong
/// thing, since no budget would have found them.
///
/// Both shapes are covered, because only one of them a fallback can fix:
/// a search that finds NOTHING (answerable from the index instead) and a search
/// that finds SOME of what is there (not silently replaced — reported).
#[test]
fn a_scan_says_what_it_cannot_read() {
    use std::io::Write as _;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp(ms / 1000, 0)
            .unwrap()
            .to_rfc3339()
    };
    let write = |uuid: &str, text: &str, ms: i64| {
        std::fs::write(
            proj.join(format!("{uuid}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": iso(ms),
                    "message": {"content": text}
                })
            ),
        )
        .unwrap();
    };
    // Two sessions with the same needle; one will lose its transcript.
    write(UUID, "narwhal in the kept session", now - 60_000);
    let doomed = "22222222-3333-4444-5555-666666666666";
    write(doomed, "narwhal in the deleted session", now - 120_000);

    let (ok, _) = tp(home, &["index"]);
    assert!(ok);
    std::fs::remove_file(proj.join(format!("{doomed}.jsonl"))).unwrap();

    // PARTIAL: the scan finds the surviving one and must say the other exists.
    let (ok, out) = tp(home, &["search", "narwhal", "--since", "1d"]);
    assert!(ok, "{out}");
    assert!(out.contains("kept session"), "{out}");
    assert!(
        !out.contains("deleted session"),
        "a scan cannot read it: {out}"
    );
    assert!(
        out.contains("no transcript on disk") && out.contains("1 session(s)"),
        "the miss must be reported, not left to the budget line: {out}"
    );
    // …and --index reads both, with nothing to report.
    let (ok, out) = tp(home, &["search", "narwhal", "--since", "1d", "--index"]);
    assert!(ok, "{out}");
    assert!(out.contains("deleted session"), "{out}");
    assert!(!out.contains("no transcript on disk"), "{out}");

    // A window with nothing missing stays silent.
    let (_, out) = tp(home, &["search", "narwhal", "--since", "90s"]);
    assert!(!out.contains("no transcript on disk"), "{out}");

    // EMPTY through MCP: the fallback answers rather than reporting an absence
    // the scan was never able to establish.
    let mut child = Command::new(env!("CARGO_BIN_EXE_tp"))
        .arg("mcp")
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    write!(
        child.stdin.take().unwrap(),
        "{}\n{}\n",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"teleport_search",
            "arguments":{"query":"deleted session","since":"1d"}}}),
    )
    .unwrap();
    let out = child.wait_with_output().unwrap();
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
    let got: serde_json::Value =
        serde_json::from_str(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(got["provider"], serde_json::json!("index"), "{got}");
    assert_eq!(got["items"].as_array().unwrap().len(), 1, "{got}");
    assert!(
        got["note_source"].as_str().unwrap().contains("gone"),
        "{got}"
    );
}

/// `tp verify` has to FAIL on a damaged index, not print a reassuring summary —
/// and it has to name the stake, because the number that decides whether damage
/// is an inconvenience or a loss is "how much of this has no other copy".
///
/// The damage injected is the one this repo actually produced: a reindex racing
/// the daemon left a session holding 12 of its 10,836 turns, with `turn_count`
/// still claiming the original. SQLite's own checks cannot see that; it is a
/// teleport invariant.
#[test]
fn verify_fails_on_damage_and_names_what_is_irreplaceable() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp(ms / 1000, 0)
            .unwrap()
            .to_rfc3339()
    };
    let write = |uuid: &str, ms: i64| {
        std::fs::write(
            proj.join(format!("{uuid}.jsonl")),
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"user","cwd":"/Users/test/dev/demo",
                                   "timestamp": iso(ms), "message":{"content":"one"}}),
                serde_json::json!({"type":"user","cwd":"/Users/test/dev/demo",
                                   "timestamp": iso(ms + 1000), "message":{"content":"two"}}),
            ),
        )
        .unwrap();
    };
    write(UUID, now - 60_000);
    let doomed = "22222222-3333-4444-5555-666666666666";
    write(doomed, now - 120_000);
    let (ok, _) = tp(home, &["index"]);
    assert!(ok);

    // Clean, with both transcripts present: nothing is irreplaceable yet.
    let (ok, out) = tp(home, &["verify"]);
    assert!(ok, "{out}");
    assert!(out.contains("still has its transcript on disk"), "{out}");
    assert!(out.trim_end().ends_with("ok"), "{out}");

    // Delete one transcript: the index becomes the only copy of its turns, and
    // verify must say so — with the count, not a vague warning.
    std::fs::remove_file(proj.join(format!("{doomed}.jsonl"))).unwrap();
    let (ok, out) = tp(home, &["verify"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("1 session(s) / 2 of 4 turn(s) exist ONLY here"),
        "{out}"
    );

    // A backup of it succeeds, refuses to clobber itself, and is a real DB.
    let bk = home.join("snapshot.db");
    let (ok, out) = tp(home, &["backup", bk.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert!(out.contains("have no other copy"), "{out}");
    let (ok, out) = tp(home, &["backup", bk.to_str().unwrap()]);
    assert!(!ok, "a second backup to the same path must refuse: {out}");
    assert!(out.contains("refusing to overwrite"), "{out}");
    let n: i64 = rusqlite::Connection::open(&bk)
        .unwrap()
        .query_row("SELECT count(*) FROM turn", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 4, "the snapshot holds the turns, not just a file");

    // Damage a teleport invariant SQLite cannot see, and verify must fail.
    {
        let conn = rusqlite::Connection::open(home.join(".teleport/teleport.db")).unwrap();
        conn.execute("DELETE FROM turn WHERE seq = 2", []).unwrap();
    }
    let (ok, out) = tp(home, &["verify"]);
    assert!(!ok, "a damaged index must exit non-zero: {out}");
    assert!(out.contains("turn_count disagrees"), "{out}");
    assert!(
        out.contains("tp reindex"),
        "the way out belongs here: {out}"
    );
}

/// `tp archive` moves old sessions out; it must not lose one.
///
/// Moving rather than deleting is what lets the rule be as simple as "older
/// than N days". A quarter of a real index has no transcript left on disk and
/// age is no guide to which quarter, so a policy that DELETED by age would
/// destroy the only copies first — copying them out asks no such question.
#[test]
fn archive_moves_old_sessions_and_keeps_them_readable() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp(ms / 1000, 0)
            .unwrap()
            .to_rfc3339()
    };
    let write = |uuid: &str, text: &str, ms: i64| {
        std::fs::write(
            proj.join(format!("{uuid}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({"type":"user","cwd":"/Users/test/dev/demo",
                                   "timestamp": iso(ms), "message":{"content": text}})
            ),
        )
        .unwrap();
    };
    let old_on_disk = UUID;
    let old_gone = "22222222-3333-4444-5555-666666666666";
    let recent = "33333333-4444-5555-6666-777777777777";
    write(old_on_disk, "old but still on disk", now - 10 * 86_400_000);
    write(old_gone, "old and irreplaceable", now - 10 * 86_400_000);
    write(recent, "recent", now - 60_000);
    let (ok, _) = tp(home, &["index"]);
    assert!(ok);
    // Whether the transcript survives is deliberately irrelevant to archiving.
    std::fs::remove_file(proj.join(format!("{old_gone}.jsonl"))).unwrap();

    let arch = home.join("arch.db");
    let count = |db: &std::path::Path, sql: &str| -> i64 {
        rusqlite::Connection::open(db)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    };
    let main = home.join(".teleport/teleport.db");
    assert_eq!(count(&main, "SELECT count(*) FROM turn"), 3);

    let (ok, out) = tp(
        home,
        &[
            "archive",
            "--before",
            "1d",
            "--to",
            arch.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(ok, "{out}");
    assert!(
        out.contains("2 session(s) / 2 turn(s)"),
        "both old ones: {out}"
    );
    assert!(!arch.exists(), "--dry-run wrote a file: {out}");

    let (ok, out) = tp(
        home,
        &["archive", "--before", "1d", "--to", arch.to_str().unwrap()],
    );
    assert!(ok, "{out}");
    assert_eq!(count(&main, "SELECT count(*) FROM turn"), 1, "{out}");
    assert_eq!(count(&arch, "SELECT count(*) FROM turn"), 2, "{out}");
    // Nothing lost, including the one with no transcript to fall back on.
    let archived: Vec<String> = {
        let conn = rusqlite::Connection::open(&arch).unwrap();
        let mut stmt = conn.prepare("SELECT text FROM turn ORDER BY text").unwrap();
        let v = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        v
    };
    assert_eq!(archived, ["old and irreplaceable", "old but still on disk"]);
    // Its search index came with it, so the archive answers on its own.
    assert_eq!(count(&arch, "SELECT count(*) FROM turn_fts"), 2);

    // And TP_DB is what makes that reachable with the tools that already exist.
    let out = Command::new(env!("CARGO_BIN_EXE_tp"))
        .args(["search", "irreplaceable", "--index", "--since", "30d"])
        .env("HOME", home)
        .env("TP_DB", &arch)
        .output()
        .unwrap();
    // The FTS snippet brackets the match ("old and [irreplaceable]"), so the
    // session is what to assert on.
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains(old_gone), "{text}");
}

/// After archiving, an ordinary read must still cover both halves.
///
/// Splitting the corpus and asking the caller to remember the second file would
/// recreate the failure the rest of this suite exists for: an answer that reads
/// as "never discussed" when it means "not in the half you asked". So the
/// archive beside the index is consulted by every read, without a flag.
#[test]
fn reads_cover_the_archive_without_being_told() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp(ms / 1000, 0)
            .unwrap()
            .to_rfc3339()
    };
    let write = |uuid: &str, text: &str, ms: i64| {
        std::fs::write(
            proj.join(format!("{uuid}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({"type":"user","cwd":"/Users/test/dev/demo",
                                   "timestamp": iso(ms), "message":{"content": text}})
            ),
        )
        .unwrap();
    };
    let old = UUID;
    let recent = "33333333-4444-5555-6666-777777777777";
    write(old, "narwhal from long ago", now - 10 * 86_400_000);
    write(recent, "narwhal from today", now - 60_000);
    let (ok, _) = tp(home, &["index"]);
    assert!(ok);

    // Default destination — the one a read knows to look for without being told.
    let (ok, out) = tp(home, &["archive", "--before", "1d"]);
    assert!(ok, "{out}");
    assert!(home.join(".teleport/archive.db").exists(), "{out}");

    // Search finds BOTH halves in one call, no flag, no second command.
    let (ok, out) = tp(home, &["search", "narwhal", "--index", "--since", "30d"]);
    assert!(ok, "{out}");
    assert!(out.contains(old), "the archived half is missing: {out}");
    assert!(out.contains(recent), "the live half is missing: {out}");

    // Sessions likewise.
    let (ok, out) = tp(home, &["sessions", "--index", "--since", "30d"]);
    assert!(ok, "{out}");
    assert!(out.contains(old) && out.contains(recent), "{out}");

    // And reading an archived session BY ID works — the caller already knows it
    // exists, so "not found" would be the worst possible answer.
    let machine = tp(home, &["id"])
        .1
        .lines()
        .find_map(|l| l.strip_prefix("device id : ").map(str::to_string))
        .unwrap();
    let (ok, out) = tp(
        home,
        &["turns", &format!("{machine}/claude_code/{old}"), "--index"],
    );
    assert!(ok, "{out}");
    assert!(out.contains("narwhal from long ago"), "{out}");
}

/// Piping into a reader that quits early must exit, not panic.
///
/// Rust sets SIGPIPE to SIG_IGN before `main`, so the failed write returns EPIPE
/// and `println!` panics on it. Every command whose output outlives its reader
/// dies with a backtrace where `git` and `rg` exit silently — reported the same
/// day from a macOS install and a fresh Linux build, which is what a language
/// default looks like rather than a platform quirk.
///
/// Driven through a real pipe rather than by asserting on the signal
/// disposition: the disposition is the mechanism, and the mechanism is not the
/// promise. The promise is that stderr stays empty.
#[test]
fn a_reader_that_quits_early_does_not_panic() {
    use std::io::Read as _;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    fixture(home);

    // `version` prints several lines and needs no index; `sessions` exercises a
    // command whose output length varies with the corpus.
    for args in [vec!["version"], vec!["sessions", "--since", "30d"]] {
        let mut producer = Command::new(env!("CARGO_BIN_EXE_tp"))
            .args(&args)
            .env("HOME", home)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // Read ONE line, then drop the pipe — this is what `| head -1` does.
        {
            let mut out = producer.stdout.take().unwrap();
            let mut one = [0u8; 64];
            let _ = out.read(&mut one);
        }

        let mut stderr = String::new();
        producer
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        let _ = producer.wait();

        assert!(
            !stderr.contains("panicked") && !stderr.contains("Broken pipe"),
            "tp {args:?} panicked when its reader went away:\n{stderr}"
        );
    }
}

/// `tp backup` must record that it happened, and `tp version` must say so.
///
/// The unit tests cover the table. They cannot cover the thing that actually
/// broke this class of feature before: a store that works and a command that
/// never calls it. Same shape as the send half of the twin fix — the resolution
/// was correct and the caller did not ask.
#[test]
fn a_backup_is_recorded_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let iso = chrono::DateTime::from_timestamp(now / 1000, 0)
        .unwrap()
        .to_rfc3339();
    std::fs::write(
        proj.join(format!("{UUID}.jsonl")),
        format!(
            "{}\n",
            serde_json::json!({"type":"user","cwd":"/Users/test/dev/demo",
                               "timestamp": iso, "message":{"content":"a turn worth keeping"}})
        ),
    )
    .unwrap();
    let (ok, _) = tp(home, &["index"]);
    assert!(ok);

    // The transcript has to be GONE for any of this to apply: an index whose
    // sources all still exist is rebuildable and is told nothing about backups.
    std::fs::remove_file(proj.join(format!("{UUID}.jsonl"))).unwrap();

    let (ok, out) = tp(home, &["version"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("backup  NEVER"),
        "a never-backed-up index holding the only copy must say so: {out}"
    );

    let dest = home.join("snap.db");
    let (ok, out) = tp(home, &["backup", dest.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert!(!out.contains("could not record"), "{out}");

    let (ok, out) = tp(home, &["version"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("backup  today") && out.contains("snap.db"),
        "the recorded backup must be reported, with where it went: {out}"
    );
    assert!(
        !out.contains("NEVER"),
        "still claiming never after a backup: {out}"
    );
}

/// `tp reindex` must fail when a file it deleted rows for cannot be re-read.
///
/// The rows are dropped in a committed transaction and refilled afterwards, so
/// a refill that skips a file is data loss — and it used to be reported as
/// success, because `scan_root` turned every per-file failure into a log line
/// and returned only the counts that worked. Measured 2026-08-21: a reindex
/// cleared 28,182 sessions, one 44 MB transcript hit `database is locked`, and
/// the command exited 0 having destroyed 10,836 turns.
///
/// The file is made unreadable by permissions rather than by racing a daemon —
/// same failure class through `ingest_file`, deterministic instead of timing
/// dependent.
#[test]
fn reindex_fails_when_a_cleared_file_cannot_be_re_read() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let proj = home.join(".claude/projects/-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let iso = chrono::Utc::now().to_rfc3339();
    let write = |uuid: &str, text: &str| {
        let p = proj.join(format!("{uuid}.jsonl"));
        std::fs::write(
            &p,
            format!(
                "{}\n",
                serde_json::json!({"type":"user","cwd":"/Users/test/dev/demo",
                                   "timestamp": iso, "message":{"content": text}})
            ),
        )
        .unwrap();
        p
    };
    write(UUID, "readable");
    let doomed = write(
        "22222222-3333-4444-5555-666666666666",
        "about to be unreadable",
    );

    let (ok, _) = tp(home, &["index"]);
    assert!(ok);

    // Unreadable, but still present — `discover` finds it, `ingest_file` fails.
    let mut perms = std::fs::metadata(&doomed).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        perms.set_mode(0o000);
    }
    std::fs::set_permissions(&doomed, perms).unwrap();

    let (ok, out) = tp(home, &["reindex"]);
    assert!(
        !ok,
        "a reindex that could not re-read a cleared file must NOT exit 0:\n{out}"
    );
    // The specific message, not either message. Both guards produce a non-zero
    // exit — the fallback scan for sessions that came back empty would catch
    // this case too — so asserting only "it failed" cannot tell which one ran.
    // Verified by sabotage: disabling the unreadable-file check alone leaves the
    // fallback to pass the test, which is exactly the reassurance a test must
    // not give.
    assert!(
        out.contains("could not be re-read after their rows were cleared"),
        "the failure must come from the file-level check, not the empty-session \
         fallback — the point of returning failures is to stop BEFORE a full scan: \
         {out}"
    );
    assert!(
        out.contains("recoverable"),
        "the way out belongs in the message — the transcripts are still there: {out}"
    );

    // Restored, so tempdir cleanup does not trip over a 000 file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut p = std::fs::metadata(&doomed).unwrap().permissions();
        p.set_mode(0o644);
        let _ = std::fs::set_permissions(&doomed, p);
    }
}

/// The CLI half of the push-ingested false negative.
///
/// `a_runtime_the_scan_cannot_see_is_degraded_not_empty` pins the PROVIDER, and
/// pinning only that would have been reassurance: sabotaging the CLI's print
/// left the whole suite green while a user still saw a bare "no turns found".
/// The provider being right does not help anyone if the caller drops what it
/// said — the shape that already produced one bug in this repository (B1's
/// `tp backup`, where the store was correct and nothing asked it).
///
/// Asserted on the real binary's real output, because that is the only place
/// the two halves meet.
#[test]
fn a_session_the_scan_cannot_read_says_why_instead_of_nothing() {
    let home = tempfile::tempdir().unwrap();
    fixture(home.path());

    // A runtime with no adapter and no root here — the shape of any
    // push-ingested harness, whose turns reach the index through `tp ingest`
    // and never touch a file a scan could open.
    let (_ok, out) = tp(
        home.path(),
        &["turns", "someone-elses-machine/dsh/session-0bdc4c9f"],
    );
    assert!(
        out.contains("dsh"),
        "the runtime that cannot be read must be named, got:\n{out}"
    );
    assert!(
        out.contains("--index"),
        "and the provider that can must be pointed at, got:\n{out}"
    );

    // A session that is simply absent stays a plain empty answer. If every
    // empty read carried a warning the warning would mean nothing, which is the
    // way this kind of signal usually dies.
    let (_ok2, out2) = tp(
        home.path(),
        &["turns", "someone-elses-machine/claude_code/no-such-session"],
    );
    assert!(
        out2.contains("no turns found"),
        "expected the plain empty answer, got:\n{out2}"
    );
    assert!(
        !out2.contains("--index"),
        "an absent session must not be dressed up as a provider limitation, got:\n{out2}"
    );
}
