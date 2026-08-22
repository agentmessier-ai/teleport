//! Runs BEFORE the writer, never after (LLD §6.3). Not optional, not a later
//! phase: `thinking` is exactly where a model tends to restate secrets
//! verbatim while reasoning about them, and `thinking` is the newly-exposed
//! surface here.

use once_cell::sync::Lazy;
use regex::Regex;
use tp_core::turn::NormalizedTurn;

struct RedactRule {
    pattern: Regex,
    replacement: &'static str,
}

static RULES: Lazy<Vec<RedactRule>> = Lazy::new(|| {
    let specs: &[(&str, &str)] = &[
        (r"AKIA[0-9A-Z]{16}", "[redacted:aws-access-key]"),
        (r"sk-ant-[A-Za-z0-9_-]{20,}", "[redacted:anthropic-key]"),
        (r"sk-[A-Za-z0-9]{20,}", "[redacted:api-key]"),
        (r"gh[pousr]_[A-Za-z0-9]{36,}", "[redacted:github-token]"),
        (
            r"(?i)bearer\s+[A-Za-z0-9\-_.]{20,}",
            "[redacted:bearer-token]",
        ),
        (
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            "[redacted:private-key]",
        ),
        // macOS `security find-generic-password -w` output shape (session's own credential leak class).
        (
            r#""accessToken"\s*:\s*"[^"]{10,}""#,
            "\"accessToken\":\"[redacted]\"",
        ),
        (
            r#""refreshToken"\s*:\s*"[^"]{10,}""#,
            "\"refreshToken\":\"[redacted]\"",
        ),
        // Stripe. NOT covered by the `sk-` rule above: Stripe separates with an
        // UNDERSCORE, that rule requires a hyphen, so every Stripe key was
        // passing through untouched (measured 2026-08-14). Restricted keys
        // (rk_) and webhook signing secrets (whsec_) share the shape.
        (
            r"\b(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{16,}",
            "[redacted:stripe-key]",
        ),
        (
            r"\bwhsec_[A-Za-z0-9]{16,}",
            "[redacted:stripe-webhook-secret]",
        ),
        // Connection strings. Only the PASSWORD is replaced — scheme, user and
        // host are what make a transcript useful to search later, and none of
        // them is the secret. Requires `:pass@` so a credential-less
        // `redis://host:6379` is left alone.
        (
            r"(?i)\b(postgres|postgresql|mysql|mongodb\+srv|mongodb|rediss|redis|amqps|amqp)://([^:@/\s]+):[^@/\s]+@",
            "${1}://${2}:[redacted]@",
        ),
        // JWTs — three base64url segments. `eyJ` is base64 for `{"`, so the
        // prefix is a strong signal of a JSON header rather than coincidence.
        // Supabase anon keys, session cookies and id_tokens all land here.
        (
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            "[redacted:jwt]",
        ),
        (r"\bAIza[0-9A-Za-z_-]{35}", "[redacted:google-api-key]"),
        (r"\bxox[baprs]-[A-Za-z0-9-]{10,}", "[redacted:slack-token]"),
        (r"\bnpm_[A-Za-z0-9]{36}", "[redacted:npm-token]"),
        (r"\bgithub_pat_[A-Za-z0-9_]{22,}", "[redacted:github-pat]"),
    ];
    specs
        .iter()
        .map(|(p, r)| RedactRule {
            pattern: Regex::new(p).expect("static redact pattern"),
            replacement: r,
        })
        .collect()
});

/// Public because the retrieval funnel (LLD §16 rule 2) applies it to every
/// provider's output, not just to turns on the ingest path.
pub fn scrub(s: &str) -> String {
    let mut out = s.to_string();
    for rule in RULES.iter() {
        if rule.pattern.is_match(&out) {
            out = rule
                .pattern
                .replace_all(&out, rule.replacement)
                .into_owned();
        }
    }
    out
}

