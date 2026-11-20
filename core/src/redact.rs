//! Best-effort removal of credentials from private text before it is stored.
//!
//! Private content is only visible to its owner, but it is synced to a
//! server; a leaked API key there cannot be taken back. This catches the
//! common, well-structured secret formats. It is not a guarantee: free-form
//! secrets (a password typed into a prompt) can still slip through.

use regex::{Captures, Regex};
use std::borrow::Cow;
use std::sync::LazyLock;

struct Rule {
    kind: &'static str,
    re: Regex,
    /// Capture group holding the secret; the rest of the match is kept.
    group: usize,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let rule = |kind, pattern: &str, group| Rule {
        kind,
        re: Regex::new(pattern).expect("valid redaction pattern"),
        group,
    };
    vec![
        rule(
            "private_key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)",
            0,
        ),
        rule("anthropic_key", r"\bsk-ant-[A-Za-z0-9_-]{16,}", 0),
        rule("openai_key", r"\bsk-(?:proj-)?[A-Za-z0-9_-]{20,}", 0),
        rule(
            "github_token",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{20,})",
            0,
        ),
        rule("gitlab_token", r"\bglpat-[A-Za-z0-9_-]{20,}", 0),
        rule("slack_token", r"\bxox[abposr]-[A-Za-z0-9-]{10,}", 0),
        rule("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", 0),
        rule("google_api_key", r"\bAIza[0-9A-Za-z_-]{35}", 0),
        rule("stripe_key", r"\b[rs]k_(?:live|test)_[0-9A-Za-z]{16,}", 0),
        rule(
            "jwt",
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
        ),
        rule("bearer", r"(?i)\bbearer\s+([A-Za-z0-9._~+/=-]{16,})", 1),
        // scheme://user:password@host
        rule(
            "url_password",
            r"\b[a-z][a-z0-9+.-]*://[^\s:/@]+:([^\s@/]+)@",
            1,
        ),
        // password=..., "api_key": "...", SECRET_TOKEN=...
        rule(
            "assignment",
            r#"(?i)\b[a-z0-9_]*(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|private[_-]?key|client[_-]?secret|credentials?)[a-z0-9_]*["']?\s*[:=]\s*["']?([^\s"'`,;&]{6,})"#,
            1,
        ),
    ]
});

/// Replace secrets in `text` with `[REDACTED:<kind>]`.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    for rule in RULES.iter() {
        if !rule.re.is_match(&out) {
            continue;
        }
        let replaced = rule
            .re
            .replace_all(&out, |caps: &Captures<'_>| {
                let whole = caps.get(0).expect("group 0 always matches");
                let Some(secret) = caps.get(rule.group) else {
                    return whole.as_str().to_string();
                };
                if secret.as_str().starts_with("[REDACTED") {
                    return whole.as_str().to_string();
                }
                let start = secret.start() - whole.start();
                let end = secret.end() - whole.start();
                let s = whole.as_str();
                format!("{}[REDACTED:{}]{}", &s[..start], rule.kind, &s[end..])
            })
            .into_owned();
        out = Cow::Owned(replaced);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_secret_formats() {
        let cases = [
            (
                "key sk-ant-api03-abcdefghijklmnopqrstuv end",
                "key [REDACTED:anthropic_key] end",
            ),
            (
                "export OPENAI=sk-proj-abcdefghijklmnopqrstuvwx",
                "export OPENAI=[REDACTED:openai_key]",
            ),
            (
                "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                "[REDACTED:github_token]",
            ),
            ("AKIAABCDEFGHIJKLMNOP", "[REDACTED:aws_access_key]"),
            (
                "Authorization: Bearer abcdefghijklmnop1234",
                "Authorization: Bearer [REDACTED:bearer]",
            ),
            (
                "postgres://admin:hunter22@db:5432/app",
                "postgres://admin:[REDACTED:url_password]@db:5432/app",
            ),
            (
                "DB_PASSWORD=supersecret1",
                "DB_PASSWORD=[REDACTED:assignment]",
            ),
            (
                r#"{"api_key": "abcdef123456"}"#,
                r#"{"api_key": "[REDACTED:assignment]"}"#,
            ),
        ];
        for (input, want) in cases {
            assert_eq!(redact(input), want, "{input}");
        }
    }

    #[test]
    fn redacts_private_key_blocks() {
        let text = "a\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\nb";
        assert_eq!(redact(text), "a\n[REDACTED:private_key]\nb");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for text in [
            "refactor watcher supaya baca realtime",
            "cargo test --workspace",
            "the token count is high",
            "see https://example.com/docs",
            "the password field is required",
        ] {
            assert_eq!(redact(text), text);
        }
    }
}
