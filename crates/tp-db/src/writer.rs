//! The single-writer path (LLD §3, §6.2). In the P0 CLI this runs inline
//! inside one process; the daemon (P1) funnels concurrent parse-worker output
//! through an mpsc channel into the same `commit_chunk` call so the
//! transaction shape doesn't change when concurrency is added.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use tp_core::turn::{ParseChunk, Role};

#[derive(Debug, Clone)]
pub struct IngestCheckpoint {
    pub inode: i64,
    pub source_path: String,
    pub byte_offset: i64,
    pub last_seq: i64,
    pub mtime_ms: Option<i64>,
}

/// Look up the checkpoint for this INODE — never by path. Tracking position by
/// filename is the anti-pattern that breaks across rotation (LLD §15 #1).
pub fn get_checkpoint(conn: &Connection, inode: i64) -> Result<Option<IngestCheckpoint>> {
    conn.query_row(
        "SELECT inode, source_path, byte_offset, last_seq, mtime_ms
         FROM ingest_state WHERE inode = ?1 AND retired_at IS NULL",
        [inode],
        |r| {
            Ok(IngestCheckpoint {
                inode: r.get(0)?,
                source_path: r.get(1)?,
                byte_offset: r.get(2)?,
                last_seq: r.get(3)?,
                mtime_ms: r.get(4)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// `thinking_state` from what an adapter reported.
///
/// Only `opaque` needs an adapter to say anything — the other two are what the
/// `thinking` text already tells us, so every adapter that predates the column
/// keeps working unchanged and cannot get it wrong by omission.
///
/// `opaque` wins over a non-empty `thinking`: a runtime that gives BOTH a summary
/// and an encrypted payload (codex's `Reasoning` can carry `summary` alongside
/// `encrypted_content`) has reasoning that is only partly readable, and calling
/// that plain `text` would overstate what teleport holds.
fn thinking_state(thinking: &str, opaque: bool) -> &'static str {
    if opaque {
        "opaque"
    } else if thinking.is_empty() {
        "none"
    } else {
        "text"
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// Commit one parsed chunk: upsert the session row, append its turns starting
/// at `last_seq + 1`, and advance the ingest checkpoint — all in one
/// transaction, so a crash mid-commit never leaves turns without an updated
/// offset (which would cause them to be re-parsed and rejected by the
/// `UNIQUE(session_id, seq)` constraint — safe, but worth avoiding).
#[allow(clippy::too_many_arguments)]
pub fn commit_chunk(
    conn: &mut Connection,
    session_id: &str,
    machine_id: &str,
    runtime_id: &str,
    native_id: &str,
    source_path: &str,
    inode: i64,
    mtime_ms: i64,
    chunk: &ParseChunk,
) -> Result<usize> {
    // IMMEDIATE, not the rusqlite default (Deferred). A deferred transaction
    // starts as a READ and promotes on its first write; in WAL mode, if another
    // connection commits in between, the promotion fails with SQLITE_BUSY_
    // SNAPSHOT (517) — and `busy_timeout` does NOT cover that, because it is a
    // snapshot invalidation rather than a lock wait. It was observed on this
    // machine: `tpd: watcher stopped: database is locked: Error code 517:
    // Cannot promote read transaction to write transaction because of writes by
    // another connection`, nine times, each one ending indexing until the
    // daemon was restarted. Taking the write lock at BEGIN makes the contention
    // a WAIT, which busy_timeout does cover.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let prev_last_seq: i64 = tx
        .query_row(
            "SELECT last_seq FROM ingest_state WHERE inode = ?1",
            [inode],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);

    let existing_started_at: Option<i64> = tx
        .query_row(
            "SELECT started_at FROM session WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();

    tx.execute(
        "INSERT INTO session(id, machine_id, runtime_id, native_id, cwd,
                             title_user, title_ai, title_derived,
                             source_path, started_at, last_turn_at, turn_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, 0)
         ON CONFLICT(id) DO UPDATE SET
             cwd         = COALESCE(excluded.cwd, session.cwd),
             -- LAST write wins per source, unlike the old single column which
             -- was first-write-wins. A `/rename` two hours in is the newest
             -- statement of what the session is called, and freezing the first
             -- one is how a real title loses to a stale one. Only a non-NULL
             -- new value replaces: a later chunk that carries no title must not
             -- erase one an earlier chunk found.
             title_user    = COALESCE(excluded.title_user, session.title_user),
             title_ai      = COALESCE(excluded.title_ai, session.title_ai),
             -- The DERIVED one stays first-write-wins: it comes from the first
             -- user message, and a resumed read of the tail must not retitle
             -- the session with whatever was said an hour in.
             title_derived = COALESCE(session.title_derived, excluded.title_derived),
             source_path = excluded.source_path",
        params![
            session_id,
            machine_id,
            runtime_id,
            native_id,
            chunk.meta.cwd,
            chunk.meta.title_user,
            chunk.meta.title_ai,
            chunk.meta.title_derived,
            source_path,
            existing_started_at.or(chunk.meta.started_at),
        ],
    )?;

    let mut seq = prev_last_seq;
    let mut last_ts: Option<i64> = None;
    {
        let mut ins = tx.prepare(
            "INSERT INTO turn(session_id, seq, role, ts, text, thinking, tool_calls, tokens_in, tokens_out,
                              uuid, parent_uuid, model, cache_read_tokens, cache_creation_tokens,
                              thinking_state, surface, sidechain)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )?;
        for turn in &chunk.turns {
            seq += 1;
            let tool_calls_json = serde_json::to_string(&turn.tool_calls)?;
            ins.execute(params![
                session_id,
                seq,
                role_str(turn.role),
                turn.ts,
                turn.text,
                turn.thinking,
                tool_calls_json,
                turn.tokens_in,
                turn.tokens_out,
                turn.prov.uuid,
                turn.prov.parent_uuid,
                turn.prov.model,
                turn.prov.cache_read_tokens,
                turn.prov.cache_creation_tokens,
                thinking_state(&turn.thinking, turn.thinking_opaque),
                // `current` is a CLAIM, and only an adapter that can see its
                // runtime's compaction marker is entitled to make it. Otherwise
                // the row keeps the schema default, `unknown` — see
                // `ParseChunk::tracks_compaction`.
                if chunk.tracks_compaction {
                    "current"
                } else {
                    "unknown"
                },
                turn.prov.sidechain,
            ])?;
            if turn.ts.is_some() {
                last_ts = turn.ts;
            }
        }
    }

    // A compaction boundary supersedes turns this chunk did not write. The ones
    // before it were committed by an EARLIER chunk and are already in the table,
    // so this is a back-update — cheap and bounded (one statement per boundary,
    // over the `(session_id, surface)` index from migration 0012), and it happens
    // inside the same transaction as the insert, so a crash cannot leave a
    // boundary recorded with the turns it supersedes still marked current.
    //
    // `surface != 'superseded'` keeps a second compaction from rewriting rows the
    // first already settled.
    for boundary in &chunk.compaction {
        match boundary {
            // Positional (Claude Code): `At(i)` counts turns seen BEFORE the
            // marker, so `prev_last_seq + i` is the seq of the last superseded
            // turn — hence `<=`.
            tp_core::turn::CompactionBoundary::At(i) => {
                tx.execute(
                    "UPDATE turn SET surface = 'superseded'
                     WHERE session_id = ?1 AND seq <= ?2 AND surface != 'superseded'",
                    params![session_id, prev_last_seq + *i as i64],
                )?;
            }
            // Anchored (pi): the named entry is KEPT, so `<` and not `<=`.
            //
            // A subquery rather than a lookup-then-update: if the anchor is not
            // in the index — behind a checkpoint, or from a session teleport
            // indexed before it stored uuids — the inner SELECT is NULL, the
            // comparison is NULL, and NOTHING is marked. That is the conservative
            // failure: reporting live context as superseded is worse than leaving
            // superseded content unmarked.
            tp_core::turn::CompactionBoundary::Before(uuid) => {
                tx.execute(
                    "UPDATE turn SET surface = 'superseded'
                     WHERE session_id = ?1
                       AND surface != 'superseded'
                       AND seq < (SELECT seq FROM turn WHERE session_id = ?1 AND uuid = ?2)",
                    params![session_id, uuid],
                )?;
            }
        }
    }

    if seq > prev_last_seq {
        tx.execute(
            "UPDATE session SET turn_count = turn_count + ?2, last_turn_at = COALESCE(?3, last_turn_at) WHERE id = ?1",
            params![session_id, seq - prev_last_seq, last_ts],
        )?;
    }

    tx.execute(
        "INSERT INTO ingest_state(inode, source_path, session_id, byte_offset, last_seq, mtime_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(inode) DO UPDATE SET
             source_path = excluded.source_path,
             session_id  = excluded.session_id,
             byte_offset = excluded.byte_offset,
             last_seq    = excluded.last_seq,
             mtime_ms    = excluded.mtime_ms",
        params![
            inode,
            source_path,
            session_id,
            chunk.new_offset as i64,
            seq,
            mtime_ms
        ],
    )?;

    tx.commit()?;
    Ok(chunk.turns.len())
}

/// Commit turns a runtime PUSHED to us, rather than turns parsed from a file
/// teleport discovered.
///
/// Why this exists beside `commit_chunk`: that one is built around
/// `(inode, byte_offset)` — the resumable-append contract for a JSONL file on
/// disk (LLD §15 #1). A pushing runtime has no file for us to checkpoint
/// against; it has session ids and turns. Forcing it through a synthetic inode
/// would put a lie in `ingest_state` that a later real scan of the same runtime
/// could trip over.
///
/// Idempotency is by `(session_id, prov.uuid)`, which is exactly what migration
/// 0005's partial index was created for. A push path without dedupe is a bug
/// with a known shape: Tencent's `/v3/conversation/add` says in its own source
/// that resending duplicates writes duplicates. A turn with no uuid cannot be
/// deduped and is appended — reported to the caller so a runtime that could
/// supply one learns that it should.
///
/// `seq` continues from whatever this session already has, so a pushed session
/// keeps a contiguous ordinal even across restarts of the pushing runtime.
pub struct PushOutcome {
    pub inserted: usize,
    pub duplicates: usize,
    /// Turns accepted without a uuid, and therefore not dedupable.
    pub unkeyed: usize,
}

pub fn commit_pushed(
    conn: &mut Connection,
    session_id: &str,
    machine_id: &str,
    runtime_id: &str,
    native_id: &str,
    meta: &tp_core::turn::SessionMeta,
    turns: &[tp_core::turn::NormalizedTurn],
) -> Result<PushOutcome> {
    // IMMEDIATE, not the rusqlite default (Deferred). A deferred transaction
    // starts as a READ and promotes on its first write; in WAL mode, if another
    // connection commits in between, the promotion fails with SQLITE_BUSY_
    // SNAPSHOT (517) — and `busy_timeout` does NOT cover that, because it is a
    // snapshot invalidation rather than a lock wait. It was observed on this
    // machine: `tpd: watcher stopped: database is locked: Error code 517:
    // Cannot promote read transaction to write transaction because of writes by
    // another connection`, nine times, each one ending indexing until the
    // daemon was restarted. Taking the write lock at BEGIN makes the contention
    // a WAIT, which busy_timeout does cover.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let existing_started_at: Option<i64> = tx
        .query_row(
            "SELECT started_at FROM session WHERE id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();

    // `source_path` is NULL: there is no file. `tp turns` on a pushed session
    // reads the index rather than a transcript, which is the honest state —
    // inventing a path would make a scan-provider read fail confusingly later.
    tx.execute(
        "INSERT INTO session(id, machine_id, runtime_id, native_id, cwd,
                             title_user, title_ai, title_derived,
                             started_at, last_turn_at, turn_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, 0)
         ON CONFLICT(id) DO UPDATE SET
             cwd           = COALESCE(excluded.cwd, session.cwd),
             title_user    = COALESCE(excluded.title_user, session.title_user),
             title_ai      = COALESCE(excluded.title_ai, session.title_ai),
             title_derived = COALESCE(session.title_derived, excluded.title_derived)",
        params![
            session_id,
            machine_id,
            runtime_id,
            native_id,
            meta.cwd,
            meta.title_user,
            meta.title_ai,
            meta.title_derived,
            existing_started_at.or(meta.started_at),
        ],
    )?;

    let mut seq: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM turn WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let mut out = PushOutcome {
        inserted: 0,
        duplicates: 0,
        unkeyed: 0,
    };
    let mut last_ts: Option<i64> = None;
    {
        let mut seen =
            tx.prepare("SELECT 1 FROM turn WHERE session_id = ?1 AND uuid = ?2 LIMIT 1")?;
        let mut ins = tx.prepare(
            "INSERT INTO turn(session_id, seq, role, ts, text, thinking, tool_calls, tokens_in, tokens_out,
                              uuid, parent_uuid, model, cache_read_tokens, cache_creation_tokens,
                              thinking_state, sidechain)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )?;
        for turn in turns {
            match &turn.prov.uuid {
                Some(u) => {
                    if seen.exists(params![session_id, u])? {
                        out.duplicates += 1;
                        continue;
                    }
                }
                None => out.unkeyed += 1,
            }
            seq += 1;
            ins.execute(params![
                session_id,
                seq,
                role_str(turn.role),
                turn.ts,
                turn.text,
                turn.thinking,
                serde_json::to_string(&turn.tool_calls)?,
                turn.tokens_in,
                turn.tokens_out,
                turn.prov.uuid,
                turn.prov.parent_uuid,
                turn.prov.model,
                turn.prov.cache_read_tokens,
                turn.prov.cache_creation_tokens,
                thinking_state(&turn.thinking, turn.thinking_opaque),
                turn.prov.sidechain,
            ])?;
            out.inserted += 1;
            if turn.ts.is_some() {
                last_ts = turn.ts;
            }
        }
    }

    if out.inserted > 0 {
        tx.execute(
            "UPDATE session
                SET turn_count   = (SELECT COUNT(*) FROM turn WHERE session_id = ?1),
                    last_turn_at = COALESCE(?2, last_turn_at)
              WHERE id = ?1",
            params![session_id, last_ts],
        )?;
    }
    tx.commit()?;
    Ok(out)
}
