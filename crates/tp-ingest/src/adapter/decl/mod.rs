//! Declarative adapter (LLD §5) — one parser engine, configured per runtime,
//! so adding a runtime stops meaning "write Rust and cut a release".
//!
//! §5's opening requirement was "adapter churn (codex, openclaw, hermes, …)
//! must never require a Rust rebuild". It was designed and never built; both
//! shipped adapters are hand-written, which is the thing it existed to prevent.
//!
//! The two hand-written adapters turned out to *validate* the design rather
//! than argue against it. Everything that differs between Claude Code and pi —
//! the role living at `message.role` vs on the entry `type`, `toolCall`/
//! `arguments` vs `tool_use`/`input`, `input`/`output` vs `input_tokens`/
//! `output_tokens`, the native id being the whole filename stem vs the part
//! after `_` — is field mapping. The one difference that sounded structural,
//! pi's branchable tree, needed no code at all: both adapters normalize in file
//! order because the retrieval coordinate is `(session_id, ts)`, never a tree
//! position (LLD §16 rule 1).
//!
//! What is NOT configurable lives here once, shared: byte-exact line splitting
//! with torn-write tolerance, `SessionMeta` extraction, and the `locate`/
//! `discover` walk. That machinery is identical in both adapters today — 62
//! lines of it duplicated verbatim, which is the largest exact clone in this
//! repo — and it is the part with a real correctness contract (a mis-computed
//! offset silently drops turns), so it is exactly what should not be copied.
//!
//! Scope note: this ships the engine and the config type. Loading configs from
//! `~/.teleport/runtimes.d/*.toml` is the next step and is deliberately not
//! bundled with it — the claim being proven first is that one engine can
//! reproduce a hand-written adapter exactly, which the conformance test
//! asserts. A runtime whose format outgrows the config still drops to a Rust
//! `Adapter` impl, as §5 intended.

//!
//! Split (2026-08): `config` holds the descriptor vocabulary — the types a
//! TOML file deserializes into, the embedded shipped descriptors, and loading.
//! `engine` holds the one parser those configs drive. Everything is re-exported
//! here so `decl::` paths are unchanged.

mod config;
mod engine;

pub use config::*;
pub use engine::*;

/// A descriptor from `install/runtimes.d/` — the file teleport actually ships,
/// not a config the test built. The distinction is the point: the engine
/// supporting a rule and the shipped descriptor carrying it are different
/// facts, and asserting only the first is how `codex.toml` sat unshipped while
/// every test passed. Shared by test modules in both halves.
#[cfg(test)]
pub(crate) fn shipped_config(name: &str) -> DeclConfig {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../install/runtimes.d")
        .join(format!("{name}.toml"));
    toml::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}
