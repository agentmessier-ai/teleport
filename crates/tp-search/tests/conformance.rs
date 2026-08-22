//! Provider conformance: the SAME assertions run against BOTH the scan and the
//! index provider (LLD §16 "Verification requirement"). An abstraction claimed
//! but exercised through one implementation is not a seam.
//!
//! Any divergence must be either fixed or declared in `Capabilities` — those
//! are the only two legal outcomes, so the tests below assert on capabilities
//! wherever the two legitimately differ.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tp_core::retrieval::{Query, Scope, TurnCursor};
use tp_core::SessionId;
use tp_search::{IndexProvider, Retrieval, ScanProvider};

const MACHINE: &str = "m-test";
const SESSION_UUID: &str = "11111111-2222-3333-4444-555555555555";

/// Build a fake `~/.claude/projects` tree with one session containing a secret
/// in `thinking` — the case the redaction funnel must catch on every backend.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let ts = |off: i64| {
        let dt = now_ms - off;
        chrono_fmt(dt)
    };

    let lines = [
        serde_json::json!({
            "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": ts(5000),
            "message": {"content": "how do we handle pagination"}
        }),
        serde_json::json!({
            "type": "assistant", "timestamp": ts(4000),
            "message": {"content": [
                {"type": "thinking", "thinking": "the token is sk-ant-oat01-SECRETSECRETSECRETSECRET and pagination uses cursors"},
                {"type": "text", "text": "use a cursor-based approach"},
                {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}}
            ], "usage": {"input_tokens": 10, "output_tokens": 20}}
        }),
        // Non-conversational record: both backends must skip it identically.
        serde_json::json!({"type": "queue-operation", "timestamp": ts(3000)}),
    ];
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(proj.join(format!("{SESSION_UUID}.jsonl")), body).unwrap();
    (dir, root)
}

fn chrono_fmt(ms: i64) -> String {
    // RFC3339, which the adapter parses.
    let secs = ms / 1000;
    let nsec = ((ms % 1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nsec)
        .unwrap()
        .to_rfc3339()
}

fn scan_provider(root: &Path) -> Retrieval {
    Retrieval::new(Box::new(ScanProvider::new(
        MACHINE,
        vec![Box::new(tp_ingest::builtin("claude_code"))],
        vec![("claude_code".to_string(), root.to_path_buf())],
    )))
}

/// Build an index over the same fixture, then return a provider reading it.
fn index_provider(root: &Path, db_dir: &std::path::Path) -> Retrieval {
    use tp_ingest::Adapter;
    let db_path = db_dir.join("t.db");
    let mut db = tp_db::Db::open(&db_path).unwrap();
    db.ensure_self_machine(MACHINE, "TestMac").unwrap();
    db.ensure_runtime("claude_code", root.to_str().unwrap())
        .unwrap();

    let adapter = tp_ingest::builtin("claude_code");
    for src in adapter.discover(root).unwrap() {
        let mut chunk = adapter.parse_from(&src.path, 0).unwrap();
        for t in &mut chunk.turns {
            tp_ingest::redact::redact(t);
        }
        let sid = SessionId::new(MACHINE, "claude_code", &src.native_id).to_string();
        tp_db::writer::commit_chunk(
            db.conn_mut(),
            &sid,
            MACHINE,
            "claude_code",
            &src.native_id,
            src.path.to_str().unwrap(),
            src.inode,
            src.mtime_ms,
            &chunk,
        )
        .unwrap();
    }
    Retrieval::new(Box::new(IndexProvider::new(db_path)))
}

fn wide_scope() -> Scope {
    Scope {
        folder: None,
        since: Duration::from_secs(3600),
        runtimes: vec![],
        until: None,
    }
}

fn q(text: &str, include_thinking: bool) -> Query {
    Query {
        text: text.to_string(),
        regex: false,
        include_thinking,
        limit: 50,
    }
}

// ── The conformance body: identical assertions, run per provider ────────────

fn assert_finds_text(r: &Retrieval) {
    let got = r.search(&q("pagination", false), &wide_scope()).unwrap();
    assert!(
        !got.items.is_empty(),
        "[{}] must find 'pagination' in text",
        r.provider_name()
    );
    assert!(
        got.items
            .iter()
            .all(|h| h.at.session_id.ends_with(SESSION_UUID)),
        "[{}] hits must carry the composite session id",
        r.provider_name()
    );
    assert!(
        got.items.iter().all(|h| h.at.ts.is_some()),
        "[{}] every hit must carry the universal (session_id, ts) coordinate",
        r.provider_name()
    );
}

fn assert_thinking_gate(r: &Retrieval) {
    let name = r.provider_name();
    // "cursors" appears ONLY inside thinking.
    let hidden = r.search(&q("cursors", false), &wide_scope()).unwrap();
    assert!(
        hidden.items.is_empty(),
        "[{name}] thinking must not be searched when include_thinking=false"
    );

    if r.capabilities().search_thinking {
        let shown = r.search(&q("cursors", true), &wide_scope()).unwrap();
        assert!(
            !shown.items.is_empty(),
            "[{name}] thinking must be searchable when opted in"
        );
    }
}

/// The security-critical invariant: no backend may emit an unredacted secret,
/// regardless of whether it scrubbed at write time or read time.
fn assert_redaction_on_every_path(r: &Retrieval) {
    let name = r.provider_name();
    const SECRET: &str = "sk-ant-oat01-SECRETSECRETSECRETSECRET";

    let hits = r.search(&q("pagination", true), &wide_scope()).unwrap();
    for h in &hits.items {
        assert!(
            !h.excerpt().contains(SECRET),
            "[{name}] search excerpt leaked a secret"
        );
    }

    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);
    let turns = r
        .turns(&sid, TurnCursor::Start, true, 50, None)
        .unwrap()
        .items;
    assert!(
        !turns.is_empty(),
        "[{name}] turns() must return the session's turns"
    );
    for t in &turns {
        assert!(
            !t.text.contains(SECRET),
            "[{name}] turn text leaked a secret"
        );
        assert!(
            !t.thinking.contains(SECRET),
            "[{name}] turn thinking leaked a secret"
        );
    }
    assert!(
        turns
            .iter()
            .any(|t| t.thinking.contains("[redacted:anthropic-key]")),
        "[{name}] the secret must be replaced by a visible placeholder, not silently dropped"
    );
}

