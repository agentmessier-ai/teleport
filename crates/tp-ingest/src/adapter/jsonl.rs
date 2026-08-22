//! Incremental JSONL reading, once.
//!
//! Every runtime teleport indexes writes one JSON object per line and appends
//! while a session is live, so every adapter needs the same loop: read from a
//! byte offset, split on newlines, and report how far it got. Three adapters
//! had written it out — `claude_code`, `pi` and the declarative engine, whose
//! own copy carries the comment "this is the shared machinery the whole module
//! exists to stop people copying". Two of the three were still byte-identical
//! after normalisation; the third had already started to drift.
//!
//! The rule that makes this worth centralising is the torn final line. A live
//! transcript can be read mid-`write`, so the last line may be half an object.
//! It must be neither parsed nor counted in `new_offset` (the adapter contract
//! in `mod.rs` says so): counting it would resume the next poll from the middle
//! of a line, and that turn would never be seen again. It is a silent,
//! permanent loss of exactly the newest thing the user said — and it is only
//! reachable when a poll lands inside a write, which is why nobody finds it by
//! trying it once.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use tp_core::turn::{CompactionBoundary, NormalizedTurn, Role, SessionMeta};

use super::ParseChunk;

/// How much of the first user message becomes the fallback title.
///
/// 200 has no stated origin — it was copied between call sites, and this is the
/// first place it is named. It bounds a display string, so the number matters
/// less than there being ONE of it: `scan.rs` truncates the same way for the
/// same purpose, and the two silently disagreeing would make a session's title
/// depend on which provider answered.
pub const TITLE_CHARS: usize = 200;

/// Asks whether a record is a compaction marker, given the number of turns seen
/// BEFORE it — which is what a positional boundary needs and an anchored one
/// ignores. Named rather than written inline because the bare type is unreadable
/// and clippy says so.
type BoundaryFn<'a> = &'a dyn Fn(&Value, usize) -> Option<CompactionBoundary>;

/// Read complete JSONL records from `offset` onward.
///
/// `cwd_at` and `parse_entry` are the only parts that are ever runtime-specific:
/// where the working directory is recorded, and how one object becomes a turn.
/// Everything else — byte accounting, the torn line, blank and unparseable
/// lines, and which session metadata may be set — is the same everywhere and
/// lives here.
///
/// Session metadata is collected ONLY on the `offset == 0` pass. A later poll
/// re-reading the tail of a file must not overwrite the title with whatever the
/// user happened to type an hour in.
pub fn read_chunk(
    path: &Path,
    offset: u64,
    cwd_at: impl Fn(&Value) -> Option<String>,
    parse_entry: impl Fn(&Value) -> Option<NormalizedTurn>,
    title_at: impl Fn(&Value) -> Option<(tp_core::turn::TitleSource, String)>,
    // `None` = this runtime's compaction marker is unknown to us, which is NOT
    // the same as "there was none" — see `ParseChunk::tracks_compaction`.
    // Returns the boundary this record declares, if any. `None` for the whole
    // closure means the runtime's marker is unknown to us, which is NOT the same
    // as "there was none" — see `ParseChunk::tracks_compaction`.
    compaction_at: Option<BoundaryFn<'_>>,
) -> Result<ParseChunk> {
    let mut file = File::open(path).with_context(|| format!("open {path:?}"))?;
    file.seek(SeekFrom::Start(offset))?;
    // BYTES, not `read_to_string`. A live transcript can be read mid-write, and
    // a write can be torn in the middle of a MULTI-BYTE CHARACTER as easily as
    // in the middle of a line — CJK, emoji and box-drawing are routine in these
    // transcripts. `read_to_string` validates the WHOLE buffer, so two stray
    // bytes at the end made it return "stream did not contain valid UTF-8" and
    // the entire chunk failed, including every complete line before them. That
    // error then propagated out of the watcher and killed indexing for the life
    // of the daemon. Verified by hand: one complete JSON line followed by the
    // bytes E2 82 (a truncated €) fails the read outright.
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    let mut turns = Vec::new();
    let mut compaction = Vec::new();
    let mut meta = SessionMeta::default();
    let mut consumed: u64 = 0;
    let first_pass = offset == 0;

    // Byte-exact splitting, newline included, so `consumed` maps onto file
    // offsets. Running out of newlines ends the loop with the remainder UNREAD:
    // that is the torn tail — a partial line, a partial character, or both —
    // and leaving it uncounted is what lets the next poll re-read it whole.
    let mut rest = bytes.as_slice();
    while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
        let line = &rest[..nl];
        consumed += (nl + 1) as u64;
        rest = &rest[nl + 1..];

        // A complete line that is not valid UTF-8 is consumed and skipped, for
        // the same reason a complete line that is not valid JSON is: the
        // newline proves the writer finished it, so this is corruption rather
        // than a write in progress, and stopping would wedge the session on it
        // forever.
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };

        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if first_pass && meta.cwd.is_none() {
            meta.cwd = cwd_at(&v);
        }

        // A title entry is NOT gated on `first_pass`. A rename can happen at any
        // point in a session — Claude Code appends a `custom-title` entry when
        // the user runs /rename — so the newest one in the file is the answer,
        // and only reading the head would freeze whatever was there first. This
        // is the opposite rule from `title_derived` below, deliberately: one is
        // a statement the runtime made, the other is teleport guessing from the
        // opening message.
        if let Some((source, title)) = title_at(&v) {
            meta.set_title(source, title);
        }

        // Checked BEFORE parsing the record as a turn, and deliberately without
        // skipping it. The two runtimes need opposite treatment and both get it
        // this way: Claude Code's marker maps to no role, so it contributes no
        // turn; pi's `compaction` entry carries the distilled summary of what it
        // replaced and pi.toml indexes it on purpose ("skipping it would put a
        // recall hole in exactly the long sessions search matters most for").
        //
        // Order matters: the boundary is recorded against the turns seen SO FAR,
        // so a marker that is also a turn lands after its own boundary and stays
        // current — which is right, a compaction summary is new context.
        if let Some(boundary_of) = compaction_at {
            if let Some(b) = boundary_of(&v, turns.len()) {
                compaction.push(b);
            }
        }

        if let Some(turn) = parse_entry(&v) {
            // The FALLBACK title, and now labelled as one. Every runtime
            // teleport reads has a native title somewhere; this is what
            // teleport has instead of reading it, and
            // it loses to any real title in the read-time COALESCE.
            if first_pass
                && meta.title_derived.is_none()
                && turn.role == Role::User
                && !turn.text.is_empty()
            {
                meta.title_derived = Some(turn.text.chars().take(TITLE_CHARS).collect());
            }
            if meta.started_at.is_none() {
                meta.started_at = turn.ts;
            }
            turns.push(turn);
        }
    }

    Ok(ParseChunk {
        turns,
        new_offset: offset + consumed,
        meta,
        compaction,
        tracks_compaction: compaction_at.is_some(),
    })
}

