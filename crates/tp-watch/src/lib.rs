//! Continuous watcher (LLD §6.1, §6.2). FSEvents is the fast path; the
//! periodic reconcile is the correctness net, because FSEvents coalesces and
//! can miss events across volume/mount changes and editor rename dances.
//!
//! Single-writer discipline: events are pushed into a queue, and a single
//! background loop drains it — the same shape the SQLite writer path requires.

pub mod ingest;

use anyhow::Result;
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tp_db::Db;
use tp_ingest::adapter::Adapter;

const DEBOUNCE: Duration = Duration::from_millis(250);
const RECONCILE_EVERY: Duration = Duration::from_secs(30);

/// A runtime root together with its adapter and the machine id that prefixes
/// every session id it produces.
pub struct WatchRoot {
    pub runtime_id: String,
    pub root: PathBuf,
    pub adapter: Box<dyn Adapter>,
}

pub struct Watcher {
    roots: Vec<WatchRoot>,
    machine_id: String,
    db: Mutex<Db>,
    /// Parallel to `roots`: each root resolved through symlinks, because
    /// FSEvents reports canonical paths (see `ingest_dirty`).
    canonical_roots: Vec<PathBuf>,
    /// Shared with the FSEvents callback — see `new`.
    events: Arc<Mutex<HashSet<PathBuf>>>,
    _notify: RecommendedWatcher,
}

impl Watcher {
    pub fn new(machine_id: impl Into<String>, db: Db, roots: Vec<WatchRoot>) -> Result<Self> {
        // Events are cheap root-dirty signals; paths are re-scanned from the
        // roots on each batch, so we don't need to track which path fired.
        // The callback and `run_iter` MUST share this one set — storing a
        // separate set on the struct silently kills the fast path and leaks.
        let events: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
        let events_clone = Arc::clone(&events);

        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res {
                    // Any event under a watched root marks that root dirty, not the
                    // specific file — FSEvents is directory-granular anyway.
                    // `paths` can be empty (kind Other, FSEvents MustScanSubDirs),
                    // so indexing [0] would panic on the notify worker thread.
                    if let Some(p) = ev.paths.first() {
                        if let Ok(mut set) = events_clone.lock() {
                            set.insert(p.clone());
                        }
                    }
                }
            })?;

        for root in &roots {
            watcher.watch(&root.root, RecursiveMode::Recursive)?;
        }

        let canonical_roots = roots
            .iter()
            .map(|r| r.root.canonicalize().unwrap_or_else(|_| r.root.clone()))
            .collect();

        Ok(Self {
            roots,
            canonical_roots,
            machine_id: machine_id.into(),
            db: Mutex::new(db),
            events,
            _notify: watcher,
        })
    }

    /// Drain pending events after debounce, then reconcile every 30s.
    /// The watcher loop. It does NOT stop on a failed iteration.
    ///
    /// It used to: `run_iter(..)?` ended the loop, `run` returned the error, and
    /// `tpd` logged "watcher stopped" and let the thread die while the process
    /// stayed alive — so launchd never restarted anything and the index quietly
    /// stopped growing. That happened on this machine nine times, every one of
    /// them `database is locked` from another `tp` process writing at the same
    /// moment: a transient condition that ended indexing permanently.
    ///
    /// The promotion race behind those is fixed at the source (writer.rs takes
    /// its write lock at BEGIN), but a busy timeout can still expire under real
    /// contention, and a watcher that dies on one is a watcher that lies about
    /// what it has seen. A failed iteration is reported and retried.
    pub fn run(&self) -> Result<()> {
        let mut last_reconcile = Instant::now();
        let mut consecutive = 0u32;
        loop {
            std::thread::sleep(DEBOUNCE);
            match self.run_iter(&mut last_reconcile) {
                Ok(()) => consecutive = 0,
                Err(e) => {
                    consecutive += 1;
                    // Named every time. A count alone would turn a persistent
                    // fault into background noise, which is how a stale index
                    // goes unnoticed.
                    tp_core::log_warn!(
                        "watcher iteration failed ({consecutive}): {e:#} — retrying"
                    );
                    // Back off so a genuinely stuck database is not hammered,
                    // bounded so recovery is still prompt when it clears.
                    std::thread::sleep(std::cmp::min(
                        DEBOUNCE * consecutive.min(30),
                        std::time::Duration::from_secs(30),
                    ));
                }
            }
        }
    }

    /// Expose the DB for tests / inspection.
    pub fn db(&self) -> &Mutex<Db> {
        &self.db
    }

    /// One loop iteration — separated so tests can drive it without a thread.
    pub fn run_iter(&self, last_reconcile: &mut Instant) -> Result<()> {
        let dirty = {
            let mut set = self.events.lock().unwrap();
            std::mem::take(&mut *set)
        };
        if !dirty.is_empty() {
            self.ingest_dirty(&dirty)?;
        }
        if last_reconcile.elapsed() >= RECONCILE_EVERY {
            self.reconcile()?;
            *last_reconcile = Instant::now();
        }
        Ok(())
    }

    /// A dirty event means "look at this root again" — cheapest correct
    /// response is to re-scan the affected root(s) from their checkpoints.
    fn ingest_dirty(&self, paths: &HashSet<PathBuf>) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        for (i, root) in self.roots.iter().enumerate() {
            // Match against the CANONICAL root: macOS reports FSEvents paths
            // resolved through symlinks (/var → /private/var), so a raw
            // `starts_with(root)` silently never matches and the fast path dies.
            let canon = &self.canonical_roots[i];
            if paths
                .iter()
                .any(|p| p.starts_with(canon) || p.starts_with(&root.root))
            {
                let _ = ingest::scan_root(
                    &mut db,
                    &self.machine_id,
                    root.adapter.as_ref(),
                    &root.root,
                )?;
            }
        }
        Ok(())
    }

    /// Full sweep of every root — the correctness net for events FSEvents
    /// never delivered. Inode-keyed checkpoints make this cheap: unchanged
    /// files are skipped by a stat, not a re-parse.
    fn reconcile(&self) -> Result<()> {
        let mut db = self.db.lock().unwrap();
        for root in &self.roots {
            let _ =
                ingest::scan_root(&mut db, &self.machine_id, root.adapter.as_ref(), &root.root)?;
        }
        Ok(())
    }
}

/// Every runtime's watch root, built from the same single registry the CLI and
/// the daemon read (`tp_ingest::adapter::all_*`) so none of the three can drift
/// on which runtimes exist.
///
/// A root that doesn't exist on this machine is dropped rather than watched:
/// most installs have Claude Code but not pi (or vice versa), and `notify`
/// would otherwise fail to register a watch on a missing directory.
pub fn all_roots() -> Vec<WatchRoot> {
    tp_ingest::adapter::all_adapters()
        .into_iter()
        .zip(tp_ingest::adapter::all_roots())
        .filter(|(_, (_, root))| root.exists())
        .map(|(adapter, (runtime_id, root))| WatchRoot {
            runtime_id,
            root,
            adapter,
        })
        .collect()
}
