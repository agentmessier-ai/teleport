//! Wire addressing: `session.id` is the composite string `<machine_id>/<runtime_id>/<native_id>`
//! (LLD §4). It is the PK, not a surrogate — peers refer to sessions by this string directly.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId {
    pub machine_id: String,
    pub runtime_id: String,
    pub native_id: String,
}

impl SessionId {
    pub fn new(
        machine_id: impl Into<String>,
        runtime_id: impl Into<String>,
        native_id: impl Into<String>,
    ) -> Self {
        Self {
            machine_id: machine_id.into(),
            runtime_id: runtime_id.into(),
            native_id: native_id.into(),
        }
    }

    /// Parse `"<machine_id>/<runtime_id>/<native_id>"`. `native_id` may itself contain
    /// `/` (paths, nested ids) so we split on the first two separators only.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.splitn(3, '/');
        let machine_id = parts.next()?.to_string();
        let runtime_id = parts.next()?.to_string();
        let native_id = parts.next()?.to_string();
        if machine_id.is_empty() || runtime_id.is_empty() || native_id.is_empty() {
            return None;
        }
        Some(Self {
            machine_id,
            runtime_id,
            native_id,
        })
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}",
            self.machine_id, self.runtime_id, self.native_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let id = SessionId::new("m1", "claude_code", "abc-123");
        assert_eq!(SessionId::parse(&id.to_string()).unwrap(), id);
    }

    #[test]
    fn native_id_may_contain_slashes() {
        let id = SessionId::parse("m1/claude_code/nested/path/id").unwrap();
        assert_eq!(id.native_id, "nested/path/id");
    }

    #[test]
    fn rejects_short_input() {
        assert!(SessionId::parse("only-one-part").is_none());
        assert!(SessionId::parse("two/parts").is_none());
    }

    #[test]
    fn rejects_empty_segment() {
        assert!(SessionId::parse("/claude_code/abc").is_none());
    }
}