/// Normalise ONE line, for the scan backend.
///
/// The same three rules `read_chunk` applies per line — blank is nothing,
/// unparseable is nothing, otherwise hand it to the runtime's own parser. Both
/// backends must agree about what a line means or a session reads differently
/// depending on which provider answered, which is what `decl`'s
/// `parse_from_agrees_*` test exists to catch.
pub fn parse_one(
    line: &str,
    parse_entry: impl Fn(&Value) -> Option<NormalizedTurn>,
) -> Option<NormalizedTurn> {
    if line.trim().is_empty() {
        return None;
    }
    parse_entry(&serde_json::from_str::<Value>(line).ok()?)
}

/// `v["cwd"]` — what every hand-written adapter looks for.
pub fn cwd_field(v: &Value) -> Option<String> {
    v.get("cwd").and_then(|c| c.as_str()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn user(v: &Value) -> Option<NormalizedTurn> {
        let text = v.get("text")?.as_str()?.to_string();
        Some(NormalizedTurn {
            role: Role::User,
            ts: v.get("ts").and_then(|t| t.as_i64()),
            text,
            thinking: String::new(),
            thinking_opaque: false,
            tool_calls: Vec::new(),
            surface: Default::default(),
            tokens_in: None,
            tokens_out: None,
            prov: Default::default(),
        })
    }

    fn write(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let mut f = File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        (dir, p)
    }

    fn read(p: &Path, offset: u64) -> ParseChunk {
        read_chunk(p, offset, cwd_field, user, |_| None, None).unwrap()
    }

    /// The rule this module exists for. A poll that lands inside a write sees
    /// half an object; consuming it would resume the next poll mid-line and
    /// that turn would be lost permanently.
    #[test]
    fn a_torn_final_line_is_neither_parsed_nor_consumed() {
        let whole = "{\"ts\":1,\"text\":\"first\"}\n";
        let (_d, p) = write(&format!("{whole}{{\"ts\":2,\"text\":\"tor"));

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1, "the torn line must not be parsed");
        assert_eq!(
            chunk.new_offset,
            whole.len() as u64,
            "new_offset must stop at the last complete line"
        );
    }

    /// And the other half of the same rule: once the writer finishes the line,
    /// resuming from `new_offset` picks it up whole. Without this, the first
    /// test could be satisfied by dropping the turn entirely.
    #[test]
    fn the_torn_line_is_recovered_when_the_write_completes() {
        let head = "{\"ts\":1,\"text\":\"first\"}\n";
        let (_d, p) = write(&format!("{head}{{\"ts\":2,\"text\":\"tor"));
        let first = read(&p, 0);

        // The writer finishes the record.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"n\"}\n").unwrap();

        let second = read(&p, first.new_offset);
        assert_eq!(second.turns.len(), 1);
        assert_eq!(second.turns[0].text, "torn");
    }

    /// A complete-but-unparseable line is consumed. The newline proves the
    /// writer finished it, so stopping would wedge the session on one bad
    /// record forever.
    #[test]
    fn a_complete_line_that_is_not_json_is_skipped_not_retried() {
        let body = "not json at all\n{\"ts\":1,\"text\":\"after\"}\n";
        let (_d, p) = write(body);

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1);
        assert_eq!(chunk.turns[0].text, "after");
        assert_eq!(chunk.new_offset, body.len() as u64, "both lines consumed");
    }

    #[test]
    fn blank_lines_are_consumed_and_ignored() {
        let body = "\n\n{\"ts\":1,\"text\":\"x\"}\n\n";
        let (_d, p) = write(body);

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1);
        assert_eq!(chunk.new_offset, body.len() as u64);
    }

    /// Metadata belongs to the session, not to the chunk. A later poll must
    /// not retitle the session with whatever was said an hour in.
    #[test]
    fn session_metadata_is_taken_only_on_the_first_pass() {
        let head = "{\"cwd\":\"/w\",\"ts\":1,\"text\":\"the title\"}\n";
        let (_d, p) = write(&format!(
            "{head}{{\"cwd\":\"/elsewhere\",\"ts\":2,\"text\":\"later\"}}\n"
        ));

        let first = read(&p, 0);
        assert_eq!(first.meta.cwd.as_deref(), Some("/w"));
        assert_eq!(first.meta.title_derived.as_deref(), Some("the title"));
        assert_eq!(first.meta.started_at, Some(1));

        let second = read(&p, head.len() as u64);
        assert_eq!(second.meta.cwd, None, "a resumed read must not retitle");
        assert_eq!(second.meta.title_derived, None);
    }

    /// Offsets are byte counts, not character counts — a multi-byte line would
    /// otherwise leave the next read starting mid-character.
    #[test]
    fn offsets_are_bytes_even_when_the_text_is_not_ascii() {
        let body = "{\"ts\":1,\"text\":\"a €10 line\"}\n{\"ts\":2,\"text\":\"second\"}\n";
        let (_d, p) = write(body);

        let chunk = read(&p, 0);
        assert_eq!(chunk.new_offset, body.len() as u64);
        assert_eq!(chunk.turns.len(), 2);
    }

    /// A write torn in the middle of a MULTI-BYTE CHARACTER, which is what
    /// `read_to_string` could not survive. Two stray bytes at the end used to
    /// fail the whole read — losing the complete line before them, and, because
    /// that error propagated, stopping the watcher for the life of the daemon.
    #[test]
    fn a_torn_multibyte_character_does_not_lose_the_complete_lines_before_it() {
        let whole = "{\"ts\":1,\"text\":\"first\"}\n";
        let (_d, p) = write(whole);
        // Append the first two bytes of € (E2 82 AC) — a character the writer
        // has not finished emitting.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&[0xE2, 0x82]).unwrap();

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1, "the complete line must survive");
        assert_eq!(
            chunk.new_offset,
            whole.len() as u64,
            "the torn character must not be consumed"
        );
    }

    /// And the recovery half: once the writer finishes the character, the line
    /// containing it is read whole.
    #[test]
    fn the_torn_character_is_recovered_when_the_write_completes() {
        let head = "{\"ts\":1,\"text\":\"first\"}\n";
        let (_d, p) = write(head);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{\"ts\":2,\"text\":\"").unwrap();
        f.write_all(&[0xE2, 0x82]).unwrap();
        let first = read(&p, 0);
        assert_eq!(first.turns.len(), 1);

        f.write_all(&[0xAC]).unwrap();
        f.write_all(b"\"}\n").unwrap();
        let second = read(&p, first.new_offset);
        assert_eq!(second.turns.len(), 1);
        assert_eq!(second.turns[0].text, "€");
    }

    /// A COMPLETE line that is not valid UTF-8 is corruption, not a write in
    /// progress. It is consumed and skipped, exactly like a complete line that
    /// is not valid JSON — stopping would wedge the session on it forever.
    #[test]
    fn a_complete_line_that_is_not_utf8_is_skipped_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.jsonl");
        let mut f = File::create(&p).unwrap();
        f.write_all(&[0xFF, 0xFE]).unwrap();
        f.write_all(b"\n{\"ts\":1,\"text\":\"after\"}\n").unwrap();
        drop(f);

        let chunk = read(&p, 0);
        assert_eq!(chunk.turns.len(), 1);
        assert_eq!(chunk.turns[0].text, "after");
        assert_eq!(
            chunk.new_offset,
            std::fs::metadata(&p).unwrap().len(),
            "both lines consumed"
        );
    }

    /// An empty file is a session that has not been written to yet, not an
    /// error and not a torn line.
    #[test]
    fn an_empty_file_yields_nothing_and_advances_nothing() {
        let (_d, p) = write("");
        let chunk = read(&p, 0);
        assert!(chunk.turns.is_empty());
        assert_eq!(chunk.new_offset, 0);
    }
}
