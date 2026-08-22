//! Regression: the index provider applied the time window AFTER SQL's LIMIT,
//! so `LIMIT n` selected the n best-ranked matches across ALL history and the
//! window filter then discarded them. With many old matches and a small
//! --limit, a query returned a confident "no matches" while thousands of
//! in-window matches existed. Found by running the real CLI against a real
//! 350k-turn index, not by reading the code — the comment there described it
//! as a harmless simplification.

use std::path::PathBuf;
use std::time::Duration;
use tp_core::retrieval::{Query, RetrievalProvider, Scope};
use tp_core::turn::{NormalizedTurn, ParseChunk, Role, SessionMeta};
use tp_core::SessionId;
use tp_search::IndexProvider;

const MACHINE: &str = "M";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn turn(ts: i64, text: &str) -> NormalizedTurn {
    NormalizedTurn {
        role: Role::User,
        ts: Some(ts),
        text: text.to_string(),
        thinking: String::new(),
        thinking_opaque: false,
        tool_calls: vec![],
        surface: Default::default(),
        tokens_in: None,
        tokens_out: None,
        prov: Default::default(),
    }
}

/// Build an index where OLD matches vastly outnumber recent ones — the shape
/// that makes a rank-ordered `LIMIT` fill up entirely with out-of-window rows.
fn seed(dir: &std::path::Path) -> PathBuf {
    let db_path = dir.join("teleport.db");
    let mut db = tp_db::Db::open(&db_path).unwrap();
    db.ensure_self_machine(MACHINE, "TestMac").unwrap();
    db.ensure_runtime("claude_code", "/tmp/root").unwrap();

    let day = 24 * 3600 * 1000;
    let now = now_ms();

    let mut commit = |native: &str, ts: i64, text: &str, inode: i64| {
        let chunk = ParseChunk {
            turns: vec![turn(ts, text)],
            new_offset: 0,
            meta: SessionMeta {
                cwd: Some("/tmp/p".into()),
                title_derived: Some(native.into()),
                started_at: Some(ts),
                ..Default::default()
            },
            ..Default::default()
        };
        let sid = SessionId::new(MACHINE, "claude_code", native).to_string();
        tp_db::writer::commit_chunk(
            db.conn_mut(),
            &sid,
            MACHINE,
            "claude_code",
            native,
            &format!("/tmp/root/{native}.jsonl"),
            inode,
            ts,
            &chunk,
        )
        .unwrap();
    };

    // 50 old matches, all far outside a 6h window.
    for i in 0..50 {
        commit(
            &format!("old-{i}"),
            now - 30 * day,
            "teleport teleport teleport",
            i as i64 + 1,
        );
    }
    // One recent match, inside the window.
    commit(
        "fresh",
        now - 60_000,
        "teleport is what we are building",
        999,
    );

    db_path
}

#[test]
fn small_limit_does_not_hide_in_window_matches() {
    let dir = tempfile::tempdir().unwrap();
    let p = IndexProvider::new(seed(dir.path()));

    let scope = Scope {
        folder: None,
        since: Duration::from_secs(6 * 3600),
        runtimes: vec![],
        until: None,
    };
    let q = Query {
        text: "teleport".into(),
        regex: false,
        include_thinking: false,
        limit: 2,
    };

    let got = p.search(&q, &scope).unwrap();
    assert!(
        !got.items.is_empty(),
        "50 out-of-window matches must not consume the LIMIT and hide the in-window one"
    );
    assert!(
        got.items.iter().all(|h| h.at.session_id.ends_with("fresh")),
        "every returned hit must be inside the window, got: {:?}",
        got.items
            .iter()
            .map(|h| &h.at.session_id)
            .collect::<Vec<_>>()
    );
}

/// The window must still be honoured when it excludes everything — a real
/// empty result is fine, it is the *false* empty that is the bug.
#[test]
fn window_that_excludes_everything_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let p = IndexProvider::new(seed(dir.path()));

    let scope = Scope {
        folder: None,
        since: Duration::from_secs(1),
        runtimes: vec![],
        until: None,
    };
    let q = Query {
        text: "teleport".into(),
        regex: false,
        include_thinking: false,
        limit: 10,
    };
    assert!(
        p.search(&q, &scope).unwrap().items.is_empty(),
        "a 1s window must exclude all seeded turns"
    );
}

