//! Regression: scan built the excerpt FIRST and scrubbed the excerpt.
//! Redaction rules anchored on a closing delimiter — `"accessToken":"…"` needs
//! the closing quote, a private key needs `-----END` — silently no-op once the
//! window cuts the closer off, emitting raw credential material. Scrubbing must
//! happen on the full field, before truncation.

use std::time::Duration;
use tp_core::retrieval::{Query, Scope};
use tp_search::{Retrieval, ScanProvider};

const MACHINE: &str = "m-redact";

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp(ms / 1000, ((ms % 1000) * 1_000_000) as u32)
        .unwrap()
        .to_rfc3339()
}

fn fixture(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
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
        proj.join("dddddddd-1111-2222-3333-444444444444.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();
    (dir, root)
}

fn scan(root: &std::path::Path) -> Retrieval {
    Retrieval::new(Box::new(ScanProvider::new(
        MACHINE,
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![("claude_code".to_string(), root.to_path_buf())],
    )))
}

fn wide() -> Scope {
    Scope {
        folder: None,
        since: Duration::from_secs(3600),
        runtimes: vec![],
        until: None,
    }
}

fn q(text: &str) -> Query {
    Query {
        text: text.to_string(),
        regex: false,
        include_thinking: true,
        limit: 10,
    }
}

/// A token with no distinctive prefix: only the delimiter-anchored
/// `"accessToken":"…"` rule can catch it, so truncation defeats it.
#[test]
fn access_token_survives_excerpt_truncation() {
    const SECRET: &str = "AbCdEf0123456789XyZwVuTsRqPoNmLkJiHgFeDcBa";
    // Long lead-in so the excerpt window cuts before the closing quote.
    let filler = "context ".repeat(20);
    let text =
        format!("{filler}accessToken plumbing: {{\"accessToken\":\"{SECRET}\",\"other\":1}}");
    let (_t, root) = fixture(&text);

    let hits = scan(&root).search(&q("accessToken"), &wide()).unwrap();
    assert!(!hits.items.is_empty(), "must find the match");
    for h in &hits.items {
        assert!(
            !h.excerpt().contains(SECRET),
            "excerpt leaked the raw access token: {:?}",
            h.excerpt()
        );
    }
}

/// A private key body: the rule needs `-----END`, which a truncated window drops.
#[test]
fn private_key_body_survives_excerpt_truncation() {
    const BODY: &str =
        "MIIEowIBAAKCAQEAx7Qk9vTn2sLwR4pQzYbN8mHcVdF3jKlO5uWpXaZ2rEsTgYhUiIjKlMnOpQrStUvWxYz";
    let filler = "notes ".repeat(20);
    let text =
        format!("{filler}-----BEGIN RSA PRIVATE KEY-----\n{BODY}\n-----END RSA PRIVATE KEY-----");
    let (_t, root) = fixture(&text);

    let hits = scan(&root).search(&q("PRIVATE KEY"), &wide()).unwrap();
    assert!(!hits.items.is_empty(), "must find the match");
    for h in &hits.items {
        assert!(
            !h.excerpt().contains(&BODY[..40]),
            "excerpt leaked private key material: {:?}",
            h.excerpt()
        );
    }
}

/// Titles are truncated to 200 chars — same hazard on the sessions() path.
#[test]
fn session_title_scrubs_before_truncating() {
    const SECRET: &str = "AbCdEf0123456789XyZwVuTsRqPoNmLkJiHgFeDcBa";
    let filler = "intro ".repeat(30); // pushes the closing quote past 200 chars
    let text = format!("{filler}{{\"accessToken\":\"{SECRET}\"}}");
    let (_t, root) = fixture(&text);

    let got = scan(&root).sessions(&wide(), 10).unwrap();
    assert_eq!(got.items.len(), 1);
    if let Some(title) = &got.items[0].title {
        assert!(
            !title.contains(SECRET),
            "session title leaked a token: {title:?}"
        );
    }
}
