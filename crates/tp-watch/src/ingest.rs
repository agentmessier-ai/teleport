//! The shared ingest path — used by the one-shot `tp index` AND the continuous
//! watcher. One implementation, two triggers (LLD §6.2).

use anyhow::Result;
use std::path::Path;
use tp_core::SessionId;
use tp_db::{writer, Db};
use tp_ingest::adapter::{Adapter, SourceFile};

/// Ingest one file from its inode checkpoint. Returns the number of new turns
/// written (0 = no-op: nothing new, or only a torn trailing line).
pub fn ingest_file(
    db: &mut Db,
    machine_id: &str,
    adapter: &dyn Adapter,
    src: &SourceFile,
) -> Result<usize> {
    let checkpoint = writer::get_checkpoint(db.conn(), src.inode)?;
    let offset = checkpoint
        .as_ref()
        .map(|c| c.byte_offset as u64)
        .unwrap_or(0);
    if checkpoint.is_some() && offset >= src.size {
        return Ok(0); // nothing new since the last checkpoint
    }

    let mut chunk = adapter.parse_from(&src.path, offset)?;
    if chunk.turns.is_empty() && chunk.new_offset == offset {
        return Ok(0); // torn trailing line only — wait for the next scan
    }
    for turn in &mut chunk.turns {
        tp_ingest::redact::redact(turn);
    }

    let session_id = SessionId::new(machine_id, adapter.id(), &src.native_id).to_string();
    writer::commit_chunk(
        db.conn_mut(),
        &session_id,
        machine_id,
        adapter.id(),
        &src.native_id,
        src.path.to_str().unwrap_or_default(),
        src.inode,
        src.mtime_ms,
        &chunk,
    )
}

/// Full sweep of one root: discover every file, ingest each from its
/// checkpoint. Returns (files_ingested, turns_written).
///
/// One file's failure is ISOLATED. It used to propagate: a single unreadable
/// transcript aborted the sweep, every file after it in discovery order was
/// skipped, and — because the error travelled all the way out through
/// `reconcile` and `run` — the watcher thread exited while `tpd` stayed alive.
/// launchd never restarted it, because the process had not died. The operator
/// got one line on stderr and an index that silently stopped growing.
///
/// That is the failure this project keeps meeting from the other side: a
/// failure rendered as nothing having happened. A corpus of 27,000 files
/// written by four other programs will contain a bad one eventually, and the
/// right response is to skip it loudly and keep indexing the other 26,999.
/// What one pass over a root actually did, INCLUDING what it could not do.
///
/// The failures are returned rather than only logged, because the three callers
/// need opposite things from the same function:
///
///   `tp index`     continue — one bad file must not stop 28,000 good ones
///   `tpd` watcher  continue — the next sweep retries
///   `tp reindex`   STOP — the rows were already deleted, and a refill that
///                  fails is data loss reported as success
///
/// Only the last one is a correctness requirement, and it is the one the old
/// signature could not express: `(usize, usize)` has nowhere to put a failure,
/// so every caller got "continue" whether that was right for it or not.
///
/// Measured, on 2026-08-21: a reindex cleared 28,182 sessions, one 44 MB
/// transcript hit `database is locked` against a running daemon, the failure
/// became a log line, and the command printed its usual summary and exited 0
/// having destroyed 10,836 turns.
#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub files: usize,
    pub turns: usize,
    /// Path and reason, one per file that could not be read. Empty is the
    /// normal case and the only one a delete-then-refill caller may proceed on.
    pub failed: Vec<(std::path::PathBuf, String)>,
}

impl ScanOutcome {
    /// One line naming what failed, or `None` when nothing did. Callers that
    /// continue still owe the user this — a warning nobody surfaces is the
    /// state that produced the incident above.
    pub fn failure_note(&self) -> Option<String> {
        if self.failed.is_empty() {
            return None;
        }
        let shown: Vec<String> = self
            .failed
            .iter()
            .take(3)
            .map(|(p, e)| format!("{}: {e}", p.display()))
            .collect();
        let more = self.failed.len().saturating_sub(shown.len());
        Some(format!(
            "{} file(s) could not be read — {}{}",
            self.failed.len(),
            shown.join("; "),
            if more > 0 {
                format!("; and {more} more")
            } else {
                String::new()
            }
        ))
    }
}

pub fn scan_root(
    db: &mut Db,
    machine_id: &str,
    adapter: &dyn Adapter,
    root: &Path,
) -> Result<ScanOutcome> {
    let sources = adapter.discover(root)?;
    let mut out = ScanOutcome::default();
    for src in &sources {
        match ingest_file(db, machine_id, adapter, src) {
            Ok(n) => {
                if n > 0 {
                    out.files += 1;
                    out.turns += n;
                }
            }
            // Collected AND logged. The log line is for the person tailing
            // tpd.err.log during an incident; the returned value is for the
            // caller that has to decide whether continuing is safe.
            Err(e) => {
                tp_core::log_warn!(
                    "skipping {}: {e:#} (indexing continues)",
                    src.path.display()
                );
                out.failed.push((src.path.clone(), format!("{e:#}")));
            }
        }
    }
    Ok(out)
}
