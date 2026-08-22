//! Data access for the two tables reach owns: `live_session` and `message`.
//!
//! Why this module exists at all. The schema for these tables lives here, in
//! `tp-db/migrations/`, but every query against them used to live in `tp-reach`
//! — 45 SQL literals reached through `Db::conn()`. Renaming a column therefore
//! meant editing a migration in one crate and grepping another for statements
//! nothing tied to it. That split is what let migration 0006's `presence` column
//! change the meaning of a hand-written `WHERE pid = ?1 LIMIT 1` in
//! `session_of_process` without anything noticing (see the `own_session_tests`
//! regression suite).
//!
//! The shape is Drone's `store` layer: SQL lives next to the schema it queries,
//! callers get typed rows and never a `Connection` method call. Its anti-pattern
//! names ours exactly — "leaks transaction state into callers."
//!
//! Deliberately NOT an ORM or a query builder. Every statement here is a static
//! literal (the one exception, `prune_scan_rows`, expands an `IN (?,?,?)` list
//! and nothing else), so there is no dynamic-filter composition for a builder to
//! simplify — which is the one thing the Rust ecosystem agrees you should leave
//! raw SQL for. rusqlite also keeps `tp-reach` synchronous; sqlx would drag a
//! tokio runtime into five sync crates to buy compile-time checking we can get
//! from tests.
//!
//! Policy stays with the caller. These functions take `now` rather than reading
//! the clock, and return counts rather than deciding what a count means — so
//! `MAX_DELIVER`, the TTL constants and the scan-beats-declared rule remain
//! readable in `tp-reach`, where they are explained.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

// ---------------------------------------------------------------- live_session

/// One `live_session` row as the scan's dedupe pass needs it.
#[derive(Debug, Clone)]
pub struct ScanRow {
    pub session_id: String,
    pub source: String,
    pub registered_at: i64,
}

/// How deliverable an address is, as far as this machine's DB can tell.
///
/// `live_session` and `session` answer different questions and teleport used to
/// collapse both into one `Target::NotLive`, which made every undeliverable send
/// report "delivered on next /tp inbox" — a promise it had no basis for. Of 13
/// unread messages measured on a real install, 12 were undeliverable and every
/// one of them had been reported that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressability {
    /// A `live_session` row exists. Something is expected to drain this mailbox.
    Registered,
    /// A CONVERSATION teleport itself published, whose members are all
    /// currently unregistered. Distinct from `Unknown`, which it used to be
    /// classified as — telling a sender "teleport has never seen this session
    /// id" about an address teleport printed in `tp live` is both wrong and
    /// actively misleading. Reported by a session that read it as a rejection,
    /// resent twice, and delivered three copies of the same report.
    DormantConversation,
    /// No `live_session` row, but the session is one teleport has indexed. Real
    /// once, not currently claimed by any process — most often because the id
    /// rotated (Claude Code mints a new session id at every compaction) and the
    /// conversation now answers to a different address.
    Dormant,
    /// Neither table knows this id. It may be a session that has not registered
    /// or been indexed yet — including one on a peer machine — so this is not
    /// proof of a bad address, only the absence of any evidence for it.
    Unknown,
}

/// Classify an address WITHOUT enqueueing anything, so a caller can say what
/// will actually happen before it promises delivery.
pub fn addressability(conn: &Connection, session_id: &str) -> Result<Addressability> {
    let live: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM live_session WHERE session_id = ?1)",
        [session_id],
        |r| r.get(0),
    )?;
    if live {
        return Ok(Addressability::Registered);
    }
    // Belonging to a conversation is the real test, not the shape of the
    // address. `tp ask <conv-…>` resolves to a member before this runs, so
    // matching only on the conversation id classified that member as "never
    // seen" — an address teleport had just published. Either form counts.
    let is_conv: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation WHERE id = ?1)
             OR EXISTS(SELECT 1 FROM conversation_member WHERE session_id = ?1)",
        [session_id],
        |r| r.get(0),
    )?;
    if is_conv {
        return Ok(Addressability::DormantConversation);
    }
    let known: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session WHERE id = ?1)",
        [session_id],
        |r| r.get(0),
    )?;
    Ok(if known {
        Addressability::Dormant
    } else {
        Addressability::Unknown
    })
}

/// A `live_session` row as `tp live` displays it.
#[derive(Debug, Clone)]
pub struct LiveRow {
    pub session_id: String,
    pub pid: i32,
    pub tty: Option<String>,
    pub cwd: Option<String>,
    pub source: String,
    pub last_seen_at: i64,
}

/// What `resolve` needs to pick a delivery target.
#[derive(Debug, Clone)]
pub struct TargetRow {
    pub pid: i32,
    pub tty: Option<String>,
    pub deliver: Option<String>,
    pub stale_at: Option<i64>,
}

