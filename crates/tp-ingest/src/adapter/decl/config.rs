//! The descriptor vocabulary: what a `runtimes.d/*.toml` file can say, the
//! shipped descriptors embedded in the binary, and how user files are loaded.
//! The engine that ACTS on these lives in `engine.rs`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// How a session's native id is recovered from its filename stem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeIdRule {
    /// The stem IS the id (Claude Code: `<uuid>.jsonl`).
    Stem,
    /// Everything after the first `_` (pi: `<timestamp>_<uuid>.jsonl`).
    AfterUnderscore,
    /// The trailing UUID of a stem that also carries other dash-separated parts
    /// (codex: `rollout-2026-04-08T09-32-49-<uuid>.jsonl`). Neither rule above
    /// can reach it — the stem is not the id, and the timestamp contains the
    /// same `-` the UUID does, so no prefix split lands in the right place.
    TrailingUuid,
}

/// Where a record's role comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleSource {
    /// The entry's own `type` (Claude Code: `"user"` / `"assistant"`).
    EntryType,
    /// A field on the nested message (pi: `message.role`).
    MessageRole,
}

/// An entry type that is conversational but carries its text in a named field
/// instead of `content` — pi's `compaction` / `branch_summary`, whose `summary`
/// IS the distilled content of messages that are no longer replayed.
/// One or several accepted values, so a descriptor can say "either of these"
/// without the config growing a parallel plural field.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    pub(crate) fn matches(&self, s: &str) -> bool {
        match self {
            OneOrMany::One(v) => v == s,
            OneOrMany::Many(vs) => vs.iter().any(|v| v == s),
        }
    }
}

impl Default for OneOrMany {
    fn default() -> Self {
        OneOrMany::One(String::new())
    }
}

/// One `path == value` precondition on a record.
///
/// `deny_unknown_fields` on this and the other rule structs is not tidiness. A
/// bare TOML key written after a table header belongs to that TABLE, so a
/// root-level setting placed next to the rule it describes lands inside the rule
/// and — without this — is silently dropped. That happened with
/// `compaction_keeps_from`: pi's anchored boundary quietly degraded to positional
/// and reported 15, 43 and 68 live entries as superseded. Now it fails to load.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub path: String,
    pub equals: String,
}

fn default_type_path() -> String {
    "type".to_string()
}
fn default_role_path() -> String {
    "message.role".to_string()
}
fn default_cwd_path() -> String {
    "cwd".to_string()
}
fn default_dir_depth() -> usize {
    1
}

/// The trailing `8-4-4-4-12` UUID of a dash-separated stem, if there is one.
pub(crate) fn trailing_uuid(stem: &str) -> Option<&str> {
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    let tail = &parts[parts.len() - 5..];
    let shaped = [8usize, 4, 4, 4, 12]
        .iter()
        .zip(tail)
        .all(|(want, got)| got.len() == *want && got.chars().all(|c| c.is_ascii_hexdigit()));
    if !shaped {
        return None;
    }
    let start = stem.len() - (tail.iter().map(|p| p.len()).sum::<usize>() + 4);
    Some(&stem[start..])
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryRule {
    pub entry_type: String,
    /// "user" | "assistant"
    pub role: String,
    /// Path to the text, relative to the record.
    pub text_path: String,
}

/// An entry that names the SESSION instead of contributing a turn.
///
/// teleport used to have no concept of this and derived a title by truncating
/// the first user message — while every runtime teleport reads states one
/// somewhere. Claude Code emits two entry types, `custom-title` and `ai-title`
/// (16,839 of the latter on the machine this was written on, zero of the
/// former); pi versions one in a `session_info` entry. User beats AI, which
/// teleport does not need to be told: it resolves at read time in SQL, so a
/// rename arriving late wins without a rewrite.
///
/// A rule matches on the same `type_path` `entry_rules` uses, so a title entry
/// costs a stanza rather than a code change.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TitleRule {
    pub entry_type: String,
    /// Path to the title text, relative to the record.
    pub title_path: String,
    /// `"user"` (a person named it) or `"ai"` (a model did). Decides read
    /// precedence, so it is required rather than defaulted — a wrong guess
    /// silently outranks a real title.
    pub source: String,
}

