//! Regression: the FSEvents callback and `run_iter` must share ONE event set.
//! A previous version built an `Arc<Mutex<HashSet>>` for the callback and then
//! stored a *different*, empty set on the struct — so the fast path was dead
//! (ingest only ever happened on the 30s reconcile) and the callback's set grew
//! forever with nobody draining it.

use std::time::{Duration, Instant};
use tp_db::Db;
use tp_watch::{WatchRoot, Watcher};

const MACHINE: &str = "m-evt";

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp(ms / 1000, ((ms % 1000) * 1_000_000) as u32)
        .unwrap()
        .to_rfc3339()
}

/// Drive the real FSEvents path: write a file, wait for the OS event, then call
/// `run_iter` with the reconcile timer NOT expired. Any ingest that happens can
/// only have come from the event set.
#[test]
fn fsevents_path_ingests_without_reconcile() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("projects");
    std::fs::create_dir_all(root.join("-Users-test-dev-demo")).unwrap();

    let db = Db::open_in_memory().unwrap();
    db.ensure_self_machine(MACHINE, "TestMac").unwrap();
    db.ensure_runtime("claude_code", root.to_str().unwrap())
        .unwrap();

    let watcher = Watcher::new(
        MACHINE,
        db,
        vec![WatchRoot {
            runtime_id: "claude_code".to_string(),
            root: root.clone(),
            adapter: Box::new(tp_ingest::builtin("claude_code")),
        }],
    )
    .unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let uuid = "eeeeeeee-1111-2222-3333-444444444444";
    let line = serde_json::json!({
        "type": "user", "cwd": "/Users/test/dev/demo", "timestamp": rfc3339(now),
        "message": {"content": "event-driven hello"}
    });
    std::fs::write(
        root.join("-Users-test-dev-demo")
            .join(format!("{uuid}.jsonl")),
        format!("{line}\n"),
    )
    .unwrap();

    // Give FSEvents time to deliver. Keep the reconcile timer fresh so ONLY the
    // event path can cause an ingest.
    let mut ingested = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(100));
        let mut fresh = Instant::now(); // never >= RECONCILE_EVERY → reconcile cannot fire
        watcher.run_iter(&mut fresh).unwrap();
        let db = watcher.db().lock().unwrap();
        let n = tp_db::query::list_turns(
            db.conn(),
            &format!("{MACHINE}/claude_code/{uuid}"),
            0,
            false,
            10,
        )
        .unwrap()
        .len();
        drop(db);
        if n > 0 {
            ingested = true;
            break;
        }
    }
    assert!(
        ingested,
        "the FSEvents fast path never ingested: the callback's event set is not the one run_iter drains"
    );
}