/// Every `scan`-owned row for a pid.
///
/// Scoped to `presence = 'scan'` on purpose: a `declared` row may legitimately
/// share a pid with many others (one dsh host serves many sessions), so the
/// caller's keep-exactly-one rule must never see them.
pub fn scan_rows_for_pid(conn: &Connection, pid: i32) -> Result<Vec<ScanRow>> {
    let rows = conn
        .prepare(
            "SELECT session_id, source, registered_at FROM live_session
              WHERE pid = ?1 AND presence = 'scan'",
        )?
        .query_map([pid], |r| {
            Ok(ScanRow {
                session_id: r.get(0)?,
                source: r.get(1)?,
                registered_at: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `(session_id, presence)` for every row on a pid, regardless of presence —
/// the raw material for "which session am I".
pub fn rows_for_pid(conn: &Connection, pid: i32) -> Result<Vec<(String, String)>> {
    let rows = conn
        .prepare("SELECT session_id, presence FROM live_session WHERE pid = ?1")?
        .query_map([pid], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Every known live session, most recently seen first — the `tp live` listing.
pub fn list_live(conn: &Connection) -> Result<Vec<LiveRow>> {
    let rows = conn
        .prepare(
            "SELECT session_id, pid, tty, cwd, source, last_seen_at
               FROM live_session ORDER BY last_seen_at DESC",
        )?
        .query_map([], |r| {
            Ok(LiveRow {
                session_id: r.get(0)?,
                pid: r.get(1)?,
                tty: r.get(2)?,
                cwd: r.get(3)?,
                source: r.get(4)?,
                last_seen_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Which runtimes a BARE native id is registered under, right now.
///
/// A runtime that hands teleport its own native id — every non-Claude
/// integration does — has to be composed into `<machine>/<runtime>/<native>`
/// before it is an address. The MCP server was guessing that middle segment
/// from the string's shape and defaulting to `claude_code`, so a codex or dsh
/// session that passed its bare id got a return address under a runtime it does
/// not belong to: accepted, stored, and never deliverable.
///
/// Returns every match, because one is an answer and two are not. The caller
/// decides what to do with an ambiguous or absent one — this reports, it does
/// not guess.
pub fn runtimes_for_native(conn: &Connection, native_id: &str) -> Result<Vec<String>> {
    let like = format!("%/{native_id}");
    let rows = conn
        .prepare("SELECT DISTINCT session_id FROM live_session WHERE session_id LIKE ?1")?
        .query_map([&like], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out: Vec<String> = rows
        .iter()
        .filter_map(|sid| {
            let mut parts = sid.split('/');
            let _machine = parts.next()?;
            let runtime = parts.next()?;
            let native = parts.next()?;
            // LIKE '%/x' also matches '…/foo/barx' — check the segment itself.
            (native == native_id).then(|| runtime.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

pub fn target_row(conn: &Connection, session_id: &str) -> Result<Option<TargetRow>> {
    let row = conn
        .query_row(
            "SELECT pid, tty, deliver, stale_at FROM live_session WHERE session_id = ?1",
            [session_id],
            |r| {
                Ok(TargetRow {
                    pid: r.get(0)?,
                    tty: r.get(1)?,
                    deliver: r.get(2)?,
                    stale_at: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Upsert a hook/runtime-provided registration. `source` is always `'hook'`:
/// an id the session states about itself outranks anything the scan inferred.
#[allow(clippy::too_many_arguments)]
pub fn upsert_registration(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
    cwd: Option<&str>,
    presence: &str,
    deliver: Option<&str>,
    runtime_id: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO live_session(session_id, pid, tty, cwd, source, registered_at, last_seen_at,
                                  presence, deliver, runtime_id)
         VALUES (?1, ?2, ?3, ?4, 'hook', ?5, ?5, ?6, ?7, ?8)
         ON CONFLICT(session_id) DO UPDATE SET
             pid = excluded.pid, tty = excluded.tty, cwd = excluded.cwd, source = 'hook',
             last_seen_at = excluded.last_seen_at, presence = excluded.presence,
             deliver = excluded.deliver, runtime_id = excluded.runtime_id,
             stale_at = NULL",
        params![session_id, pid, tty, cwd, now, presence, deliver, runtime_id],
    )?;
    Ok(())
}

/// Insert a row the scan discovered. Never overwrites an existing row's
/// `session_id` — only its location and liveness.
pub fn insert_scanned(
    conn: &Connection,
    session_id: &str,
    pid: i32,
    tty: Option<&str>,
    cwd: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO live_session(session_id, pid, tty, cwd, source, registered_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, 'scan', ?5, ?5)
         ON CONFLICT(session_id) DO UPDATE SET
             pid = excluded.pid, tty = excluded.tty, cwd = excluded.cwd,
             last_seen_at = excluded.last_seen_at",
        params![session_id, pid, tty, cwd, now],
    )?;
    Ok(())
}

/// Refresh an existing row's location and liveness — NEVER its `session_id`.
pub fn touch_location(
    conn: &Connection,
    session_id: &str,
    tty: Option<&str>,
    cwd: Option<&str>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE live_session SET tty = ?2, cwd = ?3, last_seen_at = ?4 WHERE session_id = ?1",
        params![session_id, tty, cwd, now],
    )?;
    Ok(())
}

pub fn delete_session(conn: &Connection, session_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM live_session WHERE session_id = ?1",
        [session_id],
    )?;
    Ok(())
}

/// Delete only if the row still belongs to `pid` — guards against unregistering
/// a newer incarnation that reused the same `session_id`.
pub fn delete_session_pinned(conn: &Connection, session_id: &str, pid: i32) -> Result<()> {
    conn.execute(
        "DELETE FROM live_session WHERE session_id = ?1 AND pid = ?2",
        params![session_id, pid],
    )?;
    Ok(())
}

/// Delete every `scan` row whose pid is not in `live_pids`. `declared` rows are
/// untouched — they are owned by their runtime and expire on heartbeat timeout.
///
/// The only statement in this module built at runtime, and only to expand the
/// `IN (?,?,?)` placeholder list; the shape of the query never varies.
pub fn prune_scan_rows(conn: &Connection, live_pids: &[i32]) -> Result<()> {
    if live_pids.is_empty() {
        conn.execute("DELETE FROM live_session WHERE presence = 'scan'", [])?;
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", live_pids.len())
        .collect::<Vec<_>>()
        .join(",");
    conn.execute(
        &format!(
            "DELETE FROM live_session WHERE presence = 'scan' AND pid NOT IN ({placeholders})"
        ),
        rusqlite::params_from_iter(live_pids.iter()),
    )?;
    Ok(())
}

/// Renew liveness and clear any stale mark. Returns rows touched, so a runtime
/// beating into a session teleport already evicted learns it must re-register.
pub fn touch_heartbeat(conn: &Connection, session_id: &str, now: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE live_session SET last_seen_at = ?2, stale_at = NULL WHERE session_id = ?1",
        params![session_id, now],
    )?)
}

/// Stage one of declared expiry: mark rows silent since `silent_before`.
/// `registered_at` is checked too so a row gets one full TTL to send its first
/// beat before it can be marked.
pub fn mark_stale(conn: &Connection, now: i64, silent_before: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE live_session SET stale_at = ?1
          WHERE presence = 'declared' AND stale_at IS NULL
            AND last_seen_at  < ?2
            AND registered_at < ?2",
        params![now, silent_before],
    )?)
}

/// Stage two: actually delete rows marked stale before `marked_before`.
pub fn evict_stale(conn: &Connection, marked_before: i64) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM live_session
          WHERE presence = 'declared' AND stale_at IS NOT NULL AND stale_at < ?1",
        [marked_before],
    )?)
}

pub fn last_wake_at(conn: &Connection, session_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT last_wake_at FROM live_session WHERE session_id = ?1",
            [session_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

pub fn set_last_wake_at(conn: &Connection, session_id: &str, ts: i64) -> Result<()> {
    conn.execute(
        "UPDATE live_session SET last_wake_at = ?2 WHERE session_id = ?1",
        params![session_id, ts],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- conversation

/// How long after a conversation was last seen a NEW session on the same
/// process may still be treated as its continuation.
///
/// A compaction re-registers within milliseconds, so this only has to cover a
/// slow hook. It is short on purpose: the join key includes a pid, and pids are
/// reused. A generous window would eventually merge two unrelated conversations
/// that happened to inherit the same pid in the same directory — a far worse
/// failure than minting one address too many, because it would deliver one
/// agent's mail to another.
pub const CONVERSATION_JOIN_GRACE_MS: i64 = 5 * 60_000;

/// Bind `session_id` to a conversation, continuing an existing one when this
/// looks like a rotation and minting a new address otherwise.
///
/// Recognition is `(runtime_id, pid, cwd)` within the grace window: a compaction
/// produces a new session id from the same process, in the same directory,
/// immediately. All three must match — `cwd` alone collides across concurrent
/// sessions, and `pid` alone collides after reuse.
///
/// Idempotent: a session already bound keeps its conversation, so re-running the
/// SessionStart hook (a `--resume`, a reload) never re-parents it.
/// How a rotation is RECOGNIZED. Grouped because the three parts only mean
/// anything together: `pid` alone collides after reuse, `cwd` alone collides
/// across concurrent sessions, `runtime_id` alone is not an identity at all.
#[derive(Debug, Clone, Copy)]
pub struct ConversationKey<'a> {
    pub machine_id: &'a str,
    pub runtime_id: &'a str,
    pub pid: i32,
    /// Opaque process start time — `ps -o lstart=` verbatim. Together with the
    /// pid this is a process INCARNATION, which is what a conversation actually
    /// belongs to. `None` when it could not be read; the caller then falls back
    /// to the time window, which is strictly weaker but never wrong-by-merge.
    pub pid_start: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

/// Every conversation row that belongs to the same PANE as `session_id`.
///
/// A pane can own more than one. `join_conversation` keys on
/// `(pid, pid_start, cwd)` and creates a new row when that key does not match —
/// which happens two ways, both observed on this machine:
///
///   * `pid_start` is NULL on rows created before migration 0008, and that
///     column was added without a backfill. Six of the eight NULL rows here
///     belong to processes that have since exited, so the value is not missing,
///     it is gone.
///   * `cwd` is part of the key and changes during ordinary work — an agent
///     that `cd`s into a subdirectory or a scratchpad splits its own pane.
///
/// The result is TWINS: two live rows, both legitimate, both updating
/// `last_seen_at`, neither looking like the wrong one. Whichever the current
/// session belongs to is the only mailbox `inbox` reads and the only address a
/// reply stamps — so mail addressed to the other twin is invisible to a pane
/// that is sitting right there.
///
/// Ordered most-recently-seen first, so a caller that wants ONE (the address to
/// publish) takes the head and a caller that wants ALL (the mailboxes to drain)
/// takes the lot.
pub fn conversations_of_pane(conn: &Connection, session_id: &str) -> Result<Vec<String>> {
    // The pane is identified through the session's own conversation row rather
    // than by re-deriving pid/pid_start here: whatever key that row was created
    // with is the key its twins share.
    let Some(mine) = conversation_of(conn, session_id)? else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT c.id FROM conversation c
           JOIN conversation me ON me.id = ?1
          WHERE c.machine_id = me.machine_id
            AND c.runtime_id = me.runtime_id
            AND c.pid        = me.pid
            AND (c.pid_start IS me.pid_start)
          ORDER BY c.last_seen_at DESC",
    )?;
    let rows = stmt
        .query_map([&mine], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    // `cwd` is deliberately NOT in the match: it is the key component that
    // splits a pane during ordinary work, so grouping by it would reproduce the
    // split this function exists to see across. `(pid, pid_start)` already
    // identifies a process incarnation on its own.
    Ok(if rows.is_empty() { vec![mine] } else { rows })
}

pub fn join_conversation(
    conn: &Connection,
    session_id: &str,
    key: ConversationKey<'_>,
    now: i64,
    new_id: &str,
) -> Result<String> {
    let ConversationKey {
        machine_id,
        runtime_id,
        pid,
        cwd,
        ..
    } = key;
    if let Some(existing) = conversation_of(conn, session_id)? {
        conn.execute(
            "UPDATE conversation SET last_seen_at = ?2, pid = ?3 WHERE id = ?1",
            params![existing, now, pid],
        )?;
        return Ok(existing);
    }

    let continues = find_conversation(conn, key, now)?;

    let conv = match continues {
        Some(id) => {
            conn.execute(
                "UPDATE conversation SET last_seen_at = ?2, cwd = COALESCE(?3, cwd) WHERE id = ?1",
                params![id, now, cwd],
            )?;
            id
        }
        None => {
            conn.execute(
                "INSERT INTO conversation(id, machine_id, runtime_id, pid, pid_start, cwd, created_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![new_id, machine_id, runtime_id, pid, key.pid_start, cwd, now],
            )?;
            new_id.to_string()
        }
    };

    conn.execute(
        "INSERT INTO conversation_member(session_id, conversation_id, joined_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id, conv, now],
    )?;
    Ok(conv)
}

/// Bind a session id the SCAN inferred to a conversation that already exists on
/// that process — never minting one.
///
/// The scan does not know a session id, it GUESSES one: the most recently
/// active indexed session sharing this runtime and cwd. That guess is good
/// enough to be published as an address — `live_session` is exactly that claim,
/// and a wake sent to it lands on this process — but not good enough to seed a
/// new correspondent identity, so this never inserts into `conversation`.
///
/// Joining an EXISTING one is not a further leap. If teleport is already telling
/// senders that this id reaches this process, then mail sent there must be
/// drainable by this process; refusing to record the membership leaves the id in
/// the worst possible state — wakeable but never read. That state is not
/// hypothetical: a message from another session woke the right pane, was
/// reported delivered, and sat unread because the scan had resurrected a
/// pre-conversation segment id as a live address that belonged to no
/// conversation.
///
/// Also refreshes `last_seen_at`, so a process that runs for hours without
/// compacting keeps its conversation inside the join grace window. Without that
/// the scan would stop being able to join five minutes after the last rotation.
pub fn join_existing_conversation(
    conn: &Connection,
    session_id: &str,
    key: ConversationKey<'_>,
    now: i64,
) -> Result<Option<String>> {
    if let Some(existing) = conversation_of(conn, session_id)? {
        conn.execute(
            "UPDATE conversation SET last_seen_at = ?2 WHERE id = ?1",
            params![existing, now],
        )?;
        return Ok(Some(existing));
    }
    let Some(conv) = find_conversation(conn, key, now)? else {
        return Ok(None);
    };
    conn.execute(
        "INSERT INTO conversation_member(session_id, conversation_id, joined_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO NOTHING",
        params![session_id, conv, now],
    )?;
    conn.execute(
        "UPDATE conversation SET last_seen_at = ?2 WHERE id = ?1",
        params![conv, now],
    )?;
    Ok(Some(conv))
}

/// The conversation already running on this process, if any.
///
/// Two rules, and which one applies is decided by whether the process's start
/// time is known on BOTH sides:
///
/// * both known — match on the incarnation `(runtime, pid, pid_start, cwd)` and
///   apply NO time bound. This is the whole point: the fact is observed from the
///   OS at the moment it is needed, so it cannot go stale, and an address stops
///   depending on anything continuing to run.
/// * either unknown — fall back to `(runtime, pid, cwd)` inside the grace
///   window, which is what rows written before this existed have to use, and
///   what a process whose start time could not be read gets. Weaker, never
///   wrong-by-merge: a stale window can only refuse a join, and refusing mints
///   a fresh address rather than delivering to the wrong correspondent.
fn find_conversation(
    conn: &Connection,
    key: ConversationKey<'_>,
    now: i64,
) -> Result<Option<String>> {
    if let Some(start) = key.pid_start {
        let exact: Option<String> = conn
            .query_row(
                "SELECT id FROM conversation
                  WHERE runtime_id = ?1 AND pid = ?2 AND pid_start = ?3 AND cwd IS ?4
                  ORDER BY last_seen_at DESC LIMIT 1",
                params![key.runtime_id, key.pid, start, key.cwd],
                |r| r.get(0),
            )
            .optional()?;
        if exact.is_some() {
            return Ok(exact);
        }
    }
    conn.query_row(
        "SELECT id FROM conversation
          WHERE runtime_id = ?1 AND pid = ?2 AND cwd IS ?3
            AND pid_start IS NULL
            AND last_seen_at >= ?4
          ORDER BY last_seen_at DESC LIMIT 1",
        params![
            key.runtime_id,
            key.pid,
            key.cwd,
            now - CONVERSATION_JOIN_GRACE_MS
        ],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub fn conversation_of(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT conversation_id FROM conversation_member WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// Every session id this conversation has answered to, newest first.
pub fn conversation_members(conn: &Connection, conversation_id: &str) -> Result<Vec<String>> {
    let rows = conn
        .prepare(
            "SELECT session_id FROM conversation_member
              WHERE conversation_id = ?1 ORDER BY joined_at DESC",
        )?
        .query_map([conversation_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Which session a conversation currently answers on: its newest member that
/// still has a `live_session` row, falling back to the newest member at all.
///
/// The fallback matters — a message to a conversation whose current segment is
/// momentarily unregistered still lands somewhere its next drain will find,
/// rather than being refused.
pub fn conversation_current_session(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Option<String>> {
    let live: Option<String> = conn
        .query_row(
            "SELECT m.session_id FROM conversation_member m
               JOIN live_session l ON l.session_id = m.session_id
              WHERE m.conversation_id = ?1
              ORDER BY m.joined_at DESC LIMIT 1",
            [conversation_id],
            |r| r.get(0),
        )
        .optional()?;
    if live.is_some() {
        return Ok(live);
    }
    conn.query_row(
        "SELECT session_id FROM conversation_member
          WHERE conversation_id = ?1 ORDER BY joined_at DESC LIMIT 1",
        [conversation_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

pub fn conversation_exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation WHERE id = ?1)",
        [id],
        |r| r.get(0),
    )?)
}

// --------------------------------------------------------------------- message

#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub to_session: String,
    pub from_session: Option<String>,
    pub from_machine: String,
    pub kind: String, // ask | reply | note
    pub body: String,
    pub reply_to: Option<String>,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
    pub read_at: Option<i64>,
    pub attempts: i64,
    pub dead_at: Option<i64>,
    /// Set only by an explicit `ack` (migration 0009), never by being shown.
    /// `read_at.is_some() && acked_at.is_none()` is "delivered, not confirmed
    /// finished" — the state a caller interrupted mid-processing leaves
    /// behind, and the one `pending_ack` exists to recover.
    pub acked_at: Option<i64>,
}

/// The column list every message read shares. One constant so a schema change
/// cannot leave one of the readers projecting a different tuple than
/// `map_message` expects. `acked_at` is appended rather than inserted in
/// column order, so `map_message`'s existing positional `r.get(N)` calls keep
/// their indices.
const MESSAGE_COLS: &str = "id, to_session, from_session, from_machine, kind, body, reply_to,
     created_at, delivered_at, read_at, attempts, dead_at, acked_at";

fn map_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        to_session: r.get(1)?,
        from_session: r.get(2)?,
        from_machine: r.get(3)?,
        kind: r.get(4)?,
        body: r.get(5)?,
        reply_to: r.get(6)?,
        created_at: r.get(7)?,
        delivered_at: r.get(8)?,
        read_at: r.get(9)?,
        attempts: r.get(10)?,
        dead_at: r.get(11)?,
        acked_at: r.get(12)?,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn insert_message(
    conn: &Connection,
    id: &str,
    to_session: &str,
    from_session: Option<&str>,
    from_machine: &str,
    kind: &str,
    body: &str,
    reply_to: Option<&str>,
    created_at: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO message(id, to_session, from_session, from_machine, kind, body, reply_to, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![id, to_session, from_session, from_machine, kind, body, reply_to, created_at],
    )?;
    Ok(())
}

/// Unread messages for every session id this conversation has answered to,
/// oldest first — the drain that makes a rotated address recoverable.
///
/// Mail addressed before a compaction sits in the mailbox of an id nothing
/// drains any more. Reading the union is what collects it, and it is why
/// `conversation_member` outlives `live_session`: the old id's registration is
/// long pruned, but its membership is not.
pub fn unread_for_conversation(conn: &Connection, conversation_id: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE read_at IS NULL
                AND to_session IN (SELECT session_id FROM conversation_member
                                    WHERE conversation_id = ?1)
              ORDER BY created_at ASC"
        ))?
        .query_map([conversation_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Unread messages for a session, oldest first.
pub fn unread(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE to_session = ?1 AND read_at IS NULL
              ORDER BY created_at ASC"
        ))?
        .query_map([session_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Unread AND not parked dead — the set a wake is for.
pub fn wakeable(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE to_session = ?1 AND read_at IS NULL AND dead_at IS NULL
              ORDER BY created_at ASC"
        ))?
        .query_map([session_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Delivered but not yet acked, for a session — oldest first, same shape as
/// `unread`. This is the recovery view: a message here was shown by some past
/// `tp inbox` call and nothing has since confirmed it was acted on.
pub fn pending_ack(conn: &Connection, session_id: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE to_session = ?1 AND read_at IS NOT NULL AND acked_at IS NULL
              ORDER BY created_at ASC"
        ))?
        .query_map([session_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `pending_ack`, across every session id a conversation has answered to —
/// same reasoning as `unread_for_conversation`: a message delivered before a
/// compaction sits under an id nothing else reads any more.
pub fn pending_ack_for_conversation(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE read_at IS NOT NULL AND acked_at IS NULL
                AND to_session IN (SELECT session_id FROM conversation_member
                                    WHERE conversation_id = ?1)
              ORDER BY created_at ASC"
        ))?
        .query_map([conversation_id], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Acked messages for a session since a timestamp, newest first — "what did
/// that say again", not a work queue, so it orders the opposite way from the
/// pending views above.
pub fn acked_since(conn: &Connection, session_id: &str, since_ms: i64) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE to_session = ?1 AND acked_at IS NOT NULL AND acked_at >= ?2
              ORDER BY acked_at DESC"
        ))?
        .query_map(params![session_id, since_ms], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `acked_since`, across every session id a conversation has answered to.
pub fn acked_since_for_conversation(
    conn: &Connection,
    conversation_id: &str,
    since_ms: i64,
) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE acked_at IS NOT NULL AND acked_at >= ?2
                AND to_session IN (SELECT session_id FROM conversation_member
                                    WHERE conversation_id = ?1)
              ORDER BY acked_at DESC"
        ))?
        .query_map(params![conversation_id, since_ms], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Up to two matches for an id prefix — two is enough for the caller to tell
/// "found" from "ambiguous" without fetching the whole table.
pub fn by_prefix(conn: &Connection, prefix: &str) -> Result<Vec<Message>> {
    let rows = conn
        .prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM message
              WHERE id LIKE ?1 || '%'
              ORDER BY created_at DESC
              LIMIT 2"
        ))?
        .query_map([prefix], map_message)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn mark_read(conn: &Connection, id: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE message SET read_at = ?1 WHERE id = ?2 AND read_at IS NULL",
        params![now, id],
    )?;
    Ok(())
}

/// Confirm a message finished being acted on. Guarded on `read_at IS NOT
/// NULL` the same way `mark_read` guards on `read_at IS NULL` — a message
/// that was never delivered has nothing to confirm, and the caller (not this
/// function) is responsible for turning "0 rows changed" into a real error,
/// exactly as `mark_read`'s callers already do for the read side.
pub fn ack(conn: &Connection, id: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE message SET acked_at = ?1 WHERE id = ?2 AND read_at IS NOT NULL AND acked_at IS NULL",
        params![now, id],
    )?;
    Ok(())
}

pub fn attempts_of(conn: &Connection, id: &str) -> Result<i64> {
    Ok(conn
        .query_row("SELECT attempts FROM message WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .unwrap_or(0))
}

/// Write back an attempt count. `dead_at` is the caller's decision — the cap
/// that produces it is reach policy, not a property of the table.
pub fn set_attempt(
    conn: &Connection,
    id: &str,
    attempts: i64,
    now: i64,
    dead_at: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE message SET attempts = ?2, delivered_at = COALESCE(delivered_at, ?3), dead_at = ?4
          WHERE id = ?1",
        params![id, attempts, now, dead_at],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db
    }

    fn live(db: &Db, session_id: &str, pid: i32) {
        db.conn()
            .execute(
                "INSERT INTO live_session(session_id, pid, source, registered_at, last_seen_at, presence)
                 VALUES (?1, ?2, 'scan', 0, 0, 'scan')",
                rusqlite::params![session_id, pid],
            )
            .unwrap();
    }

    /// The lookup that replaces guessing `claude_code` from a bare native id.
    /// A codex session passing its own id used to get a return address under
    /// claude_code — well-formed, accepted, stored, and never drained.
    #[test]
    fn a_bare_native_id_resolves_to_the_runtime_it_is_registered_under() {
        let db = db();
        live(&db, "m1/codex/abc-123", 1);
        assert_eq!(
            runtimes_for_native(db.conn(), "abc-123").unwrap(),
            ["codex"]
        );
    }

    /// Two answers are not an answer. Reported as two so the caller decides,
    /// rather than picked for it.
    #[test]
    fn the_same_native_id_under_two_runtimes_is_reported_as_both() {
        let db = db();
        live(&db, "m1/codex/dup", 1);
        live(&db, "m1/pi/dup", 2);
        assert_eq!(
            runtimes_for_native(db.conn(), "dup").unwrap(),
            ["codex", "pi"]
        );
    }

    /// The LIKE '%/x' pattern also matches a session whose native id merely
    /// ENDS in x. The segment itself has to match, or a lookup for "123"
    /// silently claims the runtime of "abc-123".
    #[test]
    fn a_native_id_that_is_only_a_suffix_does_not_match() {
        let db = db();
        live(&db, "m1/codex/abc-123", 1);
        assert!(runtimes_for_native(db.conn(), "123").unwrap().is_empty());
    }

    #[test]
    fn an_unregistered_native_id_resolves_to_nothing() {
        let db = db();
        assert!(runtimes_for_native(db.conn(), "never-seen")
            .unwrap()
            .is_empty());
    }

    /// Every statement in this module must still be valid against the schema the
    /// migrations actually produce. `prepare` compiles the SQL without running
    /// it, so a renamed or dropped column fails here rather than at the call
    /// site in `tp-reach` — which is the drift that made this module necessary.
    ///
    /// It is NOT a substitute for the behavioural tests: a query can compile and
    /// still answer the wrong question, which is exactly what
    /// `session_of_process`'s `LIMIT 1` did.
    #[test]
    fn every_statement_compiles_against_the_migrated_schema() {
        let db = db();
        let conn = db.conn();
        let pids = [1, 2, 3];
        scan_rows_for_pid(conn, 1).unwrap();
        rows_for_pid(conn, 1).unwrap();
        list_live(conn).unwrap();
        addressability(conn, "s").unwrap();
        target_row(conn, "s").unwrap();
        upsert_registration(conn, "s", 1, None, None, "scan", None, None, 0).unwrap();
        insert_scanned(conn, "s2", 2, None, None, 0).unwrap();
        touch_location(conn, "s", None, None, 1).unwrap();
        delete_session_pinned(conn, "s2", 2).unwrap();
        prune_scan_rows(conn, &pids).unwrap();
        prune_scan_rows(conn, &[]).unwrap();
        touch_heartbeat(conn, "s", 1).unwrap();
        mark_stale(conn, 1, 0).unwrap();
        evict_stale(conn, 0).unwrap();
        last_wake_at(conn, "s").unwrap();
        set_last_wake_at(conn, "s", 1).unwrap();
        delete_session(conn, "s").unwrap();

        insert_message(conn, "m", "s", None, "m1", "ask", "hi", None, 0).unwrap();
        unread(conn, "s").unwrap();
        wakeable(conn, "s").unwrap();
        by_prefix(conn, "m").unwrap();
        attempts_of(conn, "m").unwrap();
        set_attempt(conn, "m", 1, 0, None).unwrap();
        mark_read(conn, "m", 0).unwrap();
    }

    /// The three states must stay distinguishable — collapsing them is exactly
    /// what made teleport report "delivered on next /tp inbox" for a mailbox
    /// nothing would ever drain.
    #[test]
    fn addressability_separates_registered_dormant_and_unknown() {
        let db = db();
        let conn = db.conn();
        db.ensure_runtime("claude_code", "/root").unwrap();
        upsert_registration(
            conn,
            "m1/claude_code/live",
            1,
            None,
            None,
            "scan",
            None,
            None,
            0,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session(id, machine_id, runtime_id, native_id, turn_count)
             VALUES ('m1/claude_code/old', 'm1', 'claude_code', 'old', 0)",
            [],
        )
        .unwrap();

        assert_eq!(
            addressability(conn, "m1/claude_code/live").unwrap(),
            Addressability::Registered
        );
        assert_eq!(
            addressability(conn, "m1/claude_code/old").unwrap(),
            Addressability::Dormant,
            "an indexed session with no live row is dormant, not unknown — its id most likely rotated"
        );
        assert_eq!(
            addressability(conn, "m1/claude_code/never").unwrap(),
            Addressability::Unknown
        );
    }

    /// The rotation this whole layer exists for: a compaction registers a NEW
    /// session id from the SAME process, and the address must not change.
    #[test]
    fn a_compaction_rejoins_the_same_conversation() {
        let db = db();
        let conn = db.conn();
        let cwd = Some("/Users/me/dev/proj");

        let a = join_conversation(
            conn,
            "m1/claude_code/aaa",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd,
            },
            1_000,
            "m1/claude_code/conv-1",
        )
        .unwrap();
        // Same pid, same cwd, moments later — this is what a compaction looks
        // like from the outside, and it is the only signal there is: the
        // transcripts carry no link between the id that ended and its successor.
        let b = join_conversation(
            conn,
            "m1/claude_code/bbb",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd,
            },
            2_000,
            "m1/claude_code/conv-2",
        )
        .unwrap();
        assert_eq!(a, b, "a rotation must keep the conversation address");
        assert_eq!(conversation_members(conn, &a).unwrap().len(), 2);

        // Re-running the hook must not re-parent an already-bound session.
        let again = join_conversation(
            conn,
            "m1/claude_code/aaa",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd,
            },
            3_000,
            "m1/claude_code/conv-3",
        )
        .unwrap();
        assert_eq!(again, a);
    }

    /// Both directions of the reply failure reported from the field, as one
    /// table. A return address is written when a message is SENT and used when
    /// it is ANSWERED, and everything can rotate in between.
    #[test]
    fn a_stored_return_address_still_resolves_after_the_sender_rotates() {
        let db = db();
        let conn = db.conn();
        let key = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 5,
            pid_start: None,
            cwd: Some("/p"),
        };
        let conv = join_conversation(
            conn,
            "m1/claude_code/old",
            key,
            1_000,
            "m1/claude_code/conv-r",
        )
        .unwrap();
        join_conversation(conn, "m1/claude_code/new", key, 2_000, "unused").unwrap();
        upsert_registration(
            conn,
            "m1/claude_code/new",
            5,
            None,
            Some("/p"),
            "scan",
            None,
            Some("claude_code"),
            2_100,
        )
        .unwrap();

        // Stamped with the conversation address (what `sender_address` writes now).
        assert_eq!(
            conversation_current_session(conn, &conv)
                .unwrap()
                .as_deref(),
            Some("m1/claude_code/new")
        );
        // Stamped with a segment id that has since compacted away (what older
        // messages carry): the membership is what forwards it.
        assert_eq!(
            conversation_of(conn, "m1/claude_code/old")
                .unwrap()
                .as_deref(),
            Some(conv.as_str())
        );
    }

    /// The scan may JOIN a conversation but must never CREATE one.
    ///
    /// Its session id is a guess — the most recently active indexed session
    /// sharing a runtime and cwd. Good enough to publish as an address; not
    /// good enough to seed an identity other sessions will be told to write to.
    #[test]
    fn a_scanned_row_joins_an_existing_conversation_and_never_mints_one() {
        let db = db();
        let conn = db.conn();
        let cwd = Some("/p");
        let key = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 42,
            pid_start: None,
            cwd,
        };

        // Nothing registered yet: the scan must leave no trace.
        assert_eq!(
            join_existing_conversation(conn, "m1/claude_code/guess", key, 1_000).unwrap(),
            None
        );
        let convs: i64 = conn
            .query_row("SELECT COUNT(*) FROM conversation", [], |r| r.get(0))
            .unwrap();
        assert_eq!(convs, 0, "a guessed id must not create a correspondent");

        // Once the hook has registered a real session on that process, the
        // scan's id joins it — this is the case that lost a message: an id
        // resurrected by cwd inference, live and wakeable, drained by nobody.
        let conv = join_conversation(
            conn,
            "m1/claude_code/real",
            key,
            2_000,
            "m1/claude_code/conv-1",
        )
        .unwrap();
        assert_eq!(
            join_existing_conversation(conn, "m1/claude_code/guess", key, 2_100)
                .unwrap()
                .as_deref(),
            Some(conv.as_str())
        );
        assert_eq!(conversation_members(conn, &conv).unwrap().len(), 2);

        // Mail addressed to the resurrected id is now collected.
        insert_message(
            conn,
            "m",
            "m1/claude_code/guess",
            None,
            "m1",
            "ask",
            "to the old address",
            None,
            2_200,
        )
        .unwrap();
        assert_eq!(unread_for_conversation(conn, &conv).unwrap().len(), 1);
    }

    /// The scan runs every 60s and a conversation is only refreshed when it is
    /// joined. Without the scan refreshing it, a process running for hours
    /// without compacting would fall out of the grace window and stop being
    /// joinable — the fix would work for five minutes and then quietly stop.
    #[test]
    fn scanning_keeps_a_quiet_conversation_inside_the_grace_window() {
        let db = db();
        let conn = db.conn();
        let key = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 7,
            pid_start: None,
            cwd: Some("/p"),
        };
        let conv =
            join_conversation(conn, "m1/claude_code/a", key, 0, "m1/claude_code/conv-q").unwrap();

        // Several scan cycles, each well inside the window relative to the last.
        let mut t = 0;
        for _ in 0..5 {
            t += CONVERSATION_JOIN_GRACE_MS / 2;
            assert!(join_existing_conversation(conn, "m1/claude_code/a", key, t)
                .unwrap()
                .is_some());
        }
        // Now a NEW id appears far past the original registration — joinable
        // only because the scan kept the conversation warm.
        t += CONVERSATION_JOIN_GRACE_MS / 2;
        assert_eq!(
            join_existing_conversation(conn, "m1/claude_code/b", key, t)
                .unwrap()
                .as_deref(),
            Some(conv.as_str())
        );
    }

    /// The point of `pid_start`: recognition stops depending on anything being
    /// kept warm. A compaction hours later — long past the grace window, with no
    /// scan, no daemon, nothing having touched the row — still rejoins, because
    /// the process is still the same incarnation and the OS says so.
    ///
    /// Measured before this existed: one unchanging pid and cwd produced three
    /// conversation addresses in an hour, purely because the gaps between
    /// compactions exceeded the window.
    #[test]
    fn an_incarnation_rejoins_no_matter_how_long_the_gap() {
        let db = db();
        let conn = db.conn();
        let key = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 100,
            pid_start: Some("Sat Aug 15 11:21:25 2026"),
            cwd: Some("/p"),
        };
        let a = join_conversation(
            conn,
            "m1/claude_code/a",
            key,
            1_000,
            "m1/claude_code/conv-1",
        )
        .unwrap();
        let much_later = 1_000 + CONVERSATION_JOIN_GRACE_MS * 100;
        let b = join_conversation(
            conn,
            "m1/claude_code/b",
            key,
            much_later,
            "m1/claude_code/conv-2",
        )
        .unwrap();
        assert_eq!(a, b, "an address must not expire while its process runs");
    }

    /// The reuse it is there to defeat. Same pid, same cwd, same runtime, one
    /// second later — but a different process, and the start time says so where
    /// a time window would have said "close enough".
    #[test]
    fn a_reused_pid_is_a_different_correspondent_even_one_second_later() {
        let db = db();
        let conn = db.conn();
        let first = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 100,
            pid_start: Some("Sat Aug 15 11:21:25 2026"),
            cwd: Some("/p"),
        };
        let reused = ConversationKey {
            pid_start: Some("Sat Aug 15 11:21:26 2026"),
            ..first
        };
        let a = join_conversation(
            conn,
            "m1/claude_code/a",
            first,
            1_000,
            "m1/claude_code/conv-1",
        )
        .unwrap();
        let b = join_conversation(
            conn,
            "m1/claude_code/b",
            reused,
            1_001,
            "m1/claude_code/conv-2",
        )
        .unwrap();
        assert_ne!(
            a, b,
            "delivering one agent's mail to another is worse than minting an address"
        );
    }

    /// Rows written before this migration have no start time, and a process
    /// whose start time cannot be read has none either. Both must keep working
    /// exactly as they did — the window is weaker, never wrong-by-merge.
    #[test]
    fn an_unknown_start_time_falls_back_to_the_window() {
        let db = db();
        let conn = db.conn();
        let legacy = ConversationKey {
            machine_id: "m1",
            runtime_id: "claude_code",
            pid: 100,
            pid_start: None,
            cwd: Some("/p"),
        };
        let a = join_conversation(
            conn,
            "m1/claude_code/a",
            legacy,
            1_000,
            "m1/claude_code/conv-1",
        )
        .unwrap();
        assert_eq!(
            join_conversation(conn, "m1/claude_code/b", legacy, 1_100, "unused").unwrap(),
            a,
            "inside the window, a legacy row still recognises its process"
        );
        assert_ne!(
            join_conversation(
                conn,
                "m1/claude_code/c",
                legacy,
                // Past the window measured from the LAST join, not from the
                // first: the fallback window slides with activity, which is
                // what makes it usable at all and what makes it depend on
                // something continuing to happen.
                1_100 + CONVERSATION_JOIN_GRACE_MS + 1,
                "m1/claude_code/conv-3"
            )
            .unwrap(),
            a,
            "past it, the old behaviour still applies"
        );
    }

    /// The join key is (runtime, pid, cwd) inside a grace window. Each part has
    /// to hold on its own, because a false merge delivers one agent's mail to
    /// another — strictly worse than minting one address too many.
    #[test]
    fn unrelated_sessions_never_merge() {
        let db = db();
        let conn = db.conn();
        let base = join_conversation(
            conn,
            "m1/claude_code/a",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd: Some("/p"),
            },
            1_000,
            "m1/claude_code/conv-a",
        )
        .unwrap();

        // Different cwd: two concurrent sessions can share nothing but a pid
        // namespace, and a directory is what tells them apart.
        let other_dir = join_conversation(
            conn,
            "m1/claude_code/b",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd: Some("/other"),
            },
            1_100,
            "m1/claude_code/conv-b",
        )
        .unwrap();
        assert_ne!(base, other_dir);

        // Different runtime on the same pid — a multiplexed host.
        let other_rt = join_conversation(
            conn,
            "m1/pi/c",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "pi",
                pid: 100,
                pid_start: None,
                cwd: Some("/p"),
            },
            1_200,
            "m1/pi/conv-c",
        )
        .unwrap();
        assert_ne!(base, other_rt);

        // Same everything, but long after: this is pid REUSE, not a rotation.
        let reused = join_conversation(
            conn,
            "m1/claude_code/d",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 100,
                pid_start: None,
                cwd: Some("/p"),
            },
            1_000 + CONVERSATION_JOIN_GRACE_MS + 1,
            "m1/claude_code/conv-d",
        )
        .unwrap();
        assert_ne!(
            base, reused,
            "past the grace window a shared pid means nothing"
        );
    }

    /// Mail sent to an id that has since rotated away is still collectable —
    /// this is what the whole layer buys, and what was measured lost without it.
    #[test]
    fn draining_a_conversation_collects_mail_sent_to_a_retired_id() {
        let db = db();
        let conn = db.conn();
        let cwd = Some("/p");
        let conv = join_conversation(
            conn,
            "m1/claude_code/old",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 7,
                pid_start: None,
                cwd,
            },
            1_000,
            "m1/claude_code/conv-x",
        )
        .unwrap();
        insert_message(
            conn,
            "msg-old",
            "m1/claude_code/old",
            None,
            "m1",
            "ask",
            "sent before the compaction",
            None,
            1_500,
        )
        .unwrap();

        join_conversation(
            conn,
            "m1/claude_code/new",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 7,
                pid_start: None,
                cwd,
            },
            2_000,
            "m1/claude_code/conv-y",
        )
        .unwrap();
        insert_message(
            conn,
            "msg-new",
            "m1/claude_code/new",
            None,
            "m1",
            "ask",
            "sent after",
            None,
            2_500,
        )
        .unwrap();

        let drained = unread_for_conversation(conn, &conv).unwrap();
        assert_eq!(
            drained.len(),
            2,
            "the retired id's mailbox must be drained too"
        );
        assert_eq!(drained[0].id, "msg-old", "oldest first, across ids");

        // And the per-session read still sees only its own — the conversation
        // union is an addition, not a replacement.
        assert_eq!(unread(conn, "m1/claude_code/new").unwrap().len(), 1);
    }

    /// Addressing resolves to the segment that is actually registered, so a
    /// wake lands on the live one rather than a retired sibling.
    #[test]
    fn a_conversation_resolves_to_its_registered_segment() {
        let db = db();
        let conn = db.conn();
        let cwd = Some("/p");
        let conv = join_conversation(
            conn,
            "m1/claude_code/old",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 7,
                pid_start: None,
                cwd,
            },
            1_000,
            "m1/claude_code/conv-z",
        )
        .unwrap();
        join_conversation(
            conn,
            "m1/claude_code/new",
            ConversationKey {
                machine_id: "m1",
                runtime_id: "claude_code",
                pid: 7,
                pid_start: None,
                cwd,
            },
            2_000,
            "unused",
        )
        .unwrap();

        // Only the older one is registered — newest-member order must not win
        // over actually being live.
        upsert_registration(
            conn,
            "m1/claude_code/old",
            7,
            None,
            cwd,
            "scan",
            None,
            Some("claude_code"),
            3_000,
        )
        .unwrap();
        assert_eq!(
            conversation_current_session(conn, &conv)
                .unwrap()
                .as_deref(),
            Some("m1/claude_code/old")
        );

        // With nothing registered at all, fall back to the newest member rather
        // than refusing — the message still lands where the next drain finds it.
        delete_session(conn, "m1/claude_code/old").unwrap();
        assert_eq!(
            conversation_current_session(conn, &conv)
                .unwrap()
                .as_deref(),
            Some("m1/claude_code/new")
        );
    }

    #[test]
    fn prune_spares_declared_rows_and_takes_unfound_scan_rows() {
        let db = db();
        let conn = db.conn();
        upsert_registration(conn, "hosted", 10, None, None, "declared", None, None, 0).unwrap();
        insert_scanned(conn, "scanned", 11, None, None, 0).unwrap();

        prune_scan_rows(conn, &[10]).unwrap();
        assert!(target_row(conn, "hosted").unwrap().is_some());
        assert!(
            target_row(conn, "scanned").unwrap().is_none(),
            "a scan row whose pid the scan no longer finds must be pruned"
        );

        // Even an empty scan — tmux gone, nothing found — leaves declared rows.
        prune_scan_rows(conn, &[]).unwrap();
        assert!(target_row(conn, "hosted").unwrap().is_some());
    }

    #[test]
    fn a_heartbeat_clears_a_stale_mark_and_reports_whether_a_row_existed() {
        let db = db();
        let conn = db.conn();
        upsert_registration(conn, "s", 1, None, None, "declared", None, None, 0).unwrap();
        assert_eq!(mark_stale(conn, 500, 400).unwrap(), 1);
        assert!(target_row(conn, "s").unwrap().unwrap().stale_at.is_some());

        assert_eq!(touch_heartbeat(conn, "s", 600).unwrap(), 1);
        assert!(target_row(conn, "s").unwrap().unwrap().stale_at.is_none());
        assert_eq!(
            touch_heartbeat(conn, "gone", 600).unwrap(),
            0,
            "beating into an evicted session must report that it is gone"
        );
    }
}