fn assert_sessions_listed(r: &Retrieval) {
    let got = r.sessions(&wide_scope(), 10).unwrap();
    assert_eq!(
        got.items.len(),
        1,
        "[{}] must list exactly the one fixture session",
        r.provider_name()
    );
    let s = &got.items[0];
    assert!(s.id.ends_with(SESSION_UUID));
    assert_eq!(
        s.cwd.as_deref(),
        Some("/Users/test/dev/demo"),
        "[{}] cwd",
        r.provider_name()
    );
}

fn assert_turn_cursor(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);
    let all = r
        .turns(&sid, TurnCursor::Start, false, 50, None)
        .unwrap()
        .items;
    assert_eq!(
        all.len(),
        2,
        "[{name}] exactly 2 conversational turns (queue-operation must be skipped)"
    );

    let first_ts = all[0].ts.expect("ts present");
    let rest = r
        .turns(&sid, TurnCursor::AfterTs(first_ts), false, 50, None)
        .unwrap()
        .items;
    assert_eq!(
        rest.len(),
        1,
        "[{name}] AfterTs must exclude turns at or before the cursor"
    );
}

/// The byte budget must cut at the SAME place on both backends, and must say
/// so. `limit` bounds turn COUNT, which bounds nothing a caller cares about —
/// a turn is as big as whatever the other session wrote, so an unbounded
/// `turns` call can evict the caller's context with no warning. A truncated
/// read that looks complete is the same class of failure as a truncated search
/// reported as exhaustive (LLD §16 rule 3).
fn assert_turn_byte_budget(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);

    // A starved budget still yields exactly one turn: the first is admitted
    // (already per-turn capped) so an oversized head can't make a real session
    // read as "empty".
    let tiny = r
        .turns(&sid, TurnCursor::Start, false, 50, Some(1))
        .unwrap();
    assert_eq!(
        tiny.items.len(),
        1,
        "[{name}] a starved budget must still return the first turn, not nothing"
    );
    assert!(
        tiny.coverage.truncated,
        "[{name}] stopping early MUST be reported as truncated"
    );
    assert!(
        tiny.items[0].ts.is_some(),
        "[{name}] a truncated read must carry a ts to resume from"
    );

    // A generous budget returns everything and does NOT claim truncation.
    let full = r
        .turns(&sid, TurnCursor::Start, false, 50, Some(10_000_000))
        .unwrap();
    assert_eq!(full.items.len(), 2, "[{name}] full read returns both turns");
    assert!(
        !full.coverage.truncated,
        "[{name}] a complete read must not be reported as truncated"
    );

    // Resuming from the truncated read reaches what was cut off — the budget
    // delays turns, it never drops them.
    let resumed = r
        .turns(
            &sid,
            TurnCursor::AfterTs(tiny.items[0].ts.unwrap()),
            false,
            50,
            Some(10_000_000),
        )
        .unwrap();
    assert_eq!(
        resumed.items.len(),
        1,
        "[{name}] resuming after a truncated read must return the remainder"
    );
}

/// A query containing `/` must behave the same on both backends. This was once
/// reported as a real divergence and written into the README; it was not — the
/// two commands being compared used different `--since` windows. The FTS path
/// quotes the query into a phrase, so `claude/settings` tokenizes to the same
/// adjacent pair on both sides. Locked here so a future tokenizer or
/// query-builder change can't quietly make the false claim true.
fn assert_punctuated_query_parity(r: &Retrieval) {
    let name = r.provider_name();
    let scope = Scope {
        folder: None,
        since: std::time::Duration::from_secs(86_400 * 3650),
        runtimes: vec![],
        until: None,
    };
    for probe in ["claude/settings", "a-b", "x.y"] {
        let q = Query {
            text: probe.to_string(),
            regex: false,
            include_thinking: false,
            limit: 20,
        };
        // The corpus need not contain them; what must hold is that punctuation
        // doesn't make the query ERROR or behave specially on one backend only.
        let got = r.search(&q, &scope);
        assert!(
            got.is_ok(),
            "[{name}] a punctuated query must not fail: {probe:?}"
        );
    }
}

