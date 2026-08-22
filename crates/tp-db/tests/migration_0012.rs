//! Migration 0012 — the three facts teleport was told and stored from none of
//! them.
//!
//! These run against a database built the way a REAL upgrade builds one: the
//! pre-0012 schema, populated, and only then migrated. A test that creates the
//! final schema directly would pass while the ALTER/UPDATE path was broken,
//! which is the only path a user's 1.4 GB index will ever take.

use rusqlite::Connection;

/// The schema as it stood at 0011, reduced to what 0012 touches. Deliberately
/// hand-written rather than assembled from the migration files: if a future
/// change to 0001 silently drops `session.title`, this test must fail rather
/// than quietly test something else.
const PRE_0012: &str = "
CREATE TABLE session (
  id TEXT PRIMARY KEY, machine_id TEXT NOT NULL, runtime_id TEXT NOT NULL,
  native_id TEXT NOT NULL, cwd TEXT, title TEXT, source_path TEXT,
  started_at INTEGER, last_turn_at INTEGER, turn_count INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE TABLE turn (
  id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, seq INTEGER NOT NULL,
  role TEXT NOT NULL, ts INTEGER, text TEXT, thinking TEXT, tool_calls TEXT,
  tokens_in INTEGER, tokens_out INTEGER,
  uuid TEXT, parent_uuid TEXT, model TEXT,
  cache_read_tokens INTEGER, cache_creation_tokens INTEGER,
  UNIQUE(session_id, seq)
) STRICT;
";

fn migrated_with(seed: &str) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(PRE_0012).unwrap();
    conn.execute_batch(seed).unwrap();
    conn.execute_batch(include_str!(
        "../migrations/0012_surface_and_title_provenance.sql"
    ))
    .unwrap();
    conn
}