/// A wide window must reach the old rows — proving the fix filters by time
/// rather than simply preferring recent rows.
#[test]
fn wide_window_reaches_old_matches() {
    let dir = tempfile::tempdir().unwrap();
    let p = IndexProvider::new(seed(dir.path()));

    let scope = Scope {
        folder: None,
        since: Duration::from_secs(365 * 24 * 3600),
        runtimes: vec![],
        until: None,
    };
    let q = Query {
        text: "teleport".into(),
        regex: false,
        include_thinking: false,
        limit: 51,
    };
    let got = p.search(&q, &scope).unwrap();
    assert_eq!(
        got.items.len(),
        51,
        "all 51 matches are inside a one-year window"
    );
}

/// A hit must always carry evidence of WHY it matched. `snippet()` takes one
/// column index, so a turn whose only content is a tool call (text = '')
/// previewed as an empty string — a coordinate with no excerpt.
#[test]
fn tool_call_only_turn_still_gets_an_excerpt() {
    use tp_core::turn::ToolCallDigest;
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("t.db");
    let mut db = tp_db::Db::open(&db_path).unwrap();
    db.ensure_self_machine(MACHINE, "TestMac").unwrap();
    db.ensure_runtime("claude_code", "/tmp/root").unwrap();

    let now = now_ms();
    let mut t = turn(now, ""); // no text at all…
    t.role = Role::Assistant;
    t.tool_calls = vec![ToolCallDigest {
        // …the match lives only here
        name: "Bash".into(),
        input_digest: Some("{\"command\":\"grep teleport\"}".into()),
    }];
    let chunk = ParseChunk {
        turns: vec![t],
        new_offset: 0,
        meta: SessionMeta {
            cwd: Some("/tmp/p".into()),
            title_derived: None,
            started_at: Some(now),
            ..Default::default()
        },
        ..Default::default()
    };
    let sid = SessionId::new(MACHINE, "claude_code", "toolonly").to_string();
    tp_db::writer::commit_chunk(
        db.conn_mut(),
        &sid,
        MACHINE,
        "claude_code",
        "toolonly",
        "/tmp/root/toolonly.jsonl",
        7,
        now,
        &chunk,
    )
    .unwrap();

    let p = IndexProvider::new(db_path);
    let scope = Scope {
        folder: None,
        since: Duration::from_secs(3600),
        runtimes: vec![],
        until: None,
    };
    let q = Query {
        text: "teleport".into(),
        regex: false,
        include_thinking: false,
        limit: 10,
    };
    let got = p.search(&q, &scope).unwrap();

    assert_eq!(got.items.len(), 1, "the tool-call match must be found");
    assert!(
        !got.items[0].excerpt.trim().is_empty(),
        "a hit must never come back with a blank excerpt"
    );
    assert!(
        got.items[0].excerpt.to_lowercase().contains("teleport"),
        "the excerpt must show the match, got: {:?}",
        got.items[0].excerpt
    );
}

/// The two backends must report the SAME title for the same file.
///
/// They did not, briefly: the index learned to read Claude Code's `ai-title`
/// entries while the scan kept deriving from the first user message, so
/// `tp sessions` and `tp --index sessions` printed different names for one
/// session. LLD §16 rule 1 forbids exactly that, and nothing caught it until a
/// smoke test on real data showed the two side by side.
#[test]
fn both_backends_report_the_same_native_title() {
    use tp_ingest::adapter::Adapter;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects").join("-p");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("s.jsonl");

    // A real shape: an opening user message, then the runtime naming the session
    // LATER — which is what a rename is, and what a head-only read misses.
    let body = format!(
        "{}\n{}\n",
        serde_json::json!({
            "type": "user", "cwd": "/p", "timestamp": "2026-08-16T00:00:00Z",
            "uuid": "u1", "message": {"role": "user", "content": "search internet for a serving box"}
        }),
        serde_json::json!({"type": "ai-title", "aiTitle": "Best serving solution", "sessionId": "s"})
    );
    std::fs::write(&path, body).unwrap();

    let cfg: tp_ingest::adapter::decl::DeclConfig = toml::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../install/runtimes.d/claude_code.toml"),
        )
        .unwrap(),
    )
    .unwrap();
    let adapter = tp_ingest::adapter::decl::DeclAdapter::new(cfg);

    // Index side.
    let chunk = adapter.parse_from(&path, 0).unwrap();
    assert_eq!(
        chunk.meta.title_ai.as_deref(),
        Some("Best serving solution"),
        "the index must read the stated title"
    );

    // Scan side: the same file through the same adapter, line by line, which is
    // what `head_meta` does.
    let stated = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter_map(|l| adapter.title_of_line(l))
        .next_back();
    assert_eq!(
        stated.map(|(_, t)| t).as_deref(),
        chunk.meta.title_ai.as_deref(),
        "scan and index must agree about the title"
    );
}