/// `--folder` took the cwd the tool itself PRINTS and returned nothing on scan
/// while the index answered correctly — a provider divergence that survived
/// because every case above passes `folder: None`, so the suite whose whole job
/// is keeping the two honest never exercised the filter at all.
///
/// The scan layer prunes on the transcript PATH (before opening anything, which
/// is the point) and that path is the ENCODED cwd — `-Users-test-dev-demo` — so
/// a real path could never match it. A silent empty result is the worst possible
/// shape for this: it reads as "you never worked there".
fn assert_folder_filter(r: &Retrieval) {
    let name = r.provider_name();
    let scoped = |folder: &str| Scope {
        folder: Some(folder.to_string()),
        since: Duration::from_secs(3600),
        runtimes: vec![],
        until: None,
    };

    // The exact string `tp sessions` displays for this fixture.
    for needle in ["/Users/test/dev/demo", "/Users/test/dev/demo/", "demo"] {
        let got = r.sessions(&scoped(needle), 10).unwrap();
        assert!(
            !got.items.is_empty(),
            "[{name}] --folder {needle:?} must find the session whose cwd IS that path"
        );
    }

    // Widening must not have made the filter meaningless.
    let none = r
        .sessions(&scoped("/Users/test/dev/unrelated-project"), 10)
        .unwrap();
    assert!(
        none.items.is_empty(),
        "[{name}] --folder must still exclude a non-matching folder"
    );

    // The SAME filter, through `search` — the half this case used to skip.
    //
    // Testing only `sessions` is how the index provider went on ignoring
    // `--folder` in `search` long after the scan provider was fixed here: one
    // method of one provider honoured the filter and the other dropped it, and
    // nothing compared them. Reported from a real session, whose four different
    // `--folder` values all returned one identical global result set.
    //
    // The negative case is the load-bearing one. An over-broad result is far
    // worse than an error here: it reads as "I searched only there, and this is
    // all there is", which is a wrong answer wearing the shape of a right one.
    for needle in ["/Users/test/dev/demo", "demo"] {
        let got = r.search(&q("pagination", false), &scoped(needle)).unwrap();
        assert!(
            !got.items.is_empty(),
            "[{name}] search --folder {needle:?} must still find hits in that folder"
        );
    }
    let out_of_folder = r
        .search(
            &q("pagination", false),
            &scoped("/Users/test/dev/zzz-nonexistent"),
        )
        .unwrap();
    assert!(
        out_of_folder.items.is_empty(),
        "[{name}] search --folder for a folder with no sessions must return NOTHING, \
         got {} hit(s) — a non-existent folder returning the full unfiltered result set \
         reads as a complete answer to a question that was never asked",
        out_of_folder.items.len()
    );
}

/// A window read keeps the NEWEST turns; `AfterTs` keeps the oldest. Both
/// providers must agree on that, and on `before_ms` being exclusive — paging
/// backwards passes the previous page's earliest `ts`, so an inclusive bound
/// would return that turn on every page.
fn assert_window_cursor(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let wide = TurnCursor::Window {
        since_ms: now - 3_600_000,
        before_ms: None,
    };

    let all = r.turns(&sid, TurnCursor::Start, false, 50, None).unwrap();
    let win = r.turns(&sid, wide, false, 50, None).unwrap();
    assert_eq!(
        win.items.len(),
        all.items.len(),
        "[{name}] a window covering the whole session must return all of it"
    );

    // Overflow: the window keeps the LAST turn, the forward read keeps the FIRST.
    let one_win = r.turns(&sid, wide, false, 1, None).unwrap();
    let one_fwd = r.turns(&sid, TurnCursor::Start, false, 1, None).unwrap();
    assert_eq!(one_win.items.len(), 1, "[{name}] window honours limit");
    assert_eq!(
        one_win.items[0].ts,
        all.items.last().unwrap().ts,
        "[{name}] an overflowing window must keep the NEWEST turn"
    );
    assert_eq!(
        one_fwd.items[0].ts,
        all.items.first().unwrap().ts,
        "[{name}] an overflowing forward read must keep the OLDEST turn"
    );
    assert!(
        one_win.coverage.truncated,
        "[{name}] dropping turns must be reported, never silent"
    );

    // `before_ms` is exclusive, which is what makes paging back terminate.
    let cut = all.items.last().unwrap().ts.unwrap();
    let paged = r
        .turns(
            &sid,
            TurnCursor::Window {
                since_ms: now - 3_600_000,
                before_ms: Some(cut),
            },
            false,
            50,
            None,
        )
        .unwrap();
    assert!(
        paged.items.iter().all(|t| t.ts.unwrap() < cut),
        "[{name}] before_ms must be exclusive"
    );
    assert_eq!(
        paged.items.len(),
        all.items.len() - 1,
        "[{name}] paging back must return everything before the cut, and only that"
    );
}

