//! Integration tests for the write/read path that the whole design leans on:
//! transactional upsert + inode-keyed incremental checkpointing (LLD §6.2, §15 #1).

use tp_core::turn::{NormalizedTurn, ParseChunk, Role, SessionMeta, ToolCallDigest};
use tp_db::{writer, Db};

fn turn(role: Role, text: &str, thinking: &str, ts: i64) -> NormalizedTurn {
    NormalizedTurn {
        role,
        ts: Some(ts),
        text: text.to_string(),
        thinking: thinking.to_string(),
        thinking_opaque: false,
        tool_calls: vec![],
        surface: Default::default(),
        tokens_in: None,
        tokens_out: None,
        prov: Default::default(),
    }
}

fn setup() -> Db {
    let db = Db::open_in_memory().expect("open db");
    db.ensure_self_machine("m1", "TestMac").unwrap();
    db.ensure_runtime("claude_code", "/root").unwrap();
    db
}

#[test]
fn first_commit_creates_session_and_advances_checkpoint() {
    let mut db = setup();
    let chunk = ParseChunk {
        turns: vec![
            turn(Role::User, "hello world", "", 1000),
            turn(
                Role::Assistant,
                "responding now",
                "thinking about apples secretly",
                2000,
            ),
        ],
        new_offset: 100,
        meta: SessionMeta {
            cwd: Some("/proj".into()),
            title_derived: Some("hi".into()),
            started_at: Some(1000),
            ..Default::default()
        },
        ..Default::default()
    };

    let n = writer::commit_chunk(
        db.conn_mut(),
        "m1/claude_code/sess1",
        "m1",
        "claude_code",
        "sess1",
        "/path/sess1.jsonl",
        42,
        1000,
        &chunk,
    )
    .unwrap();
    assert_eq!(n, 2);

    let ck = writer::get_checkpoint(db.conn(), 42)
        .unwrap()
        .expect("checkpoint exists");
    assert_eq!(ck.byte_offset, 100);
    assert_eq!(ck.last_seq, 2);

    let sessions = tp_db::query::list_sessions(db.conn(), None, None, None, 10).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].turn_count, 2);
    assert_eq!(sessions[0].cwd.as_deref(), Some("/proj"));
    assert_eq!(sessions[0].title.as_deref(), Some("hi"));

    let turns = tp_db::query::list_turns(db.conn(), "m1/claude_code/sess1", 0, true, 10).unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].seq, 1);
    assert_eq!(turns[1].seq, 2);
}

/// The core incremental-resume guarantee: a second commit against the SAME
/// inode continues `seq` where the first left off, and never re-inserts
/// already-written turns (UNIQUE(session_id, seq) would reject a duplicate).
#[test]
fn second_commit_resumes_seq_and_preserves_meta_via_coalesce() {
    let mut db = setup();
    let chunk1 = ParseChunk {
        turns: vec![
            turn(Role::User, "hello world", "", 1000),
            turn(
                Role::Assistant,
                "responding now",
                "thinking about apples secretly",
                2000,
            ),
        ],
        new_offset: 100,
        meta: SessionMeta {
            cwd: Some("/proj".into()),
            title_derived: Some("hi".into()),
            started_at: Some(1000),
            ..Default::default()
        },
        ..Default::default()
    };
    writer::commit_chunk(
        db.conn_mut(),
        "m1/claude_code/sess1",
        "m1",
        "claude_code",
        "sess1",
        "/path/sess1.jsonl",
        42,
        1000,
        &chunk1,
    )
    .unwrap();

    // Simulate an adapter's incremental parse: offset > 0, so cwd/title come back None —
    // the writer must COALESCE onto the existing session row, not clobber it.
    let chunk2 = ParseChunk {
        turns: vec![turn(Role::User, "another message", "", 3000)],
        new_offset: 150,
        meta: SessionMeta {
            cwd: None,
            title_derived: None,
            started_at: None,
            ..Default::default()
        },
        ..Default::default()
    };
    let n = writer::commit_chunk(
        db.conn_mut(),
        "m1/claude_code/sess1",
        "m1",
        "claude_code",
        "sess1",
        "/path/sess1.jsonl",
        42,
        1500,
        &chunk2,
    )
    .unwrap();
    assert_eq!(n, 1);

    let ck = writer::get_checkpoint(db.conn(), 42).unwrap().unwrap();
    assert_eq!(ck.byte_offset, 150);
    assert_eq!(
        ck.last_seq, 3,
        "seq must continue from the first chunk's last_seq, not restart"
    );

    let sessions = tp_db::query::list_sessions(db.conn(), None, None, None, 10).unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "same inode+native_id must upsert, never duplicate the session"
    );
    assert_eq!(sessions[0].turn_count, 3);
    assert_eq!(
        sessions[0].cwd.as_deref(),
        Some("/proj"),
        "cwd must survive a chunk with meta.cwid=None"
    );
    assert_eq!(sessions[0].last_turn_at, Some(3000));

    let turns = tp_db::query::list_turns(db.conn(), "m1/claude_code/sess1", 0, false, 10).unwrap();
    let seqs: Vec<i64> = turns.iter().map(|t| t.seq).collect();
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "no gap, no duplicate, no restart across the two commits"
    );
}

