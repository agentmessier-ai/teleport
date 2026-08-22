//! Getting turns into storage: pushed by a runtime, or indexed off disk.
//!
//! Only the CLI has ever exposed these, so there is no duplication to remove.
//! They move because of what they are: the two write paths into the corpus.
//! Everything a person or an agent later reads comes through one of them, and
//! neither had a single test — `main.rs` has fifteen and every one covers a
//! formatting helper.
//!
//! The rule worth pinning down is redaction. Scanning redacts inside
//! `tp-watch::ingest`; pushing had to remember to, in the middle of a CLI
//! handler, with nothing checking that it did (LLD §6.3). These turns are the
//! ones that most need it — they were produced by someone else's parser, not
//! ours — and a push path that skipped the funnel would write secrets straight
//! past the single place they are scrubbed.

use anyhow::{Context, Result};
use tp_core::turn::{NormalizedTurn, SessionMeta};
use tp_db::Db;

/// What a push wrote.
///
/// All three counts, because a caller seeing only `inserted` cannot tell a
/// successful re-push (everything a duplicate, nothing wrong) from a broken one.
#[derive(Debug)]
pub struct Pushed {
    pub session_id: String,
    pub inserted: usize,
    pub duplicates: usize,
    /// Accepted without a `prov.uuid`, and therefore not dedupable: re-pushing
    /// these WILL duplicate them. Surfaced so a runtime that could supply a
    /// uuid learns that it should.
    pub unkeyed: usize,
}

/// First user turn with text, truncated — teleport's FALLBACK title.
///
/// `commit_pushed` writes `COALESCE(session.title, excluded.title)`, so a title
/// derived from a LATER batch can never overwrite one already stored. That is
/// what makes deriving safe without tracking whether this is the session's
/// first push: the only batch whose derivation survives is the one that arrived
/// while the session still had no title.
fn derive_title(turns: &[NormalizedTurn]) -> Option<String> {
    turns
        .iter()
        .find(|t| t.role == tp_core::turn::Role::User && !t.text.is_empty())
        .map(|t| {
            t.text
                .chars()
                .take(tp_ingest::adapter::jsonl::TITLE_CHARS)
                .collect()
        })
}

/// Parse a pushed batch.
///
/// Separate from `push` so a malformed batch is rejected before a database is
/// opened, and so the error says what the input was supposed to be.
pub fn parse_turns(raw: &str) -> Result<Vec<NormalizedTurn>> {
    serde_json::from_str(raw).context("stdin must be a JSON array of normalized turns")
}

/// Store turns a runtime handed us, redacting them on the way in.
///
/// `session_id` is the composite `<machine>/<runtime>/<native>`; the native id
/// is taken from it rather than from a separate argument, so the two cannot
/// disagree about which session was written.
pub fn push(
    db: &mut Db,
    machine_id: &str,
    session_id: &str,
    runtime_id: &str,
    meta: &SessionMeta,
    mut turns: Vec<NormalizedTurn>,
) -> Result<Pushed> {
    // The one non-negotiable step, and the reason this function exists.
    for t in &mut turns {
        tp_ingest::redact::redact(t);
    }

    // Derive the FALLBACK title when the pusher supplied none, by the same rule
    // the disk path uses — first user turn, `jsonl::TITLE_CHARS`.
    //
    // Both write paths now share one owner for this. It used to be the caller's
    // job with an optional flag, so forgetting it cost nothing at push time and
    // surfaced much later as a session with no readable name: every dsh session
    // in the live database had a NULL title (2 of 2) against 45 of 41,938 for
    // the disk-read runtimes.
    //
    // It writes `title_derived`, never `title_user`/`title_ai`. dsh's
    // `SessionHeader` has no title field at all, so a pusher that sends none is
    // not declining — it may have nothing to send, and teleport must not record
    // a truncated first message where a runtime-stated title would go.
    let meta = match meta.title_derived {
        Some(_) => meta.clone(),
        None => SessionMeta {
            title_derived: derive_title(&turns),
            ..meta.clone()
        },
    };

    let native = session_id.rsplit('/').next().unwrap_or(session_id);
    db.ensure_self_machine(machine_id, "")?;
    db.ensure_runtime(runtime_id, "")?;

    let out = tp_db::writer::commit_pushed(
        db.conn_mut(),
        session_id,
        machine_id,
        runtime_id,
        native,
        &meta,
        &turns,
    )?;
    Ok(Pushed {
        session_id: session_id.to_string(),
        inserted: out.inserted,
        duplicates: out.duplicates,
        unkeyed: out.unkeyed,
    })
}