/// The fixture's assistant turn calls `Bash`. An index read used to hardcode
/// `tool_calls: vec![]`, so the same turn carried its tools from a scan and
/// arrived empty from the index — and a tool-only turn then renders as though
/// nothing was in it.
fn assert_tool_calls_survive(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);
    let turns = r
        .turns(&sid, TurnCursor::Start, false, 50, None)
        .unwrap()
        .items;
    assert!(
        turns
            .iter()
            .any(|t| t.tool_calls.iter().any(|c| c.name == "Bash")),
        "[{name}] a turn's tool calls must survive the read"
    );
}

/// `since`/`until` must mean the same thing to both providers, for both
/// `search` and `sessions`.
///
/// They did not. The scan side pruned files by mtime; the index side's
/// `list_sessions` ignored the window entirely, so `--since 1h` and `--since
/// 30d` returned identical lists there. And neither had an upper bound at all,
/// which is why "which sessions were active on the 4th" had no answer — only
/// "in the last N days", which on the 11th cannot mean the 4th.
///
/// The fixture's turns are seconds old, so a window that ended an hour ago must
/// come back EMPTY on both. An empty result is the honest answer here; the bug
/// shape being guarded against is returning recent data for an old window.
fn assert_time_window(r: &Retrieval) {
    let name = r.provider_name();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let scoped = |since: Duration, until: Option<i64>| Scope {
        folder: None,
        since,
        runtimes: vec![],
        until,
    };

    // A window that ended an hour ago holds none of the fixture's turns.
    let past = scoped(Duration::from_secs(86_400), Some(now - 3_600_000));
    assert!(
        r.sessions(&past, 10).unwrap().items.is_empty(),
        "[{name}] sessions must honour `until` — a window that ended before the \
         session existed cannot contain it"
    );
    assert!(
        r.search(&q("pagination", false), &past)
            .unwrap()
            .items
            .is_empty(),
        "[{name}] search must honour `until`"
    );

    // The same window extended to now finds it again, so the emptiness above was
    // the bound doing its job and not the query being broken.
    let live = scoped(Duration::from_secs(86_400), None);
    assert_eq!(
        r.sessions(&live, 10).unwrap().items.len(),
        1,
        "[{name}] the same window ending now must find the session"
    );
    assert!(
        !r.search(&q("pagination", false), &live)
            .unwrap()
            .items
            .is_empty(),
        "[{name}] the same window ending now must find the hit"
    );

    // Lower bound: a window that starts after the turns were written.
    let future = scoped(Duration::from_millis(1), None);
    assert!(
        r.sessions(&future, 10).unwrap().items.is_empty(),
        "[{name}] sessions must honour `since` — the index ignored it entirely"
    );
}

/// A degenerate `limit` must not be answered with "this window is empty".
///
/// `WindowBuffer` states the rule (retrieval.rs:271: "returning nothing would
/// read as 'this window is empty'") and the scan backend implemented it. The
/// index backend did not, and nothing drove both here: `limit as i64` made a
/// limit above `i64::MAX` wrap negative, `limit + 1` turned that into `LIMIT 0`,
/// and SQLite returned nothing — so "give me as many as you can" produced zero
/// while `--limit 5` produced five. A limit of ZERO, which a person types, did
/// the same. Measured on the real corpus before the fix: index 0 turns and scan
/// 1 at limit 0; index 0 and scan 361 at limit 18446744073709551615, for a
/// session holding 6,756 turns.
fn assert_degenerate_limits(r: &Retrieval) {
    let name = r.provider_name();
    let sid = SessionId::new(MACHINE, "claude_code", SESSION_UUID);

    for (label, limit) in [("zero", 0usize), ("usize::MAX", usize::MAX)] {
        let got = r
            .turns(&sid, TurnCursor::Start, false, limit, None)
            .unwrap()
            .items;
        assert!(
            !got.is_empty(),
            "[{name}] limit {label}: returned nothing, which reads as an empty window"
        );
    }

    // And the useful half: an enormous limit means EVERYTHING, not one.
    let all = r
        .turns(&sid, TurnCursor::Start, false, usize::MAX, None)
        .unwrap()
        .items;
    assert_eq!(
        all.len(),
        2,
        "[{name}] an enormous limit must return the whole session"
    );
}

fn run_conformance(r: &Retrieval) {
    assert_finds_text(r);
    assert_time_window(r);
    assert_window_cursor(r);
    assert_tool_calls_survive(r);
    assert_thinking_gate(r);
    assert_redaction_on_every_path(r);
    assert_sessions_listed(r);
    assert_folder_filter(r);
    assert_turn_byte_budget(r);
    assert_punctuated_query_parity(r);
    assert_turn_cursor(r);
    assert_degenerate_limits(r);
}

#[test]
fn scan_provider_conforms() {
    let (_tmp, root) = fixture();
    run_conformance(&scan_provider(&root));
}

#[test]
fn index_provider_conforms() {
    let (_tmp, root) = fixture();
    let dbdir = tempfile::tempdir().unwrap();
    run_conformance(&index_provider(&root, dbdir.path()));
}