impl TitleRule {
    pub(crate) fn title_source(&self) -> tp_core::turn::TitleSource {
        match self.source.as_str() {
            "ai" => tp_core::turn::TitleSource::Ai,
            // Anything else is treated as user-set, which is the SAFER default
            // for the read precedence: a title a person chose outranks one a
            // model wrote, so mistaking `ai` for `user` shows a real title too
            // prominently, while the reverse would hide it.
            _ => tp_core::turn::TitleSource::User,
        }
    }
}

/// A record whose payload is REASONING, not a message.
///
/// Claude Code and pi put thinking in a block INSIDE an assistant message, so
/// the normal path already carries it. codex emits it as its own
/// `response_item` with no `role` at all, so `role_source` drops it — and
/// dropping it is not merely lossy, it is WRONG: teleport then reports those
/// turns as having done no reasoning, when the file says otherwise.
///
/// Becomes a turn of its own with empty `text`. Merging it into the neighbouring
/// assistant message would be more faithful to teleport's one-thinking-column
/// model, but the adapter contract is per-line and stateless, and inventing
/// cross-line adjacency here to save a row is the kind of hidden state that
/// makes a resumed read disagree with a full one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningRule {
    pub entry_type: String,
    /// Where readable reasoning lives, if the record has any. Blocks are read
    /// the same way `content_path` reads message content.
    #[serde(default)]
    pub text_path: String,
    /// Which block type inside `text_path` carries the text (codex:
    /// `summary_text`). Empty means the path holds a plain string.
    #[serde(default)]
    pub block_type: String,
    /// A path whose PRESENCE means reasoning happened but cannot be read —
    /// codex's `encrypted_content`. Distinct from `text_path` being absent,
    /// which just means there was nothing.
    #[serde(default)]
    pub opaque_path: String,
}

/// Compose text from several paths, for shapes that keep no `content` at all.
/// pi's `bashExecution` is the motivating case: its searchable payload is the
/// command and its output, which the hand-written adapter renders as
/// `$ <command>\n<output>`. Applies only when the FIRST path is present, so it
/// can't manufacture text for unrelated records.
#[derive(Debug, Clone, Deserialize)]
pub struct TextJoin {
    #[serde(default)]
    pub prefix: String,
    pub paths: Vec<String>,
    #[serde(default)]
    pub sep: String,
}

/// What a harness can and cannot do, so core stops assuming.
///
/// Every field exists to remove one assumption that is currently hardcoded, and
/// each default reproduces today's behaviour exactly — a descriptor that omits
/// this section behaves as before (docs/reach-provider.md).
///
/// The assumptions being removed, each measured against a real breakage:
/// - `scannable`: `reconcile` deletes any row whose pid the process scan did not
///   find this cycle. A harness the scan cannot see (dsh's web profile has no
///   tty at all) has correct registrations pruned within one scan interval —
///   which already happened once, to pi.
/// - `multiplexed`: `reconcile` keeps exactly one row per pid. One dsh host
///   process serves many sessions, so N registrations would collapse to 1.
/// - `heartbeats`: whether the harness can renew its own presence. Claude Code
///   and pi cannot — a `SessionStart` hook fires once and there is no periodic
///   event — which is precisely why the process scan has to exist.
/// - `pane`: whether there is a tty to type the control string into. Without
///   one, a declared delivery channel is required rather than optional.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Capabilities {
    pub scannable: bool,
    pub multiplexed: bool,
    pub heartbeats: bool,
    pub pane: bool,
    /// How a scannable harness is recognized in `ps` output — substring match on
    /// the process's `comm`. Replaces the hardcoded chain in
    /// `tp-reach::discover::recognize_runtime`, and stops being a gate on
    /// whether a runtime may exist at all: a harness that registers itself needs
    /// no signature. `None` means "not discoverable without cooperation".
    pub process_match: Option<String>,
    /// The fixed phrase teleport types into this runtime's pane to make it
    /// drain its inbox. `None` uses the default `/tp inbox`.
    ///
    /// Declarable because it is a runtime's own vocabulary, not teleport's:
    /// Claude Code has a `/tp` slash command and pi has a `/tp` skill, so the
    /// default works there; codex has neither and answers `Unrecognized
    /// command '/tp'` — the wake arrives and dies at the last inch. Observed
    /// live, which is the only way it could have been found.
    ///
    /// This does NOT weaken LLD §7.3. The invariant is that nothing from the
    /// MESSAGE crosses a pane, not that the string is compiled in: this value
    /// comes from a descriptor the operator controls, is read once, and is
    /// never interpolated from any request field. A runtime that can be made to
    /// type something else is one whose descriptor was already rewritten, and
    /// that is game over for far more than this.
    pub control_string: Option<String>,
}

