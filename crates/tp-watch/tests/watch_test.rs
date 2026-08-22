//! Watcher integration: FSEvents fires asynchronously, so the test drives the
//! loop directly and asserts the *ingest path* (the part that must be correct)
//! rather than relying on background event timing.

use std::time::{Duration, Instant};
use tp_db::Db;
use tp_watch::{WatchRoot, Watcher};

const MACHINE: &str = "m-watch";

fn rfc3339(ms: i64) -> String {
    let secs = ms / 1000;
    let nsec = ((ms % 1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nsec)
        .unwrap()
        .to_rfc3339()
}

fn write_session(root: &std::path::Path, uuid: &str, now_ms: i64, turn: &str) {
    let proj = root.join("-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let line = serde_json::json!({
        "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": rfc3339(now_ms),
        "message": {"content": turn}
    });
    std::fs::write(proj.join(format!("{uuid}.jsonl")), format!("{line}\n")).unwrap();
}

fn build_watcher(root: &std::path::Path, db: Db) -> Watcher {
    let wroot = WatchRoot {
        runtime_id: "claude_code".to_string(),
        root: root.to_path_buf(),
        adapter: Box::new(tp_ingest::builtin("claude_code")),
    };
    Watcher::new(MACHINE, db, vec![wroot]).unwrap()
}

/// A fresh file (new inode) appended after the watcher started must be picked
/// up by the reconcile sweep — the correctness net for events FSEvents missed.
#[test]
fn reconcile_picks_up_new_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(MACHINE, "TestMac").unwrap();
    db.ensure_runtime("claude_code", root.to_str().unwrap())
        .unwrap();
    let watcher = build_watcher(&root, db);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    write_session(
        &root,
        "aaaaaaaa-1111-2222-3333-444444444444",
        now_ms,
        "first message",
    );

    // Reconcile sweep (the 30s net) must discover the new file with no event.
    let mut last = Instant::now() - Duration::from_secs(31);
    watcher.run_iter(&mut last).unwrap();

    let db = watcher.db().lock().unwrap();
    let turns = tp_db::query::list_turns(
        db.conn(),
        &format!("{MACHINE}/claude_code/aaaaaaaa-1111-2222-3333-444444444444"),
        0,
        false,
        10,
    )
    .unwrap();
    assert_eq!(turns.len(), 1, "reconcile must ingest a brand-new file");
    assert_eq!(turns[0].text, "first message");
    drop(db);
}

/// Incremental append: a second message appended to the SAME file must be
/// ingested from the inode checkpoint, on top of the first — not re-parsed.
#[test]
fn reconcile_appends_incrementally() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(MACHINE, "TestMac").unwrap();
    db.ensure_runtime("claude_code", root.to_str().unwrap())
        .unwrap();
    let watcher = build_watcher(&root, db);

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let uuid = "bbbbbbbb-1111-2222-3333-444444444444";
    let proj = root.join("-Users-test-dev-demo");
    std::fs::create_dir_all(&proj).unwrap();
    let path = proj.join(format!("{uuid}.jsonl"));
    let line = |t: &str| {
        serde_json::json!({
            "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": rfc3339(now_ms),
            "message": {"content": t}
        })
    };
    std::fs::write(&path, format!("{}\n", line("first"))).unwrap();

    let mut last = Instant::now() - Duration::from_secs(31);
    watcher.run_iter(&mut last).unwrap();

    // Append a second line to the same file (same inode).
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(format!("{}\n", line("second")).as_bytes())
        .unwrap();
    // Simulate the next reconcile tick (no FSEvents event delivered — the
    // correctness net must catch the append on its own).
    last = Instant::now() - Duration::from_secs(31);
    watcher.run_iter(&mut last).unwrap();

    let db = watcher.db().lock().unwrap();
    let turns = tp_db::query::list_turns(
        db.conn(),
        &format!("{MACHINE}/claude_code/{uuid}"),
        0,
        false,
        10,
    )
    .unwrap();
    assert_eq!(
        turns.len(),
        2,
        "append must be ingested incrementally, not re-parsed"
    );
    assert_eq!(turns[0].text, "first");
    assert_eq!(turns[1].text, "second");
    drop(db);
}

use std::io::Write as _;

/// One bad file must not end indexing. Before this, `scan_root` propagated a
/// single file's error, the sweep aborted, every later file was skipped, and
/// the error travelled out through `reconcile` and `run` far enough to kill the
/// watcher thread while `tpd` stayed alive — so launchd never restarted it and
/// the index silently stopped growing. A corpus of 27,000 files written by four
/// other programs will contain a bad one eventually.
#[test]
fn one_unreadable_file_does_not_stop_the_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(&root).unwrap();

    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(MACHINE, "TestMac").unwrap();
    db.ensure_runtime("claude_code", root.to_str().unwrap())
        .unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Discovery order is whatever read_dir gives, so make BOTH a good file that
    // could come after a bad one and a bad file that could come after a good
    // one: the assertion has to hold either way.
    write_session(
        &root,
        "aaaaaaaa-0000-0000-0000-000000000001",
        now_ms,
        "before",
    );
    write_session(
        &root,
        "zzzzzzzz-0000-0000-0000-000000000002",
        now_ms,
        "after",
    );

    let proj = root.join("-Users-test-dev-demo");
    let bad = proj.join("mmmmmmmm-0000-0000-0000-000000000003.jsonl");
    std::fs::write(&bad, b"{\"type\":\"user\"}\n").unwrap();
    // Unreadable: the ingest path opens it and fails.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();
    }

    let watcher = build_watcher(&root, db);
    let mut last = Instant::now() - Duration::from_secs(31);
    let out = watcher.run_iter(&mut last);
    assert!(
        out.is_ok(),
        "a single unreadable file must not fail the sweep: {out:?}"
    );

    let db = watcher.db().lock().unwrap();
    let n: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turn", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2, "both readable sessions must still be indexed");
    drop(db);
}