/// A different inode (rotation / a second file) must never collide with an
/// unrelated file's checkpoint — this is the whole point of keying by inode.
#[test]
fn different_inode_gets_independent_checkpoint() {
    let mut db = setup();
    let chunk = ParseChunk {
        turns: vec![turn(Role::User, "a", "", 1)],
        new_offset: 10,
        meta: SessionMeta::default(),
        ..Default::default()
    };
    writer::commit_chunk(
        db.conn_mut(),
        "m1/claude_code/s1",
        "m1",
        "claude_code",
        "s1",
        "/a.jsonl",
        1,
        1,
        &chunk,
    )
    .unwrap();
    writer::commit_chunk(
        db.conn_mut(),
        "m1/claude_code/s2",
        "m1",
        "claude_code",
        "s2",
        "/b.jsonl",
        2,
        1,
        &chunk,
    )
    .unwrap();

    assert_eq!(
        writer::get_checkpoint(db.conn(), 1)
            .unwrap()
            .unwrap()
            .byte_offset,
        10
    );
    assert_eq!(
        writer::get_checkpoint(db.conn(), 2)
            .unwrap()
            .unwrap()
            .byte_offset,
        10
    );
    assert!(writer::get_checkpoint(db.conn(), 999).unwrap().is_none());

    let sessions = tp_db::query::list_sessions(db.conn(), None, None, None, 10).unwrap();
    assert_eq!(sessions.len(), 2);
}

#[test]
fn search_respects_include_thinking_gate() {
    let mut db = setup();
    let chunk = ParseChunk {
        turns: vec![
            turn(Role::User, "hello world", "", 1000),
            NormalizedTurn {
                role: Role::Assistant,
                ts: Some(2000),
                text: "responding now".into(),
                thinking: "thinking about apples secretly".into(),
                thinking_opaque: false,
                tool_calls: vec![ToolCallDigest {
                    name: "Bash".into(),
                    input_digest: Some("ls -la".into()),
                }],
                surface: Default::default(),
                tokens_in: None,
                tokens_out: None,
                prov: Default::default(),
            },
        ],
        new_offset: 100,
        meta: SessionMeta::default(),
        ..Default::default()
    };
    writer::commit_chunk(
        db.conn_mut(),
        "m1/claude_code/sess1",
        "m1",
        "claude_code",
        "sess1",
        "/path.jsonl",
        42,
        1000,
        &chunk,
    )
    .unwrap();

    // "apples" only ever appears in `thinking` — must be invisible with the gate off.
    let hidden = tp_db::query::search(db.conn(), "apples", false, 10, None, None, None).unwrap();
    assert!(
        hidden.is_empty(),
        "thinking-only content must not match when include_thinking=false"
    );

    let visible = tp_db::query::search(db.conn(), "apples", true, 10, None, None, None).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].seq, 2);

    // Ordinary text matches regardless of the gate.
    let text_hit = tp_db::query::search(db.conn(), "hello", false, 10, None, None, None).unwrap();
    assert_eq!(text_hit.len(), 1);
    assert_eq!(text_hit[0].seq, 1);

    // Tool name is part of the `{text tool_calls}` filtered column set, so it
    // matches even with the thinking gate off.
    let tool_hit = tp_db::query::search(db.conn(), "Bash", false, 10, None, None, None).unwrap();
    assert_eq!(tool_hit.len(), 1);
    assert_eq!(tool_hit[0].seq, 2);
}

