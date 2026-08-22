#![allow(clippy::print_stdout, clippy::print_stderr)]
//! Exempt from the workspace print lints: this module's OUTPUT IS THE
//! PRODUCT. `tp` is a CLI; a subcommand that stopped writing to stdout would
//! have no function. Narrow and permanent — see [workspace.lints.clippy] in
//! the root Cargo.toml for why the lint exists everywhere else.

//! Network and identity commands: id, live, peers, discover, version, pairing.

use crate::{db_path, fmt_ts, hostname, identity};
use anyhow::{Context as _, Result};
use tp_db::Db;

/// The port this machine serves on — must match what `tpd` binds, or a peer
/// will record an address that nothing answers.
pub(crate) fn serve_port() -> u16 {
    std::env::var("TP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(tp_net::DEFAULT_PORT)
}

pub(crate) fn run_id() -> Result<()> {
    let id = identity()?;
    println!("device id : {}", id.device_id);
    println!("name      : {}", hostname());
    println!("port      : {}", serve_port());
    println!("\nCompare the device id out of band before approving on either side.");
    Ok(())
}

pub(crate) fn fmt_secs(ts: Option<i64>) -> String {
    // machine.last_seen_at / paired_at are unix SECONDS, unlike turn.ts.
    match ts {
        Some(s) => chrono::DateTime::from_timestamp(s, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".into()),
        None => "-".into(),
    }
}

pub(crate) fn run_live() -> Result<()> {
    let db = Db::open(&db_path())?;
    let rows = tp_app::live(&db)?;

    if rows.is_empty() {
        println!("no live sessions known — either none are running, or `tpd` hasn't completed its first scan cycle yet (every {}s)", tp_reach::SCAN_INTERVAL_SECS);
        return Ok(());
    }
    for row in &rows {
        // 'hook' rows carry a REAL session_id from Claude Code itself;
        // 'scan' rows are inferred (see discover.rs) and may be a synthetic
        // `scan-pid-N` placeholder if no matching indexed session existed yet.
        println!(
            "{:<6} pid {:<8} tty {:<12} last seen {}",
            row.row.source,
            row.row.pid,
            row.row.tty.as_deref().unwrap_or("(none)"),
            fmt_ts(Some(row.row.last_seen_at))
        );
        // Publish the CONVERSATION address when there is one. This listing is
        // where senders copy an address from, and a segment id copied from here
        // stops being deliverable at the target's next compaction — measured at
        // four ids in one afternoon for a single conversation. The segment id is
        // still printed, because it is what `tp turns` and every stored message
        // are keyed by; it is just no longer the thing to address.
        if row.address_is_stable() {
            println!(
                "       {}   (stable address — survives compaction)",
                row.address
            );
            println!("       {}   (current segment)", row.row.session_id);
        } else {
            println!("       {}", row.address);
        }
        if let Some(cwd) = &row.row.cwd {
            println!("       {cwd}");
        }
    }
    println!("\n({} live session(s))", rows.len());
    Ok(())
}

pub(crate) fn run_peers() -> Result<()> {
    let db = Db::open(&db_path())?;
    let peers = tp_app::peers::peers(&db)?;
    if peers.is_empty() {
        println!(
            "no peers yet — `tp discover` to find them, `tp pair request <host:port>` to introduce"
        );
        return Ok(());
    }
    for p in &peers {
        println!(
            "{:<9} {:<24} {:<22} last seen {}",
            p.trust,
            p.name,
            p.addr.as_deref().unwrap_or("(no address)"),
            fmt_secs(p.last_seen_at)
        );
        println!("          {}", p.id);
    }
    Ok(())
}

pub(crate) fn run_discover(host: &str) -> Result<()> {
    let me = identity()?.device_id;
    let db = Db::open(&db_path())?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let found = rt.block_on(tp_app::discover(&db, &me, host))?;

    for p in &found.peers {
        println!(
            "{:<22} {:<24} {}",
            p.addr,
            p.name,
            if p.known { "known" } else { "new" }
        );
        println!("          {}", p.device_id);
    }
    if found.peers.is_empty() {
        if found.answered > 0 {
            println!("{host} is this machine — nothing to pair with");
        } else {
            println!("no teleport daemon answered on {host}");
            println!(
                "(tried ports {}-{}; give an explicit host:port if it listens elsewhere)",
                tp_net::DEFAULT_PORT,
                tp_net::DEFAULT_PORT + tp_net::PROBE_PORTS - 1
            );
        }
    }
    Ok(())
}

/// Both builds, and whether they agree.
///
/// `tp --version` alone answers the wrong question. The binary on disk is not
/// what serves peer requests — a LaunchAgent keeps running whatever it started
/// with — so "did my upgrade take" is only answerable by comparing the two,
/// and that comparison is the entire reason this command exists.
pub(crate) fn run_version() -> Result<()> {
    println!("tp   {}", tp_core::VERSION_LINE);

    let daemon = Db::open(&db_path())
        .ok()
        .and_then(|db| tp_db::query::daemon_status(db.conn()).ok().flatten());

    let Some(d) = daemon else {
        println!("tpd  not recorded — the daemon has not started since this feature shipped");
        print_descriptor_overrides();
        print_backup_age();
        return Ok(());
    };
    println!(
        "tpd  {}  · pid {} · up {}",
        d.version,
        d.pid,
        fmt_uptime(tp_core::now_ms() / 1000 - d.started_at)
    );

    match tp_core::compare_builds(&d.version, tp_core::VERSION_LINE) {
        tp_core::BuildMatch::Different => {
            println!(
                "\nThe daemon is running different code than this binary.\n\
                 Restart it:  launchctl kickstart -k gui/$(id -u)/io.teleport.tpd"
            );
        }
        // Not silence, and not a warning: saying nothing here would read as
        // "they match", which a dirty tree cannot support.
        tp_core::BuildMatch::Unknown => {
            println!("\n(built from an uncommitted tree — the two cannot be compared)");
        }
        tp_core::BuildMatch::Same => {}
    }
    print_descriptor_overrides();
    print_backup_age();
    Ok(())
}

/// Files in `~/.teleport/runtimes.d/` shadowing a built-in runtime. The binary
/// carries the shipped descriptors, so an override is either a customization
/// (fine, it wins, but worth being able to see) or a stale copy from an install
/// that used to copy them out — the failure that twice made a rebuilt binary run
/// "byte-identical wrong" against text it no longer contained. Content cannot
/// tell those two apart; naming the file and the difference is the whole job.
/// How long since the last backup, and how much has no other copy.
///
/// Here rather than only in `tp verify` because `tp version` is what a person
/// runs without being worried — and the moment worth learning that the index is
/// the last copy of a quarter of its turns is BEFORE something goes wrong, not
/// during. `tp verify` already says it at length; this is the one line that
/// gets seen.
///
/// Silent when nothing is irreplaceable: a machine whose transcripts all still
/// exist can rebuild its index with `tp reindex`, and telling it to back up
/// would be advice it does not need.
fn print_backup_age() {
    let Ok(db) = Db::open(&db_path()) else {
        return;
    };
    let Ok((sessions, turns)) = irreplaceable(&db) else {
        return;
    };
    if sessions == 0 {
        return;
    }

    match tp_db::query::backup_status(db.conn()) {
        Ok(Some(b)) => {
            let days = (tp_core::now_ms() - b.taken_at) / 86_400_000;
            let when = match days {
                0 => "today".to_string(),
                1 => "yesterday".to_string(),
                d => format!("{d} days ago"),
            };
            // The turn delta is the part that makes the age actionable: 40 days
            // is fine on an idle machine and alarming on one that added 100k
            // turns since.
            let now: i64 = db
                .conn()
                .query_row("SELECT count(*) FROM turn", [], |r| r.get(0))
                .unwrap_or(b.turn_count);
            let drift = now - b.turn_count;
            println!(
                "\nbackup  {when} → {} ({} turn(s) since)",
                b.dest,
                if drift > 0 {
                    drift.to_string()
                } else {
                    "0".into()
                }
            );
        }
        // Never backed up is NOT "0 days ago", and it is the case that matters
        // most — this is the state a fresh install stays in until someone acts.
        Ok(None) => {
            println!(
                "\nbackup  NEVER — {turns} turn(s) across {sessions} session(s) exist only here.\n\
                 \ttp backup ~/teleport-backup.db"
            );
        }
        Err(_) => {}
    }
}

pub(crate) fn print_descriptor_overrides() {
    let overrides = tp_ingest::adapter::descriptor_overrides();
    if overrides.is_empty() {
        return;
    }
    println!();
    for o in overrides {
        if o.identical {
            println!(
                "descriptor override {} is byte-identical to this build's embedded {} —                  redundant today, stale the next time the shipped descriptor changes; safe to delete",
                o.path.display(),
                o.id
            );
        } else {
            println!(
                "descriptor override {} DIFFERS from this build's embedded {} and is what actually runs.",
                o.path.display(),
                o.id
            );
            println!("  If you customized it: working as intended.");
            println!(
                "  If you did not: it is a stale copy from an older install — delete it, the binary carries the current one."
            );
        }
    }
}

pub(crate) fn fmt_uptime(secs: i64) -> String {
    match secs {
        s if s < 0 => "?".to_string(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

pub(crate) fn run_pair_request(addr: &str) -> Result<()> {
    let me = identity()?;
    let db = Db::open(&db_path())?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let r = rt.block_on(tp_app::pair::request(
        &db,
        &me,
        addr,
        &hostname(),
        serve_port(),
    ))?;

    println!("peer  : {} ({})", r.name, r.device_id);
    println!("their side: {}", r.their_status);
    if r.my_status == tp_net::pairing::PairingStatus::Trusted {
        println!("\nThis machine ALREADY trusts that peer — nothing to approve here.");
        println!("They still have to approve us on their side if they have not.");
    } else {
        println!("\nNothing is trusted yet. On BOTH machines, compare the device ids above,");
        println!("then run:  tp pair approve <device-id>");
    }
    Ok(())
}

pub(crate) fn run_pair_list() -> Result<()> {
    let db = Db::open(&db_path())?;
    let p = tp_app::pair::pairings(&db)?;
    if p.pending.is_empty() {
        println!("no pending pairings");
    }
    for x in &p.pending {
        let dir = match x.direction {
            tp_app::Direction::TheyAskedUs => "they asked us",
            tp_app::Direction::WeAskedThem => "we asked them",
        };
        println!("{:<24} {:<14} {}", x.name, dir, x.device_id);
    }
    for x in &p.trusted {
        println!("{:<24} {:<14} {}", x.name, "trusted", x.id);
    }
    // The other half of choosing "refuse the newcomer" over "evict the oldest"
    // when the list fills: refusing is only the safer failure if it is a LOUD
    // one. To a peer being turned away this is a 503; on this machine it would
    // otherwise be a request that silently never appeared in this list, which
    // is exactly what an attacker filling it is going for.
    let incoming = p
        .pending
        .iter()
        .filter(|x| matches!(x.direction, tp_app::Direction::TheyAskedUs))
        .count();
    if incoming >= tp_net::pairing::MAX_PENDING_IN {
        println!(
            "\n{incoming} incoming requests — the cap. No new machine can pair until some are\n\
             cleared: tp pair reject <device-id>"
        );
    }
    Ok(())
}

pub(crate) fn run_pair_decide(device_id: &str, accept: bool) -> Result<()> {
    let db = Db::open(&db_path())?;
    match tp_app::pair::decide(&db, device_id, accept)? {
        Some(status) => {
            println!("{device_id} → {status:?}");
            println!("this machine will now answer that peer's signed queries");
        }
        None => println!("{device_id} refused and removed"),
    }
    Ok(())
}

pub(crate) fn run_pair_revoke(device_id: &str) -> Result<()> {
    let db = Db::open(&db_path())?;
    tp_app::pair::revoke(&db, device_id)?;
    println!("{device_id} revoked and removed");
    println!("its next signed request will be refused; it is not notified");
    Ok(())
}

/// How much of the index has no other copy — the number that decides whether
/// losing this file is an inconvenience or a loss.
///
/// A session with a `source_path` that no longer resolves, or none at all (a
/// pushed session, which never had a file), exists only here.
fn irreplaceable(db: &Db) -> anyhow::Result<(i64, i64)> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT id, source_path, turn_count FROM session")?;
    let rows: Vec<(String, Option<String>, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .filter_map(Result::ok)
        .collect();
    let (mut sessions, mut turns) = (0i64, 0i64);
    for (_, path, n) in rows {
        let gone = match &path {
            Some(p) => !std::path::Path::new(p).exists(),
            None => true,
        };
        if gone {
            sessions += 1;
            turns += n;
        }
    }
    Ok((sessions, turns))
}

/// How fast this file is growing, and what that means.
///
/// Printed because the number was invisible: knowing the index is 2.45 GB says
/// nothing without knowing it took three months and the rate is climbing. And
/// the shape below is the reason a plain "delete older than N" policy is the
/// wrong instinct here — the OLD turns are the ones with no other copy (a
/// runtime deletes its transcripts long before teleport would), so age-based
/// deletion destroys the irreplaceable and keeps the redundant.
fn print_growth(db: &Db, size: u64, total: i64) -> Result<()> {
    if total == 0 {
        return Ok(());
    }
    let now = tp_core::now_ms();
    let day = 86_400_000i64;
    let since = |d: i64| -> Result<i64> {
        Ok(db.conn().query_row(
            "SELECT count(*) FROM turn WHERE ts >= ?1",
            [now - d * day],
            |r| r.get(0),
        )?)
    };
    let (d7, d30) = (since(7)?, since(30)?);
    let oldest: Option<i64> =
        db.conn()
            .query_row("SELECT min(ts) FROM turn WHERE ts > 0", [], |r| r.get(0))?;
    let span_days = oldest.map(|o| ((now - o) / day).max(1)).unwrap_or(1);
    let per_turn = size as f64 / total as f64;
    // The recent rate, not the average: an index that doubled its rate this
    // month is not described by the three-month mean.
    let per_day = (d7 as f64 / 7.0).max(d30 as f64 / 30.0);
    let per_year_gb = per_day * per_turn * 365.0 / 1e9;
    println!(
        "{total} turns over {span_days} days · {:.0}/day recently · {:.1} KB each \
         → about {per_year_gb:.0} GB/year at this rate",
        per_day,
        per_turn / 1024.0
    );
    Ok(())
}

pub(crate) fn run_verify(full: bool) -> Result<()> {
    let db = Db::open(&db_path())?;
    let path = db_path();
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("index {} ({:.2} GB)", path.display(), size as f64 / 1e9);

    let (sessions, turns) = irreplaceable(&db)?;
    let total: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM turn", [], |r| r.get(0))?;
    if sessions > 0 {
        println!(
            "{sessions} session(s) / {turns} of {total} turn(s) exist ONLY here — their \
             transcripts are gone from disk, so this file is the last copy. \
             `tp backup <path>` while it is intact."
        );
    } else {
        println!("every indexed session still has its transcript on disk");
    }
    print_growth(&db, size, total)?;

    // `quick_check` skips the page-by-page verification `integrity_check` does.
    // Both were ~12s on the 2.4 GB index this was written against, so the
    // default is the thorough-enough one and `--full` is there for when a
    // result is being doubted rather than sampled.
    let pragma = if full {
        "integrity_check"
    } else {
        "quick_check"
    };
    let result: String = db
        .conn()
        .query_row(&format!("PRAGMA {pragma}"), [], |r| r.get(0))?;
    println!("{pragma}: {result}");

    // Structural checks SQLite cannot make: they are about teleport's own
    // invariants, and each has been violated by a real bug in this repo — a
    // reindex racing the daemon left one session with 12 of its 10,836 turns.
    let count = |sql: &str| -> Result<i64> { Ok(db.conn().query_row(sql, [], |r| r.get(0))?) };
    let miscounted = count(
        "SELECT count(*) FROM session s
          WHERE s.turn_count != (SELECT count(*) FROM turn t WHERE t.session_id = s.id)",
    )?;
    let orphans = count(
        "SELECT count(*) FROM turn t
          WHERE NOT EXISTS (SELECT 1 FROM session s WHERE s.id = t.session_id)",
    )?;
    let unindexed = count("SELECT count(*) FROM turn")? - count("SELECT count(*) FROM turn_fts")?;
    println!(
        "sessions whose turn_count disagrees with their turns: {miscounted}\n\
         turns with no session: {orphans}\n\
         turns missing from the search index: {unindexed}"
    );

    let ok = result == "ok" && miscounted == 0 && orphans == 0 && unindexed == 0;
    if !ok {
        anyhow::bail!(
            "the index has problems. Sessions whose transcript still exists can be rebuilt with \
             `tp reindex`; the ones listed above as ONLY here cannot, so restore a backup first \
             if you have one."
        );
    }
    println!("ok");
    Ok(())
}

pub(crate) fn run_backup(dest: &std::path::Path) -> Result<()> {
    // Refused rather than overwritten: the file being replaced is the previous
    // backup, and a failed VACUUM INTO onto a destroyed one leaves nothing.
    if dest.exists() {
        anyhow::bail!(
            "{} already exists — refusing to overwrite a backup. Name a new file, or remove it.",
            dest.display()
        );
    }
    let db = Db::open(&db_path())?;
    let (sessions, turns) = irreplaceable(&db)?;

    // VACUUM INTO takes a read lock and writes one consistent snapshot with the
    // WAL applied, so this is safe while tpd is writing. `cp` is not.
    db.conn()
        .execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])
        .with_context(|| format!("writing {}", dest.display()))?;

    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);

    // Recorded AFTER the copy lands, so a failed VACUUM INTO never leaves a
    // claim that a backup exists. `tp version` reads it — without this, the
    // advice to back up is a reminder that cannot tell whether it was taken.
    let total: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM turn", [], |r| r.get(0))
        .unwrap_or(0);
    if let Err(e) = db.record_backup(&dest.to_string_lossy(), total, size) {
        // The backup is on disk and good; only the bookkeeping failed. Saying so
        // is better than either failing the command or silently under-reporting
        // the next `tp version`.
        println!("(could not record this backup: {e:#} — `tp version` will not know about it)");
    }

    println!(
        "wrote {} ({:.2} GB)\n{sessions} session(s) / {turns} turn(s) in it have no other copy",
        dest.display(),
        size as f64 / 1e9
    );
    Ok(())
}