/// The declared differences. If either of these flips, the divergence is no
/// longer declared and the conformance suite above must grow to cover it.
#[test]
fn capabilities_declare_the_legitimate_differences() {
    let (_tmp, root) = fixture();
    let dbdir = tempfile::tempdir().unwrap();
    let scan = scan_provider(&root);
    let index = index_provider(&root, dbdir.path());

    assert!(
        !scan.capabilities().ranked,
        "scan cannot rank without an index"
    );
    assert!(index.capabilities().ranked, "index provides bm25");

    assert!(
        !scan.capabilities().unscoped_ok,
        "an unscoped scan is the expensive path"
    );
    assert!(index.capabilities().unscoped_ok);

    // regex is scan-only; the index must REFUSE such a query rather than
    // answering it with a literal-phrase search and reporting "no matches".
    assert!(scan.capabilities().regex);
    assert!(!index.capabilities().regex);
    let rq = Query {
        text: "ne+dle".to_string(),
        regex: true,
        include_thinking: false,
        limit: 10,
    };
    assert!(
        scan.search(&rq, &wide_scope()).is_ok(),
        "scan must accept a regex query"
    );
    let err = index.search(&rq, &wide_scope()).unwrap_err().to_string();
    assert!(
        err.contains("regex"),
        "index must refuse a regex query explicitly, got: {err}"
    );

    // …and the CLI must therefore warn for scan, but not for index.
    let broad = Scope {
        folder: None,
        since: Duration::from_secs(30 * 86400),
        runtimes: vec![],
        until: None,
    };
    assert!(
        scan.scope_warning(&broad).is_some(),
        "scan must warn before an expensive scope"
    );
    assert!(
        index.scope_warning(&broad).is_none(),
        "index needs no such warning"
    );
}

/// The false-negative reset: an index that does not exist (or is empty) must
/// REPORT that, not silently return "no matches" — otherwise a user concludes
/// "never discussed X" from an index that has nothing in it (LLD §16 rule 3).
#[test]
fn empty_or_missing_index_is_degraded_not_empty() {
    let dbdir = tempfile::tempdir().unwrap();
    let missing = dbdir.path().join("does-not-exist.db");
    let r = Retrieval::new(Box::new(IndexProvider::new(missing.clone())));
    let got = r.search(&q("anything", false), &wide_scope()).unwrap();
    assert!(got.items.is_empty());
    assert!(
        got.coverage
            .degraded
            .as_deref()
            .unwrap_or("")
            .contains("does not exist"),
        "missing index must degrade with a build hint, got: {:?}",
        got.coverage.degraded
    );

    // An existing-but-empty index (created by Db::open) must also degrade.
    let empty_path = dbdir.path().join("empty.db");
    let _db = tp_db::Db::open(&empty_path).unwrap(); // creates the file + schema
    let r2 = Retrieval::new(Box::new(IndexProvider::new(empty_path)));
    let got2 = r2.search(&q("anything", false), &wide_scope()).unwrap();
    assert!(got2.items.is_empty());
    assert!(
        got2.coverage
            .degraded
            .as_deref()
            .unwrap_or("")
            .contains("empty"),
        "empty index must degrade with a populate hint, got: {:?}",
        got2.coverage.degraded
    );

    // A freshly-populated index (fixture) must NOT degrade.
    let (_tmp, root) = fixture();
    let dbdir2 = tempfile::tempdir().unwrap();
    let r3 = index_provider(&root, dbdir2.path());
    let got3 = r3.search(&q("pagination", false), &wide_scope()).unwrap();
    assert!(!got3.items.is_empty());
    assert!(
        got3.coverage.degraded.is_none(),
        "fresh index must not degrade, got {:?}",
        got3.coverage.degraded
    );
}

/// turn_count is genuinely unavailable to a scan (counting = parsing the whole
/// file), so it is Option — asserted explicitly so nobody "fixes" it by making
/// the scan backend eagerly parse.
#[test]
fn turn_count_is_index_only() {
    let (_tmp, root) = fixture();
    let dbdir = tempfile::tempdir().unwrap();
    assert!(scan_provider(&root)
        .sessions(&wide_scope(), 10)
        .unwrap()
        .items[0]
        .turn_count
        .is_none());
    assert!(index_provider(&root, dbdir.path())
        .sessions(&wide_scope(), 10)
        .unwrap()
        .items[0]
        .turn_count
        .is_some());
}

const LONG_UUID: &str = "99999999-8888-7777-6666-555555555555";

/// A session with more turns than any one page, which the main fixture (two
/// conversational turns) could never exercise.
fn long_fixture(n: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("-Users-test-dev-long");
    std::fs::create_dir_all(&proj).unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let body: String = (0..n)
        .map(|i| {
            // Oldest first, one second apart, so every turn has a distinct ts.
            let line = serde_json::json!({
                "type": "user", "cwd": "/Users/test/dev/long",
                "timestamp": chrono_fmt(now_ms - ((n - i) as i64) * 1000),
                "message": {"content": format!("turn {i}")}
            });
            format!("{line}\n")
        })
        .collect();
    std::fs::write(proj.join(format!("{LONG_UUID}.jsonl")), body).unwrap();
    (dir, root)
}