/// A pushed session behaves like a parsed one for reads, and re-pushing the
/// same turns is a no-op. Both matter: the first because search must not care
/// how a turn arrived, the second because Tencent's equivalent endpoint says in
/// its own source that resending duplicates writes duplicates.
#[test]
fn pushed_turns_dedupe_on_uuid_and_are_searchable() {
    let mut db = tp_db::Db::open(std::path::Path::new(":memory:")).unwrap();
    db.ensure_self_machine("m1", "h").unwrap();
    db.ensure_runtime("dsh", "/none").unwrap();

    let turn = |uuid: &str, text: &str, ts: i64| tp_core::turn::NormalizedTurn {
        role: tp_core::turn::Role::User,
        ts: Some(ts),
        text: text.to_string(),
        thinking: String::new(),
        thinking_opaque: false,
        tool_calls: vec![],
        surface: Default::default(),
        tokens_in: None,
        tokens_out: None,
        prov: tp_core::turn::Provenance {
            uuid: Some(uuid.to_string()),
            ..Default::default()
        },
    };
    let meta = tp_core::turn::SessionMeta {
        cwd: Some("/w".into()),
        ..Default::default()
    };
    let turns = vec![turn("a", "alpha", 1000), turn("b", "beta", 2000)];

    let first =
        tp_db::writer::commit_pushed(db.conn_mut(), "m1/dsh/s", "m1", "dsh", "s", &meta, &turns)
            .unwrap();
    assert_eq!((first.inserted, first.duplicates), (2, 0));

    // Re-push the same two plus one new: only the new one lands.
    let again = vec![
        turn("a", "alpha", 1000),
        turn("b", "beta", 2000),
        turn("c", "gamma", 3000),
    ];
    let second =
        tp_db::writer::commit_pushed(db.conn_mut(), "m1/dsh/s", "m1", "dsh", "s", &meta, &again)
            .unwrap();
    assert_eq!(
        (second.inserted, second.duplicates),
        (1, 2),
        "re-pushing must be idempotent per (session_id, uuid)"
    );

    let (n, count): (i64, i64) = db
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM turn WHERE session_id='m1/dsh/s'),
                    (SELECT turn_count FROM session WHERE id='m1/dsh/s')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(n, 3, "three distinct turns stored");
    assert_eq!(
        count, 3,
        "session turn_count must match reality, not accumulate"
    );

    // Contiguous seq across pushes — a pushed session is orderable like any other.
    let seqs: Vec<i64> = db
        .conn()
        .prepare("SELECT seq FROM turn WHERE session_id='m1/dsh/s' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(seqs, vec![1, 2, 3]);

    // And it is searchable through the same FTS index as parsed turns.
    let hits = tp_db::query::search(db.conn(), "gamma", false, 10, None, None, None).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "a pushed turn must be findable like a parsed one"
    );
}

/// A turn with no uuid cannot be deduped. Accept it, but say so — silently
/// duplicating on the next push is the failure this reports instead of causing.
#[test]
fn turns_without_a_uuid_are_reported_as_unkeyed() {
    let mut db = tp_db::Db::open(std::path::Path::new(":memory:")).unwrap();
    db.ensure_self_machine("m1", "h").unwrap();
    db.ensure_runtime("dsh", "/none").unwrap();

    let t = tp_core::turn::NormalizedTurn {
        role: tp_core::turn::Role::User,
        ts: Some(1),
        text: "no id".into(),
        thinking: String::new(),
        thinking_opaque: false,
        tool_calls: vec![],
        surface: Default::default(),
        tokens_in: None,
        tokens_out: None,
        prov: Default::default(),
    };
    let out = tp_db::writer::commit_pushed(
        db.conn_mut(),
        "m1/dsh/s",
        "m1",
        "dsh",
        "s",
        &Default::default(),
        &[t],
    )
    .unwrap();
    assert_eq!((out.inserted, out.unkeyed), (1, 1));
}

