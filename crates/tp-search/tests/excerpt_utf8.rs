//! Regression: `excerpt_around` computed the match offset in `text.to_lowercase()`
//! but sliced into `text`. `to_lowercase()` is NOT byte-length preserving
//! (U+0130 'İ' is 2 bytes → 3 bytes; U+212A 'K' is 3 bytes → 1 byte), so the
//! index could land mid-codepoint and panic, or silently misalign the window.

use std::time::Duration;
use tp_core::retrieval::{Query, Scope};
use tp_search::{Retrieval, ScanProvider};

const MACHINE: &str = "m-utf8";

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp(ms / 1000, ((ms % 1000) * 1_000_000) as u32)
        .unwrap()
        .to_rfc3339()
}

/// Build a fixture whose turn text contains a char whose lowercase form has a
/// DIFFERENT utf-8 length, positioned before the search term.
fn fixture_with(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let line = serde_json::json!({
        "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": rfc3339(now),
        "message": {"content": text}
    });
    std::fs::write(
        proj.join("cccccccc-1111-2222-3333-444444444444.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();
    (dir, root)
}

fn search_for(root: &std::path::Path, needle: &str) -> Vec<String> {
    let r = Retrieval::new(Box::new(ScanProvider::new(
        MACHINE,
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![("claude_code".to_string(), root.to_path_buf())],
    )));
    let scope = Scope {
        folder: None,
        since: Duration::from_secs(3600),
        runtimes: vec![],
        until: None,
    };
    let q = Query {
        text: needle.to_string(),
        regex: false,
        include_thinking: false,
        limit: 10,
    };
    r.search(&q, &scope)
        .unwrap()
        .items
        .iter()
        .map(|h| h.excerpt().to_string())
        .collect()
}

/// U+212A KELVIN SIGN lowercases to 'k' (3 bytes → 1 byte): the lowercased
/// index runs BEHIND the real one, landing inside a multi-byte char.
#[test]
fn kelvin_sign_before_match_does_not_panic() {
    let text = "\u{212A}\u{1F600}payload NEEDLE tail";
    let (_tmp, root) = fixture_with(text);
    let hits = search_for(&root, "needle");
    assert_eq!(hits.len(), 1, "case-insensitive match must still be found");
    assert!(
        hits[0].to_lowercase().contains("needle"),
        "excerpt must contain the match, got: {:?}",
        hits[0]
    );
}

/// U+0130 LATIN CAPITAL I WITH DOT lowercases to "i\u{307}" (2 bytes → 3 bytes):
/// the lowercased index runs AHEAD, and with enough of them can exceed text.len().
#[test]
fn turkish_dotted_i_before_match_does_not_panic() {
    let text = "\u{0130}\u{0130}\u{0130}\u{1F600}payload NEEDLE tail";
    let (_tmp, root) = fixture_with(text);
    let hits = search_for(&root, "needle");
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].to_lowercase().contains("needle"),
        "excerpt must contain the match, got: {:?}",
        hits[0]
    );
}

/// Silent-misalignment variant: enough length-shrinking chars that the window
/// slides off the match entirely, without panicking.
#[test]
fn excerpt_window_stays_on_the_match() {
    let prefix = "\u{212A}".repeat(40); // 40 chars, 120 bytes → 40 bytes lowercased
    let text = format!("{prefix} some filler words here to push things along NEEDLE tail");
    let (_tmp, root) = fixture_with(&text);
    let hits = search_for(&root, "needle");
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].to_lowercase().contains("needle"),
        "excerpt window drifted off the match: {:?}",
        hits[0]
    );
}
