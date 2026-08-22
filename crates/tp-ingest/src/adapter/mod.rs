//! Adapter contract (LLD §5). `parse_from` MUST be resumable and side-effect
//! free, and MUST tolerate a torn final line — the source runtime may be
//! mid-write when we read.

pub mod decl;
pub mod jsonl;

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tp_core::turn::{NormalizedTurn, ParseChunk};

/// Never the raw tool input (LLD §4: tool inputs are large and frequently carry
/// secrets) — a short, non-reversible preview only. Shared by every adapter so
/// no runtime can drift into persisting more than this.
pub(crate) fn digest_input(input: &Value) -> String {
    let s = input.to_string();
    s.chars().take(80).collect()
}

/// Every runtime this build can read. `all_adapters()` and `all_roots()` are
/// index-aligned and MUST be registered together — they exist as one pair, in
/// one place, because the CLI, the daemon and the watcher each need the same
/// list and a runtime added to only some of them is invisible in the others.
/// (That exact drift already produced one live bug: the active scan didn't
/// know about pi and silently pruned its registrations — docs/pi-integration.md.)
/// Every runtime this build can read, as one list so the adapter and its root
/// can never disagree — they used to be two functions maintained in parallel,
/// which is exactly how a runtime ends up registered in one and invisible in
/// the other (docs/pi-integration.md records that bug).
///
/// A TOML config in `~/.teleport/runtimes.d/` OVERRIDES the built-in of the
/// same id, and a config with a new id ADDS a runtime — so a format in the
/// mapped class can be supported without a rebuild, which is what LLD §5 asked
/// for. Anything the config can't express still drops to a Rust impl.
/// One harness, completely described: what it is, where its transcripts live,
/// how to parse them, and what it can and cannot do (docs/reach-provider.md).
///
/// This replaces a `(String, PathBuf, Box<dyn Adapter>)` tuple. The tuple was
/// already the shape that had to be edited everywhere each time a field was
/// added, and `capabilities` is the field that would have made it a 4-tuple —
/// so it gets a name before that happens rather than after.
pub struct Harness {
    pub id: String,
    /// Human-readable; falls back to `id`.
    pub name: String,
    /// Where its transcripts live. Present even for harnesses teleport cannot
    /// usefully parse — absence would mean "no read side at all".
    pub root: PathBuf,
    pub adapter: Box<dyn Adapter>,
    pub capabilities: decl::Capabilities,
}

pub fn all_runtimes() -> Vec<Harness> {
    // The built-ins ARE the shipped descriptors, compiled in via `include_str!`
    // (see `DeclConfig::embedded`). There is no hand-written adapter behind
    // them any more: the descriptor is the implementation, a file in
    // `~/.teleport/runtimes.d/` overrides it by id, and a binary running with
    // nothing installed behaves identically to a full install — including
    // codex, which previously did not exist at all without its file.
    let mut out: Vec<Harness> = [
        decl::DeclConfig::claude_code(),
        decl::DeclConfig::pi(),
        decl::DeclConfig::codex(),
    ]
    .into_iter()
    .map(|cfg| Harness {
        id: cfg.id.clone(),
        name: cfg.name.clone().unwrap_or_else(|| cfg.id.clone()),
        root: cfg.resolved_root(),
        capabilities: cfg.capabilities.clone(),
        adapter: Box::new(decl::DeclAdapter::new(cfg)),
    })
    .collect();
    for cfg in decl::load_configs(&decl::config_dir()) {
        let harness = Harness {
            id: cfg.id.clone(),
            name: cfg.name.clone().unwrap_or_else(|| cfg.id.clone()),
            root: cfg.resolved_root(),
            capabilities: cfg.capabilities.clone(),
            adapter: Box::new(decl::DeclAdapter::new(cfg)),
        };
        match out.iter().position(|h| h.id == harness.id) {
            Some(i) => out[i] = harness,
            None => out.push(harness),
        }
    }
    out
}

/// An installed file in `~/.teleport/runtimes.d/` that shadows a built-in
/// runtime, and whether it still says anything the binary does not.
pub struct DescriptorOverride {
    pub path: PathBuf,
    pub id: String,
    /// Byte-identical to this build's embedded copy — a no-op override. It
    /// changes nothing today and becomes a stale one the next time the shipped
    /// descriptor moves, which is why the report calls it safe to delete.
    pub identical: bool,
}