pub fn redact(turn: &mut NormalizedTurn) {
    turn.text = scrub(&turn.text);
    turn.thinking = scrub(&turn.thinking);
    for tc in &mut turn.tool_calls {
        if let Some(d) = &tc.input_digest {
            tc.input_digest = Some(scrub(d));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_core::turn::Role;

    fn turn(text: &str, thinking: &str) -> NormalizedTurn {
        NormalizedTurn {
            role: Role::Assistant,
            ts: None,
            text: text.to_string(),
            thinking: thinking.to_string(),
            thinking_opaque: false,
            tool_calls: vec![],
            surface: Default::default(),
            tokens_in: None,
            tokens_out: None,
            prov: Default::default(),
        }
    }

    #[test]
    fn redacts_anthropic_key_in_thinking() {
        let mut t = turn(
            "",
            "the key is sk-ant-oat01-abcdefghijklmnopqrstuvwxyz123456",
        );
        redact(&mut t);
        assert!(!t.thinking.contains("sk-ant-oat01"));
        assert!(t.thinking.contains("[redacted:anthropic-key]"));
    }

    #[test]
    fn redacts_bearer_token() {
        let mut t = turn(
            "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc.def'",
            "",
        );
        redact(&mut t);
        assert!(!t.text.contains("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let mut t = turn(
            "just a normal sentence about the design",
            "reasoning about session ids",
        );
        let before = (t.text.clone(), t.thinking.clone());
        redact(&mut t);
        assert_eq!((t.text, t.thinking), before);
    }
    // ── Patterns added 2026-08-14 after measuring what got through ──────────
    // Each of these was verified to pass the pre-existing 8 rules untouched.
    // The Stripe one is the sharpest: `sk-[A-Za-z0-9]{20,}` requires a HYPHEN
    // and Stripe uses an UNDERSCORE, so the rule that looked like it covered
    // "sk keys" covered none of them.

    #[test]
    fn redacts_stripe_secret_key_underscore_form() {
        let mut t = turn("STRIPE_SECRET_KEY=sk_test_51QxAbCdEfGhIjKlMnOpQrStUv", "");
        redact(&mut t);
        assert!(!t.text.contains("51QxAbCdEfGhIjKlMnOpQrStUv"));
        assert!(t.text.contains("[redacted:stripe-key]"));
    }

    #[test]
    fn redacts_stripe_webhook_secret() {
        let mut t = turn("whsec_AbCdEfGhIjKlMnOpQrStUvWx1234", "");
        redact(&mut t);
        assert!(t.text.contains("[redacted:stripe-webhook-secret]"));
    }

    #[test]
    fn redacts_connection_string_password_but_keeps_the_rest() {
        let mut t = turn("postgres://agentnet:hunter2@10.42.0.1:5432/develop", "");
        redact(&mut t);
        assert!(!t.text.contains("hunter2"));
        // scheme, user, host and database survive — they are what makes the
        // transcript worth searching, and none of them is the secret.
        assert!(t
            .text
            .contains("postgres://agentnet:[redacted]@10.42.0.1:5432/develop"));
    }

    #[test]
    fn leaves_credential_less_connection_string_alone() {
        let mut t = turn("redis://10.42.0.1:6379", "");
        redact(&mut t);
        assert_eq!(t.text, "redis://10.42.0.1:6379");
    }

    #[test]
    fn redacts_jwt() {
        let mut t = turn(
            "anon key: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJyb2xlIjoiYW5vbiJ9.YZW-GDIN7q6372HaRCr",
            "",
        );
        redact(&mut t);
        assert!(!t.text.contains("eyJyb2xlIjoiYW5vbiJ9"));
        assert!(t.text.contains("[redacted:jwt]"));
    }

    #[test]
    fn redacts_google_slack_npm_and_github_pat() {
        let mut t = turn(
            "AIzaSyD-1234567890abcdefghijklmnopqrstu \
             xoxb-123456789012-1234567890123-AbCdEfGhIj \
             npm_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 \
             github_pat_11ABCDEFG0abcdefghijklmnop",
            "",
        );
        redact(&mut t);
        for leaked in [
            "AIzaSyD-1234567890abcdefghijklmnopqrstu",
            "xoxb-123456789012",
            "npm_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
            "github_pat_11ABCDEFG0abcdefghijklmnop",
        ] {
            assert!(!t.text.contains(leaked), "leaked: {leaked}");
        }
    }

    #[test]
    fn scrubbing_is_idempotent() {
        // The funnel applies scrub() on retrieval as well as ingest, so a
        // stored value gets scrubbed twice. A rule that matched its own
        // replacement text would corrupt it.
        let once = scrub("postgres://u:p@h/db sk_live_AbCdEfGhIjKlMnOpQr");
        assert_eq!(once, scrub(&once));
    }
}