/// Page a long session with `AfterTs`, the cursor the README documents, and
/// collect everything.
fn page_all(r: &Retrieval, limit: usize) -> Vec<String> {
    let sid = SessionId::new(MACHINE, "claude_code", LONG_UUID);
    let mut out: Vec<String> = Vec::new();
    let mut cursor = TurnCursor::Start;
    for _ in 0..100 {
        let got = r.turns(&sid, cursor, false, limit, None).unwrap();
        if got.items.is_empty() {
            break;
        }
        let last_ts = got.items.last().and_then(|t| t.ts).expect("ts present");
        out.extend(got.items.into_iter().map(|t| t.text));
        if !got.coverage.truncated {
            break;
        }
        cursor = TurnCursor::AfterTs(last_ts);
    }
    out
}

/// The index provider used to answer an `AfterTs` read by fetching the FIRST
/// `limit * 4` turns of the session by `seq` and filtering on `ts` in Rust, so
/// paging past the start ran out of rows and reported an EMPTY page as
/// complete. The scan provider reads the whole file and paged correctly, so the
/// two disagreed about the documented cursor — and the fixture every other case
/// uses holds two turns, which `limit * 4` never binds on.
#[test]
fn paging_with_after_ts_agrees_across_backends() {
    const N: usize = 50;
    let (_tmp, root) = long_fixture(N);
    let db_dir = tempfile::tempdir().unwrap();

    let expected: Vec<String> = (0..N).map(|i| format!("turn {i}")).collect();

    let scan = page_all(&scan_provider(&root), 7);
    assert_eq!(
        scan.len(),
        N,
        "[scan] paging lost turns: {} of {N}",
        scan.len()
    );
    assert_eq!(scan, expected, "[scan] wrong turns or wrong order");

    let index = page_all(&index_provider(&root, db_dir.path()), 7);
    assert_eq!(
        index.len(),
        N,
        "[index] paging lost turns: {} of {N}",
        index.len()
    );
    assert_eq!(index, expected, "[index] wrong turns or wrong order");
}

/// The other half: a page that ends exactly on the last turn must not claim
/// there is more, and one that does not must not claim completeness.
#[test]
fn after_ts_reports_completeness_honestly_on_both_backends() {
    const N: usize = 20;
    let (_tmp, root) = long_fixture(N);
    let db_dir = tempfile::tempdir().unwrap();
    let sid = SessionId::new(MACHINE, "claude_code", LONG_UUID);

    for r in [scan_provider(&root), index_provider(&root, db_dir.path())] {
        let name = r.provider_name();
        let first = r.turns(&sid, TurnCursor::Start, false, 5, None).unwrap();
        assert!(
            first.coverage.truncated,
            "[{name}] 5 of {N} turns is not the whole session"
        );

        let ts_of_15th = r
            .turns(&sid, TurnCursor::Start, false, 15, None)
            .unwrap()
            .items
            .last()
            .and_then(|t| t.ts)
            .expect("ts present");
        let tail = r
            .turns(&sid, TurnCursor::AfterTs(ts_of_15th), false, 100, None)
            .unwrap();
        assert_eq!(
            tail.items.len(),
            5,
            "[{name}] the tail after turn 15 is 5 turns"
        );
        assert!(
            !tail.coverage.truncated,
            "[{name}] the tail IS the rest — claiming more is a lie"
        );
    }
}

/// `surface` from both providers, turn by turn, over real compaction markers.
///
/// The two computations share no code and cannot: the writer applies boundaries
/// incrementally in SQL as chunks arrive, the scan folds them in memory over the
/// whole parsed file (`tp_core::turn::apply_compaction`). This is the test that
/// holds them together — same fixtures, same reads, equality per turn.
///
/// Built on the SHIPPED descriptors rather than the built-in adapters, because
/// the built-ins deliberately carry no compaction rules (the descriptor
/// supersedes them by id); a conformance run through the built-ins would answer
/// `unknown == unknown` everywhere and prove nothing.
mod surface_conformance {
    use super::*;
    use tp_core::turn::Surface;
    use tp_ingest::adapter::decl::{load_configs, DeclAdapter};
    use tp_ingest::Adapter;