/// The overrides currently shadowing built-ins, for `tp version` and the
/// daemon's startup log.
///
/// Two facts made this worth a check. The override mechanism is BY DESIGN —
/// a customized descriptor must win silently. And the same mechanism ate two
/// full debug cycles in one session: `install.sh` used to copy the shipped
/// descriptors out, the copies outlived the binaries that matched them, and a
/// rebuilt binary ran "byte-identical wrong" against text it no longer
/// contained — twice (a compaction fix, then a sidechain fix). Content alone
/// cannot distinguish "customized" from "stale copy of an older ship", so this
/// does not try: it reports that the file differs and from what, and the one
/// case it CAN settle — byte-identical, therefore pure redundancy — it labels
/// as such. Files whose id is not a built-in are additions, not overrides, and
/// are not listed. Files that fail to parse are `load_configs`' problem; it
/// already warns.
pub fn descriptor_overrides() -> Vec<DescriptorOverride> {
    descriptor_overrides_in(&decl::config_dir())
}

pub fn descriptor_overrides_in(dir: &Path) -> Vec<DescriptorOverride> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(cfg) = toml::from_str::<decl::DeclConfig>(&text) else {
            continue;
        };
        let Some((id, embedded)) = decl::EMBEDDED.iter().find(|(id, _)| *id == cfg.id) else {
            continue; // an added runtime, not an override
        };
        out.push(DescriptorOverride {
            path,
            id: id.to_string(),
            identical: text == *embedded,
        });
    }
    out
}

pub fn all_adapters() -> Vec<Box<dyn Adapter>> {
    all_runtimes().into_iter().map(|h| h.adapter).collect()
}

/// Process signatures for every harness that declared one, in registration
/// order. This is the single source for "what does this runtime's process look
/// like" — the question that used to be answered by a hardcoded chain in
/// `tp-reach::discover::recognize_runtime` AND, in the other direction, by
/// `ancestor_needle` in the CLI. One table, read from the descriptors.
pub fn process_signatures() -> Vec<(String, String)> {
    all_runtimes()
        .into_iter()
        .filter_map(|h| h.capabilities.process_match.map(|p| (h.id, p)))
        .collect()
}

/// The `comm` pattern for one runtime, if it declared one.
pub fn process_signature_for(runtime_id: &str) -> Option<String> {
    all_runtimes()
        .into_iter()
        .find(|h| h.id == runtime_id)
        .and_then(|h| h.capabilities.process_match)
}

/// The phrase that makes THIS runtime check its inbox, if it declared one.
pub fn control_string_for(runtime_id: &str) -> Option<String> {
    all_runtimes()
        .into_iter()
        .find(|h| h.id == runtime_id)
        .and_then(|h| h.capabilities.control_string)
}

pub fn all_roots() -> Vec<(String, PathBuf)> {
    all_runtimes().into_iter().map(|h| (h.id, h.root)).collect()
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    /// Unix inode number — the identity `ingest_state` keys on (LLD §15 #1).
    pub inode: i64,
    pub size: u64,
    pub mtime_ms: i64,
    /// Adapter-native session id (e.g. Claude Code's session UUID, taken from the filename).
    pub native_id: String,
}

pub trait Adapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn discover(&self, root: &Path) -> Result<Vec<SourceFile>>;

    /// The primitive both retrieval strategies share (LLD §16): one raw line →
    /// at most one normalized turn. Returns `None` for blank/malformed lines and
    /// for non-conversational records (queue-operation, last-prompt, …).
    ///
    /// Keeping this as the primitive means the scan backend and the index
    /// backend never drift on format quirks — including `thinking` extraction,
    /// which a naive line-grep would miss entirely.
    fn parse_line(&self, line: &str) -> Option<NormalizedTurn>;

    /// This session's working directory, if THIS record carries it.
    ///
    /// On the trait because only the adapter knows where its runtime puts it.
    /// Callers used to read a top-level `cwd` themselves, which is right for
    /// the two runtimes teleport shipped with and wrong for any runtime that
    /// nests it — codex writes it once, on a `session_meta` line, under
    /// `payload`. A caller-side guess also silently disagreed with the
    /// adapter's own parse of the same file.
    ///
    /// The default is exactly the old behaviour, so hand-written adapters and
    /// every existing descriptor keep working unchanged.
    fn cwd_of_line(&self, line: &str) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()?
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(str::to_string)
    }

    /// The session TITLE this record states, if it states one.
    ///
    /// On the trait for the same reason as `cwd_of_line`: only the adapter knows
    /// where its runtime puts it, and both retrieval backends must get the same
    /// answer. They did not, briefly — the index read Claude Code's `ai-title`
    /// entries while the scan kept deriving from the first user message, so
    /// `tp sessions` and `tp --index sessions` printed different titles for the
    /// same session. That is precisely the split LLD §16 forbids.
    ///
    /// Default `None` = "this runtime states no title I know how to read", which
    /// leaves teleport's derived fallback in place. Correct for a hand-written
    /// adapter and for any descriptor without `title_rules`.
    fn title_of_line(&self, line: &str) -> Option<(tp_core::turn::TitleSource, String)> {
        let _ = line;
        None
    }

    /// Index-only (LLD §16): parse `[offset, EOF)` resumably, built on
    /// `parse_line`. `new_offset` must exclude any unterminated trailing line.
    fn parse_from(&self, path: &Path, offset: u64) -> Result<ParseChunk>;

    /// Find ONE session's file by its native id.
    ///
    /// Reading a single session used to go through `discover()`, which enumerates
    /// and `stat`s every session file under the root — 24,623 of them on this
    /// machine — to keep one. That makes a single-session read cost
    /// O(whole corpus): it scales with how much history exists, not with what
    /// you asked for, so every `tp turns` gets slower as unrelated projects
    /// accumulate, and a paginated read pays it again per page.
    ///
    /// The default preserves exactly that behaviour, so an adapter that doesn't
    /// override this is slow, never wrong. Adapters whose on-disk layout lets
    /// them address a file directly should override it.
    fn locate(&self, root: &Path, native_id: &str) -> Result<Option<SourceFile>> {
        Ok(self
            .discover(root)?
            .into_iter()
            .find(|s| s.native_id == native_id))
    }
}

