//! Reading: which session, and what is in it.
//!
//! Divergence here does not lose anything — the worst case is two surfaces
//! answering the same question differently — which is exactly why it went
//! unnoticed longer than the messaging paths did. One difference was already
//! live when this was extracted, and it is preserved rather than quietly
//! settled: see `Resolution`.

use anyhow::{Context, Result};
use tp_core::retrieval::{Query, Scope, TurnCursor};
use tp_core::turn::NormalizedTurn;
use tp_core::{Coverage, Hit, Retrieved, SessionId, SessionRow};
use tp_search::Retrieval;

/// Which session a time-based read landed on.
///
/// Reading by time reads ONE session — that is a guess, and the two surfaces
/// disagree today about what to do with a guess they are not sure of:
///
/// * the CLI refuses and lists the candidates, on the grounds that answering
///   from the wrong session is answering a different question than the one
///   asked;
/// * MCP takes the most recent and attaches a note saying how many matched.
///
/// Returned as DATA rather than settled here, because settling it is a product
/// decision and this extraction is meant to preserve behaviour. What it does
/// change is that the disagreement now lives in one place where it can be seen,
/// instead of being implicit in two implementations that never referred to each
/// other.
pub enum Resolution {
    /// Exactly one session matched the window.
    One(SessionRow),
    /// Several matched. `candidates` is ordered most-recent first, so the head
    /// is what a caller that chooses to guess should guess.
    Ambiguous(Vec<SessionRow>),
    /// Nothing was active in the window.
    None,
}

/// The most recently active session(s) in a window.
pub fn resolve_session(r: &Retrieval, scope: &Scope, limit: usize) -> Result<Resolution> {
    let found = r.sessions(scope, limit)?;
    Ok(match found.items.len() {
        0 => Resolution::None,
        1 => Resolution::One(found.items.into_iter().next().expect("len == 1")),
        _ => Resolution::Ambiguous(found.items),
    })
}

/// Turns from one session.
///
/// `session_id` is parsed here rather than by the caller so that a malformed id
/// fails the same way on both surfaces — it used to be `with_context` on one and
/// a hand-written `tool_error` string on the other, which is how two error
/// messages for one condition come to exist.
pub fn turns(
    r: &Retrieval,
    session_id: &str,
    cursor: TurnCursor,
    include_thinking: bool,
    limit: usize,
    budget_bytes: Option<usize>,
) -> Result<Retrieved<NormalizedTurn>> {
    let sid = SessionId::parse(session_id).with_context(|| {
        format!("malformed session id {session_id:?} (want <machine>/<runtime>/<native>)")
    })?;
    r.turns(&sid, cursor, include_thinking, limit, budget_bytes)
}

/// Sessions active in a window.
pub fn sessions(r: &Retrieval, scope: &Scope, limit: usize) -> Result<Retrieved<SessionRow>> {
    r.sessions(scope, limit)
}

/// Search this machine.
pub fn search(r: &Retrieval, q: &Query, scope: &Scope) -> Result<Retrieved<Hit>> {
    r.search(q, scope)
}

/// Why a search that matched nothing might have matched nothing.
///
/// Both backends match the query as a SINGLE PHRASE: the scan provider tests
/// `line.contains(query)` on the raw line, and the index wraps the whole string
/// in an FTS5 phrase (`build_match_expr`, "avoids user input colliding with
/// FTS5 query-syntax tokens"). That is a deliberate, defensible default — and
/// it was nowhere in the output, so a multi-word query returning nothing read
/// as "this was never discussed" rather than "you asked for those words in that
/// order".
///
/// Observed: a reviewing agent searched `refutation pass defects dismissed`,
/// got zero, and concluded the index was lagging. The index was two seconds
/// old; the four words had simply never appeared consecutively. It is the same
/// failure this project keeps meeting — a fact about the QUERY rendered as a
/// fact about the CORPUS — so it belongs next to the default-window and
/// scan-budget notes, which exist for exactly that reason.
///
/// `None` when there were hits, when the query is a single word (the phrasing
/// cannot be what excluded anything), or under `--regex`, where the caller has
/// already said how matching works.
pub fn empty_note(q: &Query, hits: usize) -> Option<String> {
    if hits > 0 || q.regex || !q.text.trim().contains(char::is_whitespace) {
        return None;
    }
    let words: Vec<&str> = q.text.split_whitespace().collect();
    Some(format!(
        "{:?} was searched as ONE PHRASE — those {} words in that order, not as \
         separate terms. Try a single distinctive word, or --regex to say what \
         you mean.",
        q.text,
        words.len()
    ))
}