/// The concurrency failure the daemon actually hit, nine times: a DEFERRED
/// transaction starts as a read and promotes on its first write, and in WAL
/// mode a commit from another connection in between makes that promotion fail
/// with SQLITE_BUSY_SNAPSHOT (517). `busy_timeout` does not cover it — it is a
/// snapshot invalidation, not a lock wait — so the error surfaced immediately,
/// propagated out of the watcher, and stopped indexing until tpd was restarted:
///
///     tpd: watcher stopped: database is locked: Error code 517: Cannot promote
///     read transaction to write transaction because of writes by another
///     connection
///
/// This test drives that sequence by hand on both behaviours.
#[test]
fn a_deferred_transaction_loses_the_promotion_race_and_immediate_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let db = tp_db::Db::open(&path).unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
    }

    let open = || {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.pragma_update(None, "journal_mode", "WAL").unwrap();
        c.pragma_update(None, "busy_timeout", 5000).unwrap();
        c
    };

    // DEFERRED: read first, let another connection commit, then write.
    let mut a = open();
    let b = open();
    let tx = a
        .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
        .unwrap();
    tx.query_row("SELECT count(*) FROM machine", [], |r| r.get::<_, i64>(0))
        .unwrap(); // the snapshot is taken here
    b.execute(
        "INSERT INTO machine(id, name, trust, created_at) VALUES ('x','x','trusted',unixepoch())",
        [],
    )
    .unwrap();
    let err = tx
        .execute(
            "INSERT INTO machine(id, name, trust, created_at) VALUES ('y','y','trusted',unixepoch())",
            [],
        )
        .unwrap_err();
    // The daemon logged the extended form ("Cannot promote read transaction to
    // write transaction because of writes by another connection", code 517);
    // whether the extended text or the plain "database is locked" (code 5)
    // surfaces depends on whether extended result codes are enabled on the
    // connection. Both are the same event, and asserting the wording would make
    // this test about rusqlite's error formatting rather than about the race.
    let msg = err.to_string();
    assert!(
        msg.contains("promote") || msg.contains("locked"),
        "expected the promotion to fail, got: {err}"
    );
    drop(tx);

    // IMMEDIATE: the write lock is taken at BEGIN, so the other connection's
    // commit cannot land underneath — the contention becomes a wait, which
    // busy_timeout does cover.
    let tx = a
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    tx.query_row("SELECT count(*) FROM machine", [], |r| r.get::<_, i64>(0))
        .unwrap();
    tx.execute(
        "INSERT INTO machine(id, name, trust, created_at) VALUES ('z','z','trusted',unixepoch())",
        [],
    )
    .expect("an IMMEDIATE transaction must not lose a promotion race — it never promotes");
    tx.commit().unwrap();
}

