pub mod adapter;
pub mod redact;

pub use adapter::{Adapter, SourceFile};

/// One built-in runtime's adapter, by id — the embedded shipped descriptor.
///
/// This is what tests reach for where they used to construct
/// `ClaudeCodeAdapter` / `PiAdapter`; those structs are gone, and with them the
/// ~900 lines of second implementation every new field had to be added to twice
/// and prove equivalent (`shipped_configs_match_their_builtin_adapters` — a test
/// whose whole job was policing a drift that can no longer be expressed).
pub fn builtin(id: &str) -> adapter::decl::DeclAdapter {
    let cfg = match id {
        "claude_code" => adapter::decl::DeclConfig::claude_code(),
        "pi" => adapter::decl::DeclConfig::pi(),
        "codex" => adapter::decl::DeclConfig::codex(),
        other => panic!("no built-in runtime {other:?}"),
    };
    adapter::decl::DeclAdapter::new(cfg)
}