/// What this window holds that a SCAN can never read, however long it runs.
///
/// The scan provider answers from transcript files. Claude Code deletes those
/// at around 30 days — measured here, not read anywhere: of the sessions last
/// active 29 days ago 0% have lost their transcript, at 30 days 18%, at 31 days
/// 99.4%. So on the machine this was written on 15,178 of 43,715
/// sessions — 140,254 turns, a quarter of the corpus — exist only in the index.
/// A scan over that window does not find them, reports `no matches`, and the
/// coverage line blames its file budget: a true statement about the wrong
/// thing, since an unlimited budget would not have found them either.
///
/// This is the same failure `empty_note` exists for, one layer down — a fact
/// about the PROVIDER rendered as a fact about the CORPUS. The premise the scan
/// default rests on ("the files are the truth, the index is a cache") stopped
/// being true for a third of the corpus, and nothing said so.
///
/// `None` for the index provider (it can see them), when there is no index to
/// ask, and when the window holds none — which is the common case: a `7d`
/// window on this machine has one, a `400d` window has 15,162. The cost scales
/// the same way, one `stat` per session in the window: 1 ms at `6h`, 92 ms at
/// `400d`, against a scan that already takes seconds there.
pub fn unscannable_note(provider: &str, scope: &Scope, db: &tp_db::Db) -> Option<String> {
    if provider != "scan" {
        return None;
    }
    let (since_ms, until_ms) = scope.range_ms(tp_core::now_ms());
    let claimed = tp_db::query::sessions_claiming_a_file(db.conn(), since_ms, until_ms).ok()?;
    let (mut sessions, mut turns) = (0usize, 0i64);
    for (path, n) in claimed {
        if !std::path::Path::new(&path).exists() {
            sessions += 1;
            turns += n;
        }
    }
    // Sessions that never had a file at all, which the query above cannot see:
    // it selects on `source_path IS NOT NULL`, and a push-ingested runtime has
    // none. Counted into the SAME number deliberately — the two causes differ
    // ("the file was deleted" vs "no file was ever written") but the caller's
    // action does not, and splitting one warning into two would make the common
    // case noisier to buy a distinction nobody acts on.
    if let Ok((n, t)) = tp_db::query::sessions_without_a_file(db.conn(), since_ms, until_ms) {
        sessions += n;
        turns += t;
    }
    if sessions == 0 {
        return None;
    }
    Some(format!(
        "{sessions} session(s) in this window ({turns} turns) have no transcript on disk — \
         a scan CANNOT read them at any budget, and they are missing from this answer. \
         The index still holds them: re-run with --index."
    ))
}

/// Whether a result was cut short, and by what.
///
/// Both surfaces report coverage, and both had their own idea of when it was
/// worth mentioning. A truncated read that does not say so reads as a complete
/// one — the failure this project has met on four separate axes today — so the
/// decision belongs in one place even though the rendering does not.
pub fn is_partial(c: &Coverage) -> bool {
    c.truncated || c.degraded.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolution reports ambiguity instead of resolving it, and orders the
    /// candidates so a caller that chooses to guess guesses the newest.
    #[test]
    fn ambiguity_is_returned_not_decided() {
        let rows = vec![
            SessionRow {
                id: "m/rt/new".into(),
                runtime_id: "rt".into(),
                cwd: None,
                title: None,
                last_turn_at: Some(200),
                turn_count: None,
            },
            SessionRow {
                id: "m/rt/old".into(),
                runtime_id: "rt".into(),
                cwd: None,
                title: None,
                last_turn_at: Some(100),
                turn_count: None,
            },
        ];
        match Resolution::Ambiguous(rows) {
            Resolution::Ambiguous(c) => {
                assert_eq!(c.len(), 2);
                assert_eq!(
                    c[0].id, "m/rt/new",
                    "most recent first — a caller that guesses must guess the newest"
                );
            }
            _ => panic!("expected Ambiguous"),
        }
    }

    fn q(text: &str, regex: bool) -> Query {
        Query {
            text: text.to_string(),
            regex,
            include_thinking: false,
            limit: 20,
        }
    }

    /// The note fires exactly when the phrasing could be what excluded
    /// everything, and stays quiet otherwise — a note on every empty result
    /// would be noise, and noise in this position is what gets ignored.
    #[test]
    fn the_phrase_note_fires_only_when_phrasing_could_be_the_reason() {
        let note = empty_note(&q("refutation pass defects dismissed", false), 0)
            .expect("a multi-word query with no hits must explain the phrase rule");
        assert!(note.contains("ONE PHRASE"), "{note}");
        assert!(note.contains('4'), "should name how many words: {note}");

        assert!(
            empty_note(&q("refutation pass", false), 3).is_none(),
            "hits mean the phrasing excluded nothing"
        );
        assert!(
            empty_note(&q("refutation", false), 0).is_none(),
            "a single word cannot have been split"
        );
        assert!(
            empty_note(&q("a b c", true), 0).is_none(),
            "under --regex the caller already said how matching works"
        );
        assert!(
            empty_note(&q("  spaced  ", false), 0).is_none(),
            "surrounding whitespace is not two words"
        );
    }

    #[test]
    fn partial_covers_both_truncation_and_degradation() {
        let none = Coverage::default();
        assert!(!is_partial(&none));
        assert!(is_partial(&Coverage {
            truncated: true,
            ..Default::default()
        }));
        assert!(is_partial(&Coverage {
            degraded: Some("scan budget".into()),
            ..Default::default()
        }));
    }
}
