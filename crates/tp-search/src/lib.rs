//! Retrieval strategy selection (LLD §16). Callers depend on `Retrieval`, never
//! on a concrete provider.

pub mod index;
pub mod scan;

use anyhow::Result;
use tp_core::retrieval::{
    Capabilities, Hit, Query, RetrievalProvider, Retrieved, Scope, SessionRow, TurnCursor,
};
use tp_core::turn::NormalizedTurn;
use tp_core::SessionId;

pub use index::IndexProvider;
pub use scan::ScanProvider;

/// The public entry point. Its whole job is to be the *only* place a
/// provider's `RawHit` becomes a public `Hit` — which is what makes redaction
/// structurally unbypassable for every backend (LLD §16 rule 2).
pub struct Retrieval {
    provider: Box<dyn RetrievalProvider>,
}

impl Retrieval {
    pub fn new(provider: Box<dyn RetrievalProvider>) -> Self {
        Self { provider }
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    pub fn capabilities(&self) -> Capabilities {
        self.provider.capabilities()
    }

    /// Warning to surface when the request outruns what this provider does
    /// cheaply — so an expensive query is announced, never silently endured.
    pub fn scope_warning(&self, scope: &Scope) -> Option<String> {
        if !self.provider.capabilities().unscoped_ok && scope.is_broad() {
            Some(format!(
                "provider '{}' has no index: this scope may scan many files. \
                 Narrow with --folder/--since, or build an index with `tp index`.",
                self.provider.name()
            ))
        } else {
            None
        }
    }

    pub fn search(&self, q: &Query, scope: &Scope) -> Result<Retrieved<Hit>> {
        // Refuse rather than silently degrade: an FTS index cannot evaluate a
        // regex, and answering a regex query with a literal-phrase search
        // produces a confident, wrong "no matches".
        if q.regex && !self.provider.capabilities().regex {
            anyhow::bail!(
                "provider '{}' cannot evaluate regex queries — drop --regex, or omit --index to use the scan provider",
                self.provider.name()
            );
        }
        let raw = self.provider.search(q, scope)?;
        Ok(raw.map(|h| Hit::redacted_from(h, &|s: &str| tp_ingest::redact::scrub(s))))
    }

    pub fn sessions(&self, scope: &Scope, limit: usize) -> Result<Retrieved<SessionRow>> {
        let mut got = self.provider.sessions(scope, limit)?;
        // Titles are user prompt text and travel to peers — scrub them too.
        for row in &mut got.items {
            if let Some(t) = &row.title {
                row.title = Some(tp_ingest::redact::scrub(t));
            }
        }
        Ok(got)
    }

    /// `budget_bytes` defaults to `DEFAULT_TURN_BUDGET_BYTES` when `None`. A
    /// truncated read is resumable via the last item's `ts` as
    /// `TurnCursor::AfterTs`.
    pub fn turns(
        &self,
        session: &SessionId,
        at: TurnCursor,
        include_thinking: bool,
        limit: usize,
        budget_bytes: Option<usize>,
    ) -> Result<tp_core::Retrieved<NormalizedTurn>> {
        let budget = budget_bytes.unwrap_or(tp_core::retrieval::DEFAULT_TURN_BUDGET_BYTES);
        let mut got = self
            .provider
            .turns(session, at, include_thinking, limit, budget)?;
        for t in &mut got.items {
            tp_ingest::redact::redact(t);
        }
        Ok(got)
    }
}