    fn shipped(id: &str) -> DeclAdapter {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d");
        DeclAdapter::new(
            load_configs(&dir)
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("shipped {id}.toml must load")),
        )
    }

    /// Both providers over one adapter + fixture; returns (scan, index) turns.
    fn both(
        runtime: &str,
        native_id: &str,
        root: &Path,
        db_dir: &Path,
    ) -> (Vec<tp_core::NormalizedTurn>, Vec<tp_core::NormalizedTurn>) {
        let scan = Retrieval::new(Box::new(ScanProvider::new(
            MACHINE,
            vec![Box::new(shipped(runtime))],
            vec![(runtime.to_string(), root.to_path_buf())],
        )));

        let db_path = db_dir.join("t.db");
        let mut db = tp_db::Db::open(&db_path).unwrap();
        db.ensure_self_machine(MACHINE, "TestMac").unwrap();
        db.ensure_runtime(runtime, root.to_str().unwrap()).unwrap();
        let adapter = shipped(runtime);
        for src in adapter.discover(root).unwrap() {
            let chunk = adapter.parse_from(&src.path, 0).unwrap();
            let sid = SessionId::new(MACHINE, runtime, &src.native_id).to_string();
            tp_db::writer::commit_chunk(
                db.conn_mut(),
                &sid,
                MACHINE,
                runtime,
                &src.native_id,
                src.path.to_str().unwrap(),
                src.inode,
                src.mtime_ms,
                &chunk,
            )
            .unwrap();
        }
        drop(db);
        let index = Retrieval::new(Box::new(IndexProvider::new(db_path)));

        let sid = SessionId::new(MACHINE, runtime, native_id);
        let read = |r: &Retrieval| {
            r.turns(&sid, TurnCursor::Start, false, 100, None)
                .unwrap()
                .items
        };
        (read(&scan), read(&index))
    }

    fn surfaces(turns: &[tp_core::NormalizedTurn]) -> Vec<Surface> {
        turns.iter().map(|t| t.surface).collect()
    }

    #[test]
    fn a_positional_marker_reads_the_same_from_scan_and_index() {
        let root = tempfile::tempdir().unwrap();
        let dbd = tempfile::tempdir().unwrap();
        let proj = root.path().join("-Users-x-p");
        std::fs::create_dir_all(&proj).unwrap();
        let msg = |t: &str, ts: &str| {
            format!(r#"{{"type":"user","timestamp":"{ts}","message":{{"content":"{t}"}}}}"#)
        };
        std::fs::write(
            proj.join("aaaaaaaa-1111-2222-3333-444444444444.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n",
                msg("old one", "2026-08-04T10:00:00-07:00"),
                msg("old two", "2026-08-04T10:01:00-07:00"),
                r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-08-04T10:02:00-07:00"}"#,
                msg("kept", "2026-08-04T10:03:00-07:00"),
            ),
        )
        .unwrap();

        let (scan, index) = both(
            "claude_code",
            "aaaaaaaa-1111-2222-3333-444444444444",
            root.path(),
            dbd.path(),
        );
        use Surface::*;
        assert_eq!(
            surfaces(&scan),
            [Superseded, Superseded, Current],
            "scan: {scan:?}"
        );
        assert_eq!(
            surfaces(&scan),
            surfaces(&index),
            "the split LLD §16 rule 1 forbids"
        );
    }

    /// A search HIT carries the same two facts a turn read does, from either
    /// provider. The scan cannot know `surface` mid-file (a marker near the end
    /// supersedes a match near the start), so it resolves hit-bearing files
    /// after the fact — this is the test that says that resolution agrees with
    /// what the writer stored.
    #[test]
    fn search_hits_carry_surface_and_sidechain_from_both_providers() {
        let root = tempfile::tempdir().unwrap();
        let dbd = tempfile::tempdir().unwrap();
        let proj = root.path().join("-Users-x-p");
        let sub = proj
            .join("aaaaaaaa-1111-2222-3333-444444444444")
            .join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        // Timestamps must fall inside the search window — `wide_scope` is one
        // hour, and unlike a turns read the search path prunes by time.
        let now = tp_core::now_ms();
        let msg = |t: &str, ts: i64| {
            format!(
                r#"{{"type":"user","timestamp":"{}","message":{{"content":"{t}"}}}}"#,
                chrono_fmt(ts)
            )
        };
        let boundary = format!(
            r#"{{"type":"system","subtype":"compact_boundary","timestamp":"{}"}}"#,
            chrono_fmt(now - 240_000)
        );
        std::fs::write(
            proj.join("aaaaaaaa-1111-2222-3333-444444444444.jsonl"),
            format!(
                "{}
{}
{}
",
                msg("needle before the cut", now - 300_000),
                boundary,
                msg("needle after the cut", now - 180_000),
            ),
        )
        .unwrap();
        std::fs::write(
            sub.join("agent-conf1.jsonl"),
            format!(
                r#"{{"type":"user","isSidechain":true,"timestamp":"{}","message":{{"content":"needle from the subagent"}}}}"#,
                chrono_fmt(now - 120_000)
            ) + "
",
        )
        .unwrap();

        // Reuse `both`'s providers via a search instead of a turns read.
        let scan = Retrieval::new(Box::new(ScanProvider::new(
            MACHINE,
            vec![Box::new(shipped("claude_code"))],
            vec![("claude_code".to_string(), root.path().to_path_buf())],
        )));
        let db_path = dbd.path().join("t.db");
        {
            let mut db = tp_db::Db::open(&db_path).unwrap();
            db.ensure_self_machine(MACHINE, "TestMac").unwrap();
            db.ensure_runtime("claude_code", root.path().to_str().unwrap())
                .unwrap();
            let adapter = shipped("claude_code");
            for src in adapter.discover(root.path()).unwrap() {
                let chunk = adapter.parse_from(&src.path, 0).unwrap();
                let sid = SessionId::new(MACHINE, "claude_code", &src.native_id).to_string();
                tp_db::writer::commit_chunk(
                    db.conn_mut(),
                    &sid,
                    MACHINE,
                    "claude_code",
                    &src.native_id,
                    src.path.to_str().unwrap(),
                    src.inode,
                    src.mtime_ms,
                    &chunk,
                )
                .unwrap();
            }
        }
        let index = Retrieval::new(Box::new(IndexProvider::new(db_path)));

        let q = Query {
            text: "needle".into(),
            regex: false,
            include_thinking: false,
            limit: 10,
        };
        let flags = |r: &Retrieval| {
            let mut v: Vec<(String, bool, Surface)> = r
                .search(&q, &wide_scope())
                .unwrap()
                .items
                .into_iter()
                .map(|h| (h.excerpt().to_string(), h.sidechain, h.surface))
                .collect();
            // Providers rank differently; compare as sets keyed by excerpt.
            v.sort();
            v.into_iter()
                .map(|(e, side, surf)| {
                    // FTS snippets wrap the match in [] — strip to compare.
                    (e.replace(['[', ']'], ""), side, surf)
                })
                .collect::<Vec<_>>()
        };
        let (s_hits, i_hits) = (flags(&scan), flags(&index));
        assert_eq!(s_hits.len(), 3, "{s_hits:?}");
        assert_eq!(s_hits, i_hits, "the split LLD §16 rule 1 forbids");
        for (e, side, surf) in &s_hits {
            let (want_side, want_surf) = match e.as_str() {
                x if x.contains("before") => (false, Surface::Superseded),
                x if x.contains("after") => (false, Surface::Current),
                _ => (true, Surface::Current),
            };
            assert_eq!((*side, *surf), (want_side, want_surf), "{e}");
        }
    }

    #[test]
    fn an_anchored_marker_reads_the_same_from_scan_and_index() {
        let root = tempfile::tempdir().unwrap();
        let dbd = tempfile::tempdir().unwrap();
        let proj = root.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let msg = |id: &str, t: &str, ts: &str| {
            format!(
                r#"{{"type":"message","id":"{id}","timestamp":"{ts}","message":{{"role":"user","content":[{{"type":"text","text":"{t}"}}]}}}}"#
            )
        };
        // The anchor points EARLIER than the marker — the shape real pi data
        // has (15/43/68 entries earlier on this machine), and the reason a
        // positional reading is wrong in the worst direction.
        std::fs::write(
            proj.join("2026-08-04T10-00-00-000Z_0199aaaa.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n{}\n",
                msg("e1", "dropped", "2026-08-04T10:00:00-07:00"),
                msg("e2", "kept early", "2026-08-04T10:01:00-07:00"),
                msg("e3", "kept mid", "2026-08-04T10:02:00-07:00"),
                r#"{"type":"compaction","id":"c1","firstKeptEntryId":"e2","summary":"what e1 said","timestamp":"2026-08-04T10:03:00-07:00"}"#,
                msg("e4", "after", "2026-08-04T10:04:00-07:00"),
            ),
        )
        .unwrap();

        let (scan, index) = both("pi", "0199aaaa", root.path(), dbd.path());
        use Surface::*;
        assert_eq!(
            surfaces(&scan),
            // e1 superseded; e2 (the anchor) kept; the compaction summary is
            // itself an indexed turn and lands current.
            [Superseded, Current, Current, Current, Current],
            "scan: {:?}",
            scan.iter()
                .map(|t| (&t.text, t.surface))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            surfaces(&scan),
            surfaces(&index),
            "the split LLD §16 rule 1 forbids"
        );
    }
}