fn one<T: rusqlite::types::FromSql>(conn: &Connection, sql: &str) -> T {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// `surface` defaults to `unknown`, NOT `current`.
///
/// The distinction is the entire point of the column: a row ingested before the
/// fold was implemented is a row whose status teleport does not know, and
/// calling it `current` would assert that compacted-away content is still part
/// of the conversation. `unknown` is what a windowed search must be able to
/// report honestly.
#[test]
fn existing_turns_become_unknown_not_current() {
    let conn = migrated_with(
        "INSERT INTO session(id,machine_id,runtime_id,native_id) VALUES ('s','m','claude_code','n');
         INSERT INTO turn(session_id,seq,role,text,thinking) VALUES ('s',0,'user','hi','');",
    );
    assert_eq!(
        one::<String>(&conn, "SELECT surface FROM turn WHERE seq=0"),
        "unknown"
    );
}

/// A non-empty `thinking` IS readable text, so the backfill must say so —
/// defaulting it to `none` would have the column contradict the data next to it.
#[test]
fn thinking_state_is_backfilled_from_the_text_that_is_already_there() {
    let conn = migrated_with(
        "INSERT INTO session(id,machine_id,runtime_id,native_id) VALUES ('s','m','claude_code','n');
         INSERT INTO turn(session_id,seq,role,text,thinking) VALUES ('s',0,'assistant','a','let me think');
         INSERT INTO turn(session_id,seq,role,text,thinking) VALUES ('s',1,'assistant','b','');
         INSERT INTO turn(session_id,seq,role,text,thinking) VALUES ('s',2,'user','c',NULL);",
    );
    assert_eq!(
        one::<String>(&conn, "SELECT thinking_state FROM turn WHERE seq=0"),
        "text"
    );
    assert_eq!(
        one::<String>(&conn, "SELECT thinking_state FROM turn WHERE seq=1"),
        "none",
        "empty string is not reasoning"
    );
    assert_eq!(
        one::<String>(&conn, "SELECT thinking_state FROM turn WHERE seq=2"),
        "none",
        "NULL is not reasoning either"
    );
}

/// The invariant a later reader will rely on: `thinking_state` and `thinking`
/// must never disagree after the migration.
#[test]
fn state_and_text_never_contradict_each_other() {
    let conn = migrated_with(
        "INSERT INTO session(id,machine_id,runtime_id,native_id) VALUES ('s','m','pi','n');
         INSERT INTO turn(session_id,seq,role,text,thinking) VALUES ('s',0,'assistant','a','x');
         INSERT INTO turn(session_id,seq,role,text,thinking) VALUES ('s',1,'assistant','b','');
         INSERT INTO turn(session_id,seq,role,text,thinking) VALUES ('s',2,'user','c',NULL);",
    );
    assert_eq!(
        one::<i64>(
            &conn,
            "SELECT count(*) FROM turn WHERE thinking_state='text' AND (thinking IS NULL OR thinking='')"
        ),
        0
    );
    assert_eq!(
        one::<i64>(
            &conn,
            "SELECT count(*) FROM turn WHERE thinking_state='none' AND thinking IS NOT NULL AND thinking!=''"
        ),
        0
    );
}

/// Everything in `title` today was produced by teleport's own derivation, so it
/// belongs in `title_derived` — which is what it always was. The runtime-sourced
/// columns must stay NULL rather than inherit a truncated first user message and
/// thereby outrank a real title on the next read.
#[test]
fn the_old_title_moves_to_derived_and_nothing_claims_a_runtime_source() {
    let conn = migrated_with(
        "INSERT INTO session(id,machine_id,runtime_id,native_id,title)
           VALUES ('s','m','claude_code','n','fix the pairing bug');",
    );
    assert_eq!(
        one::<String>(&conn, "SELECT title_derived FROM session WHERE id='s'"),
        "fix the pairing bug"
    );
    assert_eq!(
        one::<i64>(
            &conn,
            "SELECT count(*) FROM session WHERE title_user IS NOT NULL OR title_ai IS NOT NULL"
        ),
        0,
        "teleport has never read a native title; migrating as if it had would \
         let a derived value win the COALESCE against a real one"
    );
}

/// An empty title is not a title. It must not become an empty `title_derived`,
/// because `COALESCE` would then select "" over a genuine title found later.
#[test]
fn an_empty_or_missing_title_does_not_become_a_derived_one() {
    let conn = migrated_with(
        "INSERT INTO session(id,machine_id,runtime_id,native_id,title) VALUES ('a','m','pi','n','');
         INSERT INTO session(id,machine_id,runtime_id,native_id) VALUES ('b','m','pi','n2');",
    );
    assert_eq!(
        one::<i64>(
            &conn,
            "SELECT count(*) FROM session WHERE title_derived IS NOT NULL"
        ),
        0
    );
}

/// `title` is kept, not dropped: a rebuild repopulates the new columns from
/// source, and keeping the old value makes a bad rebuild recoverable instead of
/// destructive.
#[test]
fn the_original_title_column_survives_the_migration() {
    let conn = migrated_with(
        "INSERT INTO session(id,machine_id,runtime_id,native_id,title)
           VALUES ('s','m','dsh','n','original');",
    );
    assert_eq!(
        one::<String>(&conn, "SELECT title FROM session WHERE id='s'"),
        "original"
    );
}

/// Provenance came in 0005 and is NOT re-added here — this pins that 0012 does
/// not disturb it, since the tree it records is the one thing teleport already
/// captures correctly.
#[test]
fn the_provenance_columns_from_0005_are_untouched() {
    let conn = migrated_with(
        "INSERT INTO session(id,machine_id,runtime_id,native_id) VALUES ('s','m','pi','n');
         INSERT INTO turn(session_id,seq,role,text,thinking,uuid,parent_uuid)
           VALUES ('s',0,'user','hi','','abc12345','def67890');",
    );
    assert_eq!(
        one::<String>(&conn, "SELECT uuid FROM turn WHERE seq=0"),
        "abc12345"
    );
    assert_eq!(
        one::<String>(&conn, "SELECT parent_uuid FROM turn WHERE seq=0"),
        "def67890"
    );
    assert_eq!(
        one::<i64>(&conn, "SELECT sidechain FROM turn WHERE seq=0"),
        0
    );
}

/// The read-time resolution, asserted against the SQL that actually serves it.
///
/// Precedence lives in `list_sessions`' COALESCE, not at write time, so a
/// `/rename` arriving two hours into a session wins on the next read with
/// nothing rewritten. This is Codex's ordering — it resolves Claude Code's
/// titles the same way when importing sessions.
#[test]
fn read_precedence_is_user_then_ai_then_derived() {
    let db = tp_db::Db::open_in_memory().unwrap();
    db.ensure_self_machine("m", "h").unwrap();
    db.ensure_runtime("claude_code", "/r").unwrap();
    let conn = db.conn();
    conn.execute_batch(
        "INSERT INTO session(id,machine_id,runtime_id,native_id,title_user,title_ai,title_derived)
           VALUES ('all','m','claude_code','1','chosen','generated','first message');
         INSERT INTO session(id,machine_id,runtime_id,native_id,title_ai,title_derived)
           VALUES ('ai','m','claude_code','2','generated','first message');
         INSERT INTO session(id,machine_id,runtime_id,native_id,title_derived)
           VALUES ('only','m','claude_code','3','first message');
         INSERT INTO session(id,machine_id,runtime_id,native_id)
           VALUES ('none','m','claude_code','4');
         INSERT INTO turn(session_id,seq,role,ts,text,thinking) VALUES ('all',0,'user',1000,'x','');
         INSERT INTO turn(session_id,seq,role,ts,text,thinking) VALUES ('ai',0,'user',1000,'x','');
         INSERT INTO turn(session_id,seq,role,ts,text,thinking) VALUES ('only',0,'user',1000,'x','');
         INSERT INTO turn(session_id,seq,role,ts,text,thinking) VALUES ('none',0,'user',1000,'x','');",
    )
    .unwrap();

    let rows = tp_db::query::list_sessions(conn, None, None, None, 50).unwrap();
    let title = |id: &str| rows.iter().find(|r| r.id == id).unwrap().title.clone();
    assert_eq!(
        title("all").as_deref(),
        Some("chosen"),
        "a person outranks a model"
    );
    assert_eq!(
        title("ai").as_deref(),
        Some("generated"),
        "a model outranks teleport"
    );
    assert_eq!(title("only").as_deref(), Some("first message"));
    assert_eq!(title("none"), None, "no title is not an empty title");
}