/// What indexing one runtime's root did.
#[derive(Debug)]
pub enum Indexed {
    /// The root does not exist on this machine. Not an error and not silence:
    /// a runtime that is simply not installed here must be distinguishable
    /// from one that was indexed and had nothing to give.
    NoRoot { runtime_id: String, root: String },
    Scanned {
        runtime_id: String,
        files_touched: usize,
        turns_written: usize,
        sources_seen: usize,
        /// Files this pass could not read, path and reason. Carried up rather
        /// than left in the log because a caller that DELETED rows before
        /// refilling them cannot treat a partial refill as success — see
        /// `ScanOutcome`.
        failed: Vec<(std::path::PathBuf, String)>,
    },
}

/// Index every runtime's transcripts on this machine.
///
/// Every runtime, not just Claude Code: one missing here would be searchable
/// under `--index` on the scan path and invisible on the index path, which is
/// the split answer LLD §16 forbids.
pub fn index_all(db: &mut Db, machine_id: &str, hostname: &str) -> Result<Vec<Indexed>> {
    db.ensure_self_machine(machine_id, hostname)?;

    let adapters = tp_ingest::adapter::all_adapters();
    let roots = tp_ingest::adapter::all_roots();
    let mut out = Vec::new();

    for (adapter, (runtime_id, root)) in adapters.iter().zip(roots.iter()) {
        if !root.exists() {
            out.push(Indexed::NoRoot {
                runtime_id: runtime_id.to_string(),
                root: root.display().to_string(),
            });
            continue;
        }
        db.ensure_runtime(runtime_id, root.to_str().unwrap_or_default())?;

        // The same ingest path the watcher uses — one implementation, two
        // triggers.
        let scanned = tp_watch::ingest::scan_root(db, machine_id, adapter.as_ref(), root)?;
        let sources = adapter.discover(root)?;

        out.push(Indexed::Scanned {
            runtime_id: runtime_id.to_string(),
            files_touched: scanned.files,
            turns_written: scanned.turns,
            failed: scanned.failed,
            sources_seen: sources.len(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_core::turn::{Provenance, Role};

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db
    }

    fn turn(uuid: Option<&str>, text: &str) -> NormalizedTurn {
        NormalizedTurn {
            role: Role::User,
            text: text.to_string(),
            ts: Some(1_000),
            thinking: String::new(),
            thinking_opaque: false,
            tool_calls: Vec::new(),
            surface: Default::default(),
            tokens_in: None,
            tokens_out: None,
            prov: Provenance {
                uuid: uuid.map(str::to_string),
                ..Default::default()
            },
        }
    }

    /// The rule this module exists for. A pushed turn goes through the same
    /// scrubber as a scanned one — these came from someone else's parser, and
    /// nothing downstream redacts again.
    #[test]
    fn a_pushed_turn_is_redacted_before_it_is_stored() {
        let mut db = db();
        let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        push(
            &mut db,
            "m1",
            "m1/dsh/s",
            "dsh",
            &SessionMeta::default(),
            vec![turn(Some("u1"), &format!("token is {secret}"))],
        )
        .unwrap();

        let stored: String = db
            .conn()
            .query_row(
                "SELECT text FROM turn WHERE session_id = 'm1/dsh/s'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !stored.contains(secret),
            "a secret reached storage unredacted: {stored}"
        );
    }

    /// Re-pushing the same batch is a no-op that says so. A caller shown only
    /// `inserted: 0` cannot tell this from a push that failed.
    #[test]
    fn re_pushing_reports_duplicates_rather_than_writing_them() {
        let mut db = db();
        let batch = || vec![turn(Some("u1"), "hello"), turn(Some("u2"), "again")];

        let first = push(
            &mut db,
            "m1",
            "m1/dsh/s",
            "dsh",
            &SessionMeta::default(),
            batch(),
        )
        .unwrap();
        assert_eq!((first.inserted, first.duplicates), (2, 0));

        let second = push(
            &mut db,
            "m1",
            "m1/dsh/s",
            "dsh",
            &SessionMeta::default(),
            batch(),
        )
        .unwrap();
        assert_eq!((second.inserted, second.duplicates), (0, 2));
    }

    /// A turn with no uuid cannot be deduplicated, so it is accepted AND
    /// counted — the count is the only warning the pusher gets that a retry
    /// will double it.
    #[test]
    fn a_turn_without_a_uuid_is_accepted_and_reported_as_unkeyed() {
        let mut db = db();
        let out = push(
            &mut db,
            "m1",
            "m1/dsh/s",
            "dsh",
            &SessionMeta::default(),
            vec![turn(None, "no provenance")],
        )
        .unwrap();
        assert_eq!(out.inserted, 1);
        assert_eq!(out.unkeyed, 1);
    }

    /// The native id comes from the composite address, so the row cannot end
    /// up filed under a session id that does not match the one it was stored
    /// against.
    #[test]
    fn the_native_id_is_taken_from_the_address_it_was_stored_under() {
        let mut db = db();
        push(
            &mut db,
            "m1",
            "m1/claude_code/abc-123",
            "claude_code",
            &SessionMeta::default(),
            vec![turn(Some("u1"), "hi")],
        )
        .unwrap();
        let native: String = db
            .conn()
            .query_row(
                "SELECT native_id FROM session WHERE id = 'm1/claude_code/abc-123'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(native, "abc-123");
    }

    #[test]
    fn a_malformed_batch_is_refused_with_what_was_expected() {
        let err = parse_turns("{\"not\": \"an array\"}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("normalized turns"), "{err}");
    }
}

#[cfg(test)]
mod title_tests {
    use super::*;
    use tp_core::turn::{Provenance, Role};

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.ensure_self_machine("m1", "TestMac").unwrap();
        db
    }

    fn t(role: Role, text: &str) -> NormalizedTurn {
        NormalizedTurn {
            role,
            text: text.to_string(),
            ts: Some(1_000),
            thinking: String::new(),
            thinking_opaque: false,
            tool_calls: Vec::new(),
            surface: Default::default(),
            tokens_in: None,
            tokens_out: None,
            prov: Provenance::default(),
        }
    }

    /// The DERIVED column — the only one this module writes.
    fn title_of(db: &Db, id: &str) -> Option<String> {
        db.conn()
            .query_row(
                "SELECT title_derived FROM session WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// What the PUSHER stated, which must not be overwritten by the fallback.
    fn user_title_of(db: &Db, id: &str) -> Option<String> {
        db.conn()
            .query_row("SELECT title_user FROM session WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// The defect this exists for: dsh pushed 2 sessions and both had a NULL
    /// title, because `--title` is optional and its plugin never sent it. The
    /// disk path derives the same field itself, so a pusher that declines was
    /// getting a worse session than a scanned one for no stated reason.
    #[test]
    fn a_push_with_no_title_is_titled_from_its_first_user_turn() {
        let mut db = db();
        push(
            &mut db,
            "m1",
            "m1/dsh/s1",
            "dsh",
            &SessionMeta::default(),
            vec![
                t(Role::Assistant, "I will start now"),
                t(Role::User, "add a title to pushed sessions"),
            ],
        )
        .unwrap();
        assert_eq!(
            title_of(&db, "m1/dsh/s1").as_deref(),
            Some("add a title to pushed sessions"),
            "the first USER turn titles the session, not the first turn"
        );
    }

    /// A runtime-stated title and the derived fallback now COEXIST in separate
    /// columns, and precedence is applied on read. The pusher's title must land
    /// in `title_user`, and deriving must still fill `title_derived` — storing
    /// only one of them is what made "teleport did not look" and "this runtime
    /// has no title" the same value.
    #[test]
    fn an_explicit_title_and_the_derived_fallback_coexist() {
        let mut db = db();
        push(
            &mut db,
            "m1",
            "m1/dsh/s2",
            "dsh",
            &SessionMeta {
                title_user: Some("chosen by the runtime".into()),
                ..Default::default()
            },
            vec![t(Role::User, "first user text")],
        )
        .unwrap();
        assert_eq!(
            user_title_of(&db, "m1/dsh/s2").as_deref(),
            Some("chosen by the runtime")
        );
        assert_eq!(
            title_of(&db, "m1/dsh/s2").as_deref(),
            Some("first user text"),
            "the fallback is still recorded — it just loses on read"
        );
    }

    /// The hazard deriving introduces, and why it is safe anyway: pushes arrive
    /// in batches, so a LATER batch's first user turn is mid-conversation and
    /// would be a wrong title. `commit_pushed`'s COALESCE is what stops it —
    /// the disk path guards the same mistake with `first_pass`.
    #[test]
    fn a_later_batch_cannot_retitle_the_session() {
        let mut db = db();
        let sess = "m1/dsh/s3";
        push(
            &mut db,
            "m1",
            sess,
            "dsh",
            &SessionMeta::default(),
            vec![t(Role::User, "the real opening question")],
        )
        .unwrap();
        push(
            &mut db,
            "m1",
            sess,
            "dsh",
            &SessionMeta::default(),
            vec![t(Role::User, "something said an hour in")],
        )
        .unwrap();
        assert_eq!(
            title_of(&db, sess).as_deref(),
            Some("the real opening question"),
            "a resumed push must not retitle"
        );
    }

    /// A batch with no user turn yet leaves the title unset rather than
    /// titling the session with an assistant message.
    #[test]
    fn a_batch_with_no_user_turn_leaves_the_title_unset() {
        let mut db = db();
        push(
            &mut db,
            "m1",
            "m1/dsh/s4",
            "dsh",
            &SessionMeta::default(),
            vec![t(Role::Assistant, "thinking out loud")],
        )
        .unwrap();
        assert_eq!(title_of(&db, "m1/dsh/s4"), None);
    }
}