/// The regression guard for the transaction BEHAVIOUR, not just the mechanism:
/// two writers going through `commit_chunk` on the same file at once. With a
/// DEFERRED transaction one of them loses the promotion race and errors; with
/// IMMEDIATE the contention becomes a wait that `busy_timeout` absorbs.
#[test]
fn concurrent_writers_do_not_lose_a_promotion_race() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let db = tp_db::Db::open(&path).unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db.ensure_runtime("claude_code", "/r").unwrap();
    }

    let errs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut handles = Vec::new();
    for w in 0..2 {
        let path = path.clone();
        let errs = errs.clone();
        handles.push(std::thread::spawn(move || {
            let mut db = tp_db::Db::open(&path).unwrap();
            for i in 0..40 {
                let chunk = tp_core::turn::ParseChunk {
                    turns: vec![tp_core::turn::NormalizedTurn {
                        role: tp_core::turn::Role::User,
                        ts: Some(1_000 + i),
                        text: format!("writer {w} turn {i}"),
                        thinking: String::new(),
                        thinking_opaque: false,
                        tool_calls: Vec::new(),
                        surface: Default::default(),
                        tokens_in: None,
                        tokens_out: None,
                        prov: Default::default(),
                    }],
                    new_offset: (i as u64 + 1) * 10,
                    meta: Default::default(),
                    ..Default::default()
                };
                if let Err(e) = tp_db::writer::commit_chunk(
                    db.conn_mut(),
                    &format!("m1/claude_code/s{w}"),
                    "m1",
                    "claude_code",
                    &format!("s{w}"),
                    &format!("/r/s{w}.jsonl"),
                    1000 + w,
                    i,
                    &chunk,
                ) {
                    errs.lock().unwrap().push(format!("{e:#}"));
                    return;
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let errs = errs.lock().unwrap();
    assert!(
        errs.is_empty(),
        "concurrent writers hit {} error(s); first: {}",
        errs.len(),
        errs[0]
    );
}

mod surface {
    use super::*;

    use tp_core::turn::CompactionBoundary;

    fn chunk(turns: Vec<NormalizedTurn>, at: Vec<usize>, tracks: bool) -> ParseChunk {
        boundaries(
            turns,
            at.into_iter().map(CompactionBoundary::At).collect(),
            tracks,
        )
    }

    fn boundaries(
        turns: Vec<NormalizedTurn>,
        compaction: Vec<CompactionBoundary>,
        tracks: bool,
    ) -> ParseChunk {
        ParseChunk {
            turns,
            new_offset: 0,
            meta: SessionMeta::default(),
            compaction,
            tracks_compaction: tracks,
        }
    }

    fn with_uuid(text: &str, uuid: &str) -> NormalizedTurn {
        let mut t = t(text);
        t.prov.uuid = Some(uuid.to_string());
        t
    }

    fn t(text: &str) -> NormalizedTurn {
        NormalizedTurn {
            role: Role::User,
            ts: Some(1_000),
            text: text.into(),
            thinking: String::new(),
            thinking_opaque: false,
            tool_calls: vec![],
            surface: Default::default(),
            tokens_in: None,
            tokens_out: None,
            prov: Default::default(),
        }
    }

    fn commit(db: &mut Db, inode: i64, c: &ParseChunk) {
        writer::commit_chunk(
            db.conn_mut(),
            "m1/claude_code/s",
            "m1",
            "claude_code",
            "s",
            "/p",
            inode,
            1,
            c,
        )
        .unwrap();
    }

    /// `(text, surface)` per turn, in seq order.
    ///
    /// Counts alone are not enough and a sabotage proved it: inverting the
    /// boundary comparison (`seq >` instead of `seq <=`) marks the wrong HALF
    /// superseded and leaves the totals identical, so a count assertion passed
    /// against a fully inverted implementation.
    fn rows(db: &Db) -> Vec<(String, String)> {
        db.conn()
            .prepare("SELECT text, surface FROM turn ORDER BY seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn counts(db: &Db) -> Vec<(String, i64)> {
        db.conn()
            .prepare("SELECT surface, count(*) FROM turn GROUP BY surface ORDER BY surface")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// THE case the design exists for. A boundary arrives in a LATER chunk than
    /// the turns it supersedes — which is the normal case for a live session —
    /// so those turns are already committed and must be back-updated. Anything
    /// that only marked turns in the current chunk would leave the first half of
    /// every compacted session claiming to be live context.
    #[test]
    fn a_boundary_in_a_later_chunk_supersedes_turns_already_written() {
        let mut db = setup();
        commit(&mut db, 1, &chunk(vec![t("a"), t("b")], vec![], true));
        assert_eq!(counts(&db), vec![("current".to_string(), 2)]);

        // Second chunk: the boundary lands before its own turns.
        commit(
            &mut db,
            1,
            &chunk(vec![t("summary"), t("c")], vec![0], true),
        );
        assert_eq!(
            rows(&db),
            vec![
                ("a".into(), "superseded".into()),
                ("b".into(), "superseded".into()),
                ("summary".into(), "current".into()),
                ("c".into(), "current".into()),
            ],
            "the EARLIER turns are the superseded ones, not the later ones"
        );
    }

    /// pi's shape: the boundary names the FIRST KEPT entry, and that entry is
    /// kept. Anchored rather than positional because pi's `firstKeptEntryId`
    /// points EARLIER than the marker — by 15, 43 and 68 entries in the three
    /// real sessions on this machine — so reading it positionally reports live
    /// context as superseded.
    #[test]
    fn an_anchored_boundary_keeps_the_entry_it_names() {
        let mut db = setup();
        commit(
            &mut db,
            1,
            &boundaries(
                vec![
                    with_uuid("old", "u1"),
                    with_uuid("older", "u2"),
                    with_uuid("KEEP", "u3"),
                    with_uuid("after", "u4"),
                ],
                vec![CompactionBoundary::Before("u3".into())],
                true,
            ),
        );
        assert_eq!(
            rows(&db),
            vec![
                ("old".into(), "superseded".into()),
                ("older".into(), "superseded".into()),
                ("KEEP".into(), "current".into()),
                ("after".into(), "current".into()),
            ],
            "the named entry is the first KEPT one, not the last superseded one"
        );
    }

    /// An anchor teleport cannot find marks NOTHING. Reporting live context as
    /// superseded is worse than leaving superseded content unmarked, and an
    /// anchor can genuinely be missing — behind an ingest checkpoint, or from a
    /// session indexed before uuids were stored.
    #[test]
    fn an_unresolvable_anchor_marks_nothing() {
        let mut db = setup();
        commit(
            &mut db,
            1,
            &boundaries(
                vec![with_uuid("a", "u1"), with_uuid("b", "u2")],
                vec![CompactionBoundary::Before("not-in-the-index".into())],
                true,
            ),
        );
        assert_eq!(
            rows(&db),
            vec![
                ("a".into(), "current".into()),
                ("b".into(), "current".into())
            ]
        );
    }

    /// `compaction_after[i]` counts turns seen BEFORE the marker, so a boundary
    /// mid-chunk splits that chunk rather than superseding all of it.
    #[test]
    fn a_mid_chunk_boundary_splits_the_chunk() {
        let mut db = setup();
        commit(
            &mut db,
            1,
            &chunk(vec![t("a"), t("b"), t("c"), t("d")], vec![2], true),
        );
        assert_eq!(
            rows(&db),
            vec![
                ("a".into(), "superseded".into()),
                ("b".into(), "superseded".into()),
                ("c".into(), "current".into()),
                ("d".into(), "current".into()),
            ]
        );
    }

    /// An adapter that cannot see its runtime's marker must not claim `current`.
    /// Empty `compaction_after` from such an adapter is indistinguishable from a
    /// session that simply has no compaction, and calling both live would assert
    /// that compacted-away content is still context.
    #[test]
    fn an_adapter_that_cannot_track_compaction_records_unknown() {
        let mut db = setup();
        commit(&mut db, 1, &chunk(vec![t("a"), t("b")], vec![], false));
        assert_eq!(counts(&db), vec![("unknown".to_string(), 2)]);
    }

    /// A second compaction must not un-supersede what the first settled, and must
    /// extend the boundary forward.
    #[test]
    fn two_compactions_accumulate_rather_than_fight() {
        let mut db = setup();
        commit(&mut db, 1, &chunk(vec![t("a"), t("b")], vec![], true));
        commit(&mut db, 1, &chunk(vec![t("c")], vec![0], true)); // supersedes a,b
        commit(&mut db, 1, &chunk(vec![t("d")], vec![0], true)); // supersedes a,b,c
        assert_eq!(
            rows(&db),
            vec![
                ("a".into(), "superseded".into()),
                ("b".into(), "superseded".into()),
                ("c".into(), "superseded".into()),
                ("d".into(), "current".into()),
            ],
            "only the newest turn is still context"
        );
    }
}

/// "Never backed up" and "backed up today" must not render the same.
///
/// The whole point of recording backups is an index that holds the only copy of
/// a quarter of its turns — 140,321 of them on the machine this was written
/// on. A missing row rendered as "0 days ago" would turn the one state that
/// needs action into the one that needs none, which is the failure mode the
/// feature exists to prevent.
mod backup_status {
    use tp_db::Db;

    #[test]
    fn absent_is_not_zero_days_ago() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            tp_db::query::backup_status(db.conn()).unwrap().is_none(),
            "a fresh index has never been backed up, and must say so"
        );
    }

    /// One row, overwritten — a log of every backup would be a second thing to
    /// prune, and the question is "how long since", not "how many".
    #[test]
    fn recording_twice_keeps_the_latest_only() {
        let db = Db::open_in_memory().unwrap();
        db.record_backup("/first.db", 100, 1_000).unwrap();
        db.record_backup("/second.db", 250, 2_000).unwrap();

        let b = tp_db::query::backup_status(db.conn()).unwrap().unwrap();
        assert_eq!(b.dest, "/second.db");
        assert_eq!(b.turn_count, 250, "the count is the drift baseline");
        assert_eq!(b.bytes, 2_000);

        let rows: i64 = db
            .conn()
            .query_row("SELECT count(*) FROM backup_status", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "one row, enforced by the CHECK on id");
    }

    /// The turn count is stored so age can be read against drift: 40 days is
    /// fine on an idle machine and alarming on one that added 100k turns since.
    #[test]
    fn the_recorded_count_is_the_drift_baseline() {
        let db = Db::open_in_memory().unwrap();
        db.record_backup("/snap.db", 500, 1_234).unwrap();
        let b = tp_db::query::backup_status(db.conn()).unwrap().unwrap();
        assert_eq!(b.turn_count, 500);
        assert!(b.taken_at > 0, "a timestamp, not a placeholder");
    }
}