impl Default for Capabilities {
    /// Today's behaviour for a descriptor that declares nothing: a
    /// one-session-per-process, scannable, pane-injectable runtime that cannot
    /// heartbeat. That is Claude Code and pi exactly.
    fn default() -> Self {
        Self {
            scannable: true,
            multiplexed: false,
            heartbeats: false,
            pane: true,
            process_match: None,
            control_string: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeclConfig {
    pub id: String,
    /// Transcript root. `~` is expanded. Omitted falls back to the built-in
    /// default for this id, so a config need only override what differs.
    #[serde(default)]
    pub root: Option<String>,
    pub native_id: NativeIdRule,
    pub role_source: RoleSource,
    /// Source role value → normalized role. Anything unlisted is not a
    /// conversational turn and is skipped, which is how non-message records
    /// (queue-operation, model_change, …) fall out without special cases.
    pub user_roles: Vec<String>,
    pub assistant_roles: Vec<String>,
    /// Dotted paths, resolved against the raw record.
    pub ts_path: String,
    pub content_path: String,
    #[serde(default)]
    pub usage_in_path: String,
    #[serde(default)]
    pub usage_out_path: String,
    /// Block `type` discriminators and the field each carries.
    pub text_block: OneOrMany,
    #[serde(default)]
    pub thinking_block: OneOrMany,
    #[serde(default)]
    pub tool_block: String,
    #[serde(default)]
    pub tool_name_field: String,
    #[serde(default)]
    pub tool_input_field: String,
    /// Entry types outside the normal role mapping (see `EntryRule`).
    #[serde(default)]
    pub entry_rules: Vec<EntryRule>,
    /// Entry types that are REASONING records rather than messages (see
    /// `ReasoningRule`). codex is the motivating case: its reasoning is a
    /// separate `response_item` with no role, so the role mapping drops it —
    /// all 15 on this machine were invisible to teleport.
    #[serde(default)]
    pub reasoning_rules: Vec<ReasoningRule>,
    /// How to recognise a compaction boundary: ALL of these `path == value`
    /// must hold. Empty means this runtime's marker is unknown to teleport, and
    /// its turns are recorded as `unknown` rather than `current`.
    #[serde(default)]
    pub compaction_markers: Vec<Requirement>,
    /// Path to the id of the FIRST ENTRY KEPT by a compaction, when the runtime
    /// names one (pi's `firstKeptEntryId`). Empty means the marker's own position
    /// is the boundary, which is Claude Code's shape.
    #[serde(default)]
    pub compaction_keeps_from: String,
    /// Entry types that carry a session TITLE rather than a turn (see
    /// `TitleRule`). Declared rather than coded so a runtime whose title lives
    /// in its transcript costs a config stanza — pi's `session_info` is the next
    /// one, and it should not need Rust.
    #[serde(default)]
    pub title_rules: Vec<TitleRule>,
    /// Tried in order when `content` yields no text — for message shapes that
    /// keep their payload elsewhere (pi's `branchSummary` uses `summary`).
    #[serde(default)]
    pub text_fallbacks: Vec<String>,
    #[serde(default)]
    pub text_join: Option<TextJoin>,
    /// Dotted paths to source identity/lineage/cost (docs/data-model-v2.md).
    /// All optional — a format without a per-message id simply omits them and
    /// the turn keeps a `None`/`seq`-ordered `Provenance`. These are the LIVE
    /// path (runtimes.d overrides the built-in), so a field absent here is a
    /// field NOT captured, even if the built-in Rust adapter would have.
    #[serde(default)]
    pub uuid_path: Option<String>,
    #[serde(default)]
    pub parent_uuid_path: Option<String>,
    #[serde(default)]
    pub model_path: Option<String>,
    #[serde(default)]
    pub cache_read_path: Option<String>,
    #[serde(default)]
    pub cache_creation_path: Option<String>,
    /// Human-readable name for `tp live` / diagnostics. Falls back to `id`.
    #[serde(default)]
    pub name: Option<String>,
    /// Dotted path to the record's TYPE, which is what `entry_rules` and
    /// `role_source = "entry_type"` match against. Defaults to the top-level
    /// `type`, which is where the runtimes teleport shipped with put it; codex
    /// nests it one level down under `payload`.
    #[serde(default = "default_type_path")]
    pub type_path: String,
    /// Field holding a text block's text. Empty (the default) means "the same
    /// name as the block's type", which is true for both shipped runtimes —
    /// `{"type":"text","text":…}` — and is a coincidence, not a rule. codex
    /// tags the block `input_text`/`output_text` and still calls the field
    /// `text`.
    #[serde(default)]
    pub text_field: String,
    /// Same, for thinking blocks.
    #[serde(default)]
    pub thinking_field: String,
    /// Dotted path to the role, used when `role_source = "message_role"`.
    /// Defaults to `message.role`, where both shipped runtimes put it; codex
    /// puts it on `payload.role`. Ignored for `role_source = "entry_type"`,
    /// which reads `type_path` instead.
    #[serde(default = "default_role_path")]
    pub role_path: String,
    /// Extra path/value equalities a record must satisfy to be considered at
    /// all. codex needs this: `payload.type == "message"` alone is ambiguous
    /// because `event_msg` payloads carry their own `type`, so the outer
    /// `type == "response_item"` has to be required too.
    #[serde(default)]
    pub require: Vec<Requirement>,

    /// Path to a boolean saying this record belongs to a side conversation
    /// (Claude Code's `isSidechain`). Empty means the runtime has no such
    /// concept, which for pi and codex is the truth rather than a gap.
    #[serde(default)]
    pub sidechain_path: String,
    /// Dotted path to the session's working directory. Defaults to a top-level
    /// `cwd` on any record; codex records it once, on its `session_meta` line.
    #[serde(default = "default_cwd_path")]
    pub cwd_path: String,
    /// How many directory levels sit between the runtime root and a transcript.
    /// 1 for `root/<project>/<file>.jsonl`, which is what both shipped runtimes
    /// use; codex date-partitions its tree as `sessions/YYYY/MM/DD/<file>`.
    #[serde(default = "default_dir_depth")]
    pub dir_depth: usize,
    /// A directory NAME, at any depth, whose files are transcripts too.
    ///
    /// Claude Code needs this and nothing else does: a subagent's transcript goes
    /// to `<project>/<parent-session>/subagents/agent-*.jsonl`, two levels below a
    /// normal session, and a subagent that spawns its own nests again — 204 files
    /// at one extra level and 130 at two on this machine, 23,899 records that
    /// `dir_depth = 1` never saw.
    ///
    /// A NAME rather than "recurse into everything", which was the first attempt
    /// and was too wide: `subagents/workflows/wf_*/journal.jsonl` is not a
    /// transcript, and eight of them share the stem `journal`, so they collapsed
    /// onto one bogus session. Matching the immediate parent directory separates
    /// them exactly — a subagent transcript's parent is always `subagents`, a
    /// journal's is `wf_…`.
    ///
    /// A separate opt-in rather than making `dir_depth` a minimum, because that
    /// would silently change what codex and pi scan too.
    #[serde(default)]
    pub nested_dir: String,
    /// What this harness can and cannot do — see `Capabilities`.
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// The shipped descriptors, byte for byte, keyed by runtime id. This is the
/// text `install.sh` used to copy into `~/.teleport/runtimes.d/` — it no longer
/// does, because an installed copy outlives the binary that matched it and then
/// silently overrides a newer embedded one. That staleness cost two full debug
/// cycles in one session (a compaction fix and a sidechain fix both ran
/// "byte-identical wrong" against stale copies). Exposed so the staleness check
/// (`descriptor_overrides_in`) and `tp version` can compare an installed
/// override against exactly what this build carries.
pub const EMBEDDED: [(&str, &str); 3] = [
    (
        "claude_code",
        include_str!("../../../../../install/runtimes.d/claude_code.toml"),
    ),
    (
        "pi",
        include_str!("../../../../../install/runtimes.d/pi.toml"),
    ),
    (
        "codex",
        include_str!("../../../../../install/runtimes.d/codex.toml"),
    ),
];

impl DeclConfig {
    /// The shipped descriptors, compiled into the binary.
    ///
    /// `include_str!`, not a Rust restatement. These used to be hand-maintained
    /// `Self { … }` literals "kept in code so the conformance test can compare",
    /// and they did what every second copy does: drifted — the code copy of
    /// claude_code carried empty `title_rules` and `compaction_markers` while
    /// the shipped file had both, so a binary running without installed
    /// descriptors silently read no titles and tracked no compaction. Embedding
    /// the file makes the drift unrepresentable: there is one text per runtime,
    /// `install.sh` copies it out, and the binary carries it.
    ///
    /// The parse is unwrap-by-construction: the text is fixed at compile time
    /// and `embedded_descriptors_parse` fails the build's test run if any stops
    /// parsing.
    fn embedded(text: &'static str) -> Self {
        toml::from_str(text)
            .expect("embedded descriptor must parse — see embedded_descriptors_parse")
    }

    pub fn claude_code() -> Self {
        Self::embedded(EMBEDDED[0].1)
    }

    pub fn pi() -> Self {
        Self::embedded(EMBEDDED[1].1)
    }

    pub fn codex() -> Self {
        Self::embedded(EMBEDDED[2].1)
    }
}

pub fn config_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    Path::new(&home).join(".teleport").join("runtimes.d")
}

impl DeclConfig {
    /// Resolved transcript root: the config's own, `~`-expanded, else the
    /// built-in default for this id.
    pub fn resolved_root(&self) -> PathBuf {
        match &self.root {
            Some(r) => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
                PathBuf::from(r.replacen('~', &home, 1))
            }
            None => default_root_for(&self.id),
        }
    }
}

/// Load every `*.toml` in `dir`. A malformed file is SKIPPED WITH A WARNING,
/// never fatal: one bad user-supplied config must not stop teleport from
/// reading the runtimes that are fine — the same fault-isolation rule the
/// ingest pipeline follows (LLD §6.1).
pub fn load_configs(dir: &Path) -> Vec<DeclConfig> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|s| toml::from_str::<DeclConfig>(&s).map_err(|e| e.to_string()))
        {
            Ok(cfg) => out.push(cfg),
            Err(e) => tp_core::log_warn!("skipping runtime config {}: {e}", path.display()),
        }
    }
    out
}