/// A native id is interpolated into a filesystem path by `locate`, and ids
/// arrive from the wire (a peer's search result, an agent's tool call). Anything
/// that could climb out of the root is refused rather than sanitized — there is
/// no legitimate id containing a separator, so rejecting is lossless.
pub(crate) fn is_safe_native_id(native_id: &str) -> bool {
    !native_id.is_empty()
        && !native_id.contains('/')
        && !native_id.contains('\\')
        && !native_id.contains("..")
        && !native_id.contains('\0')
}

/// Build a `SourceFile` from a path and its metadata.
///
/// The mtime arithmetic is the reason this is a function: `mtime() * 1000 +
/// mtime_nsec() / 1_000_000` was written out in three adapters' `discover`
/// loops as well as here, and it is what the watcher compares to decide a file
/// changed. A copy that rounded differently would make one runtime's sessions
/// re-ingest on every poll, or never.
///
/// `native_id` is `impl Into<String>` so a caller that already owns the string
/// hands it over instead of paying for a clone — `discover` runs over every
/// session file on the machine (24,623 on this one), which is why the copies
/// were written inline in the first place.
pub(crate) fn source_file_at(
    path: PathBuf,
    native_id: impl Into<String>,
    meta: &std::fs::Metadata,
) -> SourceFile {
    use std::os::unix::fs::MetadataExt;
    SourceFile {
        path,
        inode: meta.ino() as i64,
        size: meta.len(),
        mtime_ms: meta.mtime() * 1000 + meta.mtime_nsec() / 1_000_000,
        native_id: native_id.into(),
    }
}

#[cfg(test)]
mod override_status_tests {
    use super::*;

    /// The three cases the report distinguishes — and the two it deliberately
    /// does not list: an added runtime is not an override, and an unparseable
    /// file is `load_configs`' problem.
    #[test]
    fn overrides_are_classified_against_the_embedded_text() {
        let dir = tempfile::tempdir().unwrap();
        let pi = decl::EMBEDDED.iter().find(|(id, _)| *id == "pi").unwrap().1;

        // Byte-identical copy — what install.sh used to leave behind.
        std::fs::write(dir.path().join("pi.toml"), pi).unwrap();
        // A real difference: one changed value. (The failure this whole check
        // exists for was exactly a value change that never took effect.)
        std::fs::write(
            dir.path().join("claude_code.toml"),
            decl::EMBEDDED[0].1.replace("subagents", "helpers"),
        )
        .unwrap();
        // An ADDED runtime: same engine, new id — listed nowhere.
        std::fs::write(
            dir.path().join("openclaw.toml"),
            pi.replace("id = \"pi\"", "id = \"openclaw\""),
        )
        .unwrap();
        // Garbage: load_configs warns about it; this report ignores it.
        std::fs::write(dir.path().join("broken.toml"), "not toml [[[").unwrap();

        let got = descriptor_overrides_in(dir.path());
        let mut brief: Vec<(&str, bool)> =
            got.iter().map(|o| (o.id.as_str(), o.identical)).collect();
        brief.sort();
        assert_eq!(brief, [("claude_code", false), ("pi", true)]);
        // The file, not just the id — "which file do I delete" is the question.
        assert!(got.iter().all(|o| o.path.exists()));
    }
}