/// The mirror of `empty_or_missing_index_is_degraded_not_empty`, for the other
/// provider — and the case the scan default did not anticipate at all.
///
/// A push-ingested runtime (dsh calls `tp ingest`; nothing writes a transcript)
/// has no file for a scan to read and no `source_path` to notice is missing. So
/// `unscannable_note`, which exists for exactly this class, cannot see it: its
/// first condition is `source_path IS NOT NULL`, and these rows never had one.
///
/// Measured on the machine this was written on: `tp turns <dsh-session>`
/// answered "no turns found" for a session holding 50 turns, while the same
/// command with `--index` printed them. That is the false negative LLD §16
/// rule 3 names — a fact about the PROVIDER rendered as a fact about the
/// CORPUS — and the divergence is a bug rather than a declared capability,
/// because `Capabilities` has no flag that could express it.
#[test]
fn a_runtime_the_scan_cannot_see_is_degraded_not_empty() {
    let (_tmp, root) = fixture();
    let scan = scan_provider(&root);

    // Same shape as a dsh row: a runtime the provider holds no root for.
    let orphan = SessionId::new(MACHINE, "dsh", "session-0bdc4c9f");
    let got = scan
        .turns(&orphan, TurnCursor::Start, false, 10, Some(1 << 20))
        .unwrap();
    assert!(got.items.is_empty(), "there is genuinely nothing to return");
    let note = got.coverage.degraded.as_deref().unwrap_or("");
    assert!(
        note.contains("dsh"),
        "the note must name the runtime it cannot read, got: {note:?}"
    );
    assert!(
        note.contains("--index"),
        "and point at the provider that can, got: {note:?}"
    );

    // The honest empty stays empty: same runtime, root present, file absent.
    // A scan that looked and found nothing is a complete answer, and marking
    // it degraded would make the signal worthless by never being off.
    let missing = SessionId::new(MACHINE, "claude_code", "no-such-session");
    let got2 = scan
        .turns(&missing, TurnCursor::Start, false, 10, Some(1 << 20))
        .unwrap();
    assert!(got2.items.is_empty());
    assert!(
        got2.coverage.degraded.is_none(),
        "a session that is simply not there must not degrade, got: {:?}",
        got2.coverage.degraded
    );
}