pub fn default_root_for(id: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    match id {
        "pi" => Path::new(&home).join(".pi").join("agent").join("sessions"),
        _ => Path::new(&home).join(".claude").join("projects"),
    }
}

#[cfg(test)]
mod loading {
    use super::*;
    use crate::adapter::decl::DeclAdapter;
    use crate::adapter::Adapter;
    use tp_core::turn::Role;

    /// The point of §5: "add a runtime" is a file, not a release. A config
    /// loaded from a user directory drives the same engine the embedded ones
    /// do — this pins the loading path itself, with a minimal descriptor that
    /// exercises the defaults.
    #[test]
    fn a_toml_config_loaded_from_a_directory_parses_records() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("claude_code.toml"),
            r#"
id = "claude_code"
native_id = "stem"
role_source = "entry_type"
user_roles = ["user"]
assistant_roles = ["assistant"]
ts_path = "timestamp"
content_path = "message.content"
usage_in_path = "message.usage.input_tokens"
usage_out_path = "message.usage.output_tokens"
text_block = "text"
thinking_block = "thinking"
tool_block = "tool_use"
tool_name_field = "name"
tool_input_field = "input"
"#,
        )
        .unwrap();

        let cfgs = load_configs(dir.path());
        assert_eq!(cfgs.len(), 1, "the config must load");
        let decl = DeclAdapter::new(cfgs.into_iter().next().unwrap());

        let line = r#"{"type":"assistant","timestamp":"2026-08-03T23:30:44.000Z","message":{"content":[{"type":"thinking","thinking":"r"},{"type":"text","text":"v"}],"usage":{"input_tokens":12,"output_tokens":34}}}"#;
        let a = decl.parse_line(line).unwrap();
        assert_eq!(
            (
                a.role,
                a.text.as_str(),
                a.thinking.as_str(),
                a.tokens_in,
                a.tokens_out
            ),
            (Role::Assistant, "v", "r", Some(12), Some(34))
        );
        assert_eq!(decl.id(), "claude_code");
    }

    /// One broken config must not take the others down with it.
    #[test]
    fn a_malformed_config_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("broken.toml"),
            "id = \"x\"\nthis is not toml [[[",
        )
        .unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "not a config").unwrap();
        assert!(load_configs(dir.path()).is_empty());
        assert!(load_configs(Path::new("/no/such/dir")).is_empty());
    }
}

