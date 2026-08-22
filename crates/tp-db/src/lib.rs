pub mod query;
pub mod reach;
pub mod writer;

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub use query::{SearchHit, SessionRow, TurnRow};
pub use writer::IngestCheckpoint;

/// Re-exported so a caller can name the handle `conn()` hands out without taking
/// a direct rusqlite dependency — which is what lets `tp-reach` keep rusqlite in
/// `[dev-dependencies]` and be unable to hand-write SQL in production code.
pub use rusqlite::Connection as DbConnection;

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("../migrations/0001_init.sql")),
    ("0002_reach", include_str!("../migrations/0002_reach.sql")),
    ("0003_scan", include_str!("../migrations/0003_scan.sql")),
    ("0004_panel", include_str!("../migrations/0004_panel.sql")),
    (
        "0005_provenance",
        include_str!("../migrations/0005_provenance.sql"),
    ),
    (
        "0006_presence",
        include_str!("../migrations/0006_presence.sql"),
    ),
    (
        "0007_conversation",
        include_str!("../migrations/0007_conversation.sql"),
    ),
    (
        "0008_conversation_pid_start",
        include_str!("../migrations/0008_conversation_pid_start.sql"),
    ),
    ("0009_ack", include_str!("../migrations/0009_ack.sql")),
    (
        "0010_drop_dismissed_states",
        include_str!("../migrations/0010_drop_dismissed_states.sql"),
    ),
    (
        "0011_daemon_status",
        include_str!("../migrations/0011_daemon_status.sql"),
    ),
    (
        "0012_surface_and_title_provenance",
        include_str!("../migrations/0012_surface_and_title_provenance.sql"),
    ),
    (
        "0013_backup_status",
        include_str!("../migrations/0013_backup_status.sql"),
    ),
];

/// Where teleport keeps its state.
///
/// Lived in BOTH `tp/src/main.rs` and `tp/src/bin/tpd.rs`, byte-identical —
/// two binaries each deciding independently where the database is. Found by an
/// Entrography hash-equality scan, which is the kind of duplication that costs
/// nothing until the day one side is changed and the two quietly disagree about
/// which file they are opening.
///
/// It belongs here because this crate owns the database: everything that opens
/// one already depends on it.
pub fn teleport_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    std::path::PathBuf::from(home).join(".teleport")
}

pub fn default_db_path() -> std::path::PathBuf {
    // `TP_DB` points every command at another index. It exists so an archive
    // (`tp archive`) is readable with the tools that already exist rather than
    // needing its own query surface.
    if let Ok(p) = std::env::var("TP_DB") {
        return std::path::PathBuf::from(p);
    }
    daemon_db_path()
}

/// The index the DAEMON writes — `TP_DB` deliberately does not move it.
///
/// The difference matters to anything that refuses to run while tpd is live:
/// pointed at an archive or a copy, there is no race to avoid, and a guard that
/// could not tell the two apart refused work it had no reason to.
pub fn daemon_db_path() -> std::path::PathBuf {
    teleport_dir().join("teleport.db")
}

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        // LLD §3: WAL for concurrent readers + one writer.
        //
        // `synchronous = NORMAL` was justified as "we can always replay from the
        // source JSONL". That premise is FALSE for a third of this machine's
        // corpus — Claude Code deletes transcripts after ~30 days, and 15,178 of
        // 43,715 sessions (140,254 turns) now exist only here. The setting stays
        // anyway, on a narrower and true argument: under WAL, NORMAL risks losing
        // the LAST transactions on a power loss, never corruption, and the
        // irreplaceable rows are old ones committed and checkpointed long before
        // their file aged out. What NORMAL does not cover — disk failure, a
        // corrupted file, an accidental delete — was never covered by the replay
        // premise either. `tp backup` and `tp verify` are the answer to that; see
        // their doc comments for the stake.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    /// In-memory DB for tests — same schema/pragma path as `open`, minus WAL
    /// (which requires a real file).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)",
        )?;
        for (name, sql) in MIGRATIONS {
            let already: bool = self
                .conn
                .query_row(
                    "SELECT 1 FROM schema_migration WHERE name = ?1",
                    [name],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if already {
                continue;
            }
            // IMMEDIATE for the same reason as the writer: a migration is a
            // write, and a deferred transaction that promotes can lose the race
            // to another connection with SQLITE_BUSY_SNAPSHOT, which
            // busy_timeout does not retry.
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO schema_migration(name, applied_at) VALUES (?1, unixepoch())",
                [name],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Register (or refresh) this machine's own `machine` row with `trust='self'`.
    pub fn ensure_self_machine(&self, machine_id: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO machine(id, name, is_self, trust, created_at)
             VALUES (?1, ?2, 1, 'self', unixepoch())
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            rusqlite::params![machine_id, name],
        )?;
        Ok(())
    }

    /// Record which build of the daemon is now serving, and from which pid.
    ///
    /// Called by `tpd` at startup. Overwrites rather than appends: the
    /// question this answers is "what is running right now", and a previous
    /// run's row is not an answer to it (see migration 0011).
    /// Record that a backup was taken. One row, overwritten each time — see
    /// migration 0013 for why the history is not kept and what a restored
    /// snapshot reports.
    ///
    /// Written by `tp backup` AFTER the copy lands, so a failed `VACUUM INTO`
    /// never leaves a claim that a backup exists.
    pub fn record_backup(&self, dest: &str, turn_count: i64, bytes: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO backup_status(id, taken_at, dest, turn_count, bytes)
             VALUES (1, ?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 taken_at = excluded.taken_at,
                 dest = excluded.dest,
                 turn_count = excluded.turn_count,
                 bytes = excluded.bytes",
            rusqlite::params![tp_core::now_ms(), dest, turn_count, bytes as i64],
        )?;
        Ok(())
    }

    pub fn record_daemon_start(&self, version: &str, pid: u32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon_status(id, version, pid, started_at)
             VALUES (1, ?1, ?2, unixepoch())
             ON CONFLICT(id) DO UPDATE SET
                 version = excluded.version,
                 pid = excluded.pid,
                 started_at = excluded.started_at",
            rusqlite::params![version, pid],
        )?;
        Ok(())
    }

    pub fn ensure_runtime(&self, id: &str, root: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runtime(id, root) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET root = excluded.root",
            rusqlite::params![id, root],
        )?;
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}