#[cfg(test)]
mod capabilities_tests {
    use super::*;

    /// The whole point of step 0: a descriptor that says nothing about itself
    /// must behave exactly as teleport did before capabilities existed. If this
    /// drifts, the two shipped TOMLs silently change meaning.
    #[test]
    fn omitting_the_section_reproduces_todays_behaviour() {
        let toml = r#"
            id = "x"
            native_id = "stem"
            role_source = "entry_type"
            user_roles = ["user"]
            assistant_roles = ["assistant"]
            ts_path = "timestamp"
            content_path = "message.content"
            usage_in_path = "a"
            usage_out_path = "b"
            text_block = "text"
            thinking_block = "thinking"
            tool_block = "tool_use"
            tool_name_field = "name"
            tool_input_field = "input"
        "#;
        let cfg: DeclConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.capabilities,
            Capabilities {
                scannable: true,
                multiplexed: false,
                heartbeats: false,
                pane: true,
                process_match: None,
                control_string: None,
            },
            "a descriptor with no [capabilities] must mean one-session-per-process, \
             scannable, pane-injectable, cannot heartbeat — i.e. Claude Code and pi"
        );
        assert_eq!(cfg.name, None, "name falls back to id at the Harness layer");
    }

    /// A GUI/multiplexed harness has to be able to say so — this is the shape
    /// dsh needs, and every field here disables one assumption core makes.
    #[test]
    fn a_multiplexed_harness_can_declare_itself() {
        let toml = r#"
            id = "dsh"
            name = "DeepSeek Harness"
            native_id = "stem"
            role_source = "entry_type"
            user_roles = ["user"]
            assistant_roles = ["assistant"]
            ts_path = "timestamp"
            content_path = "message.content"
            usage_in_path = "a"
            usage_out_path = "b"
            text_block = "text"
            thinking_block = "thinking"
            tool_block = "tool_use"
            tool_name_field = "name"
            tool_input_field = "input"

            [capabilities]
            scannable = false
            multiplexed = true
            heartbeats = true
            pane = false
        "#;
        let cfg: DeclConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("DeepSeek Harness"));
        let c = &cfg.capabilities;
        assert!(
            !c.scannable,
            "the process scan must not prune what it cannot see"
        );
        assert!(c.multiplexed, "one host process serves many sessions");
        assert!(c.heartbeats, "it renews its own presence");
        assert!(
            !c.pane,
            "no tty — a delivery channel is required, not optional"
        );
        assert_eq!(
            c.process_match, None,
            "a self-registering harness needs no signature"
        );
    }

    /// The shipped TOMLs must carry the same capabilities as the built-ins they
    /// override. They are the LIVE path, so a descriptor that omits or contradicts
    /// them silently changes behaviour for the runtime it replaces — the exact
    /// drift `shipped_configs` exists to catch on the parsing side.
    #[test]
    fn shipped_descriptors_declare_the_same_capabilities_as_the_builtins() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../install/runtimes.d");
        for (id, builtin) in [
            ("claude_code", DeclConfig::claude_code()),
            ("pi", DeclConfig::pi()),
        ] {
            let shipped = load_configs(&dir)
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no shipped descriptor for {id}"));
            assert_eq!(
                shipped.capabilities, builtin.capabilities,
                "[{id}] shipped descriptor's capabilities diverge from the built-in"
            );
            assert!(
                shipped.name.is_some(),
                "[{id}] shipped descriptor should name itself"
            );
        }
    }

    /// `process_match` is where `recognize_runtime`'s hardcoded chain is headed,
    /// so the two shipped runtimes must already carry the signatures it uses —
    /// substring for claude (a dev build named `claude-local` must match),
    /// anchored for pi (a substring would hit `pip`, `gpio-tool`, …).
    #[test]
    fn the_builtins_carry_the_signatures_recognize_runtime_hardcodes() {
        assert_eq!(
            DeclConfig::claude_code()
                .capabilities
                .process_match
                .as_deref(),
            Some("claude")
        );
        assert_eq!(
            DeclConfig::pi().capabilities.process_match.as_deref(),
            Some("=pi")
        );
    }
}

#[cfg(test)]
mod embedded_configs {
    use super::*;

    /// The build's guard on `DeclConfig::embedded`'s `expect`: every descriptor
    /// compiled into the binary must parse, and must carry the id its
    /// constructor claims. A file in install/runtimes.d that stops parsing
    /// fails HERE, not at first run on a user's machine.
    #[test]
    fn embedded_descriptors_parse() {
        for (cfg, id) in [
            (DeclConfig::claude_code(), "claude_code"),
            (DeclConfig::pi(), "pi"),
            (DeclConfig::codex(), "codex"),
        ] {
            assert_eq!(cfg.id, id);
            assert!(cfg.root.is_some(), "{id}: embedded descriptors carry roots");
        }
    }
}
