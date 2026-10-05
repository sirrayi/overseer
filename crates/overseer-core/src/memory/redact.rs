//! Write-time secret scrub for memory text (episode prompts, `remember`,
//! distilled notes): one pass over a fixed set of secret shapes, each
//! match → `[redacted:<kind>]`. Narrower than `cred::scan` on purpose —
//! no entropy heuristic, so paths and hashes in notes survive.

use regex_lite::{Captures, Regex};
use std::borrow::Cow;
use std::sync::OnceLock;

const PLACEHOLDER: &str = "[redacted:";

fn patterns() -> &'static [(Regex, &'static str)] {
    static SET: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            (
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
                "private-key",
            ),
            (r"AKIA[0-9A-Z]{16}", "aws-key"),
            (r"gh[pousr]_[A-Za-z0-9]{36,}", "github-token"),
            (r"xox[baprs]-\S+", "slack-token"),
            (r"sk-[A-Za-z0-9_-]{20,}", "api-key"),
            (r"Bearer \S{20,}", "bearer-token"),
            (
                r"(?i)(?:api[_-]?key|token|secret|password)\s*[:=]\s*\S{8,}",
                "secret",
            ),
        ]
        .into_iter()
        .map(|(re, kind)| (Regex::new(re).expect("static pattern"), kind))
        .collect()
    })
}

/// `text` with every secret-shaped span replaced by `[redacted:<kind>]`.
/// Idempotent: a span already holding a placeholder is left alone.
pub fn scrub(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    for (re, kind) in patterns() {
        if !re.is_match(&out) {
            continue;
        }
        let next = re
            .replace_all(&out, |c: &Captures| {
                let m = &c[0];
                if m.contains(PLACEHOLDER) {
                    m.to_string()
                } else {
                    format!("{PLACEHOLDER}{kind}]")
                }
            })
            .into_owned();
        out = Cow::Owned(next);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str, kind: &str) {
        let got = scrub(text);
        assert!(
            got.contains(&format!("[redacted:{kind}]")),
            "{text:?} → {got:?}"
        );
        assert!(got.starts_with("use "), "{got:?}");
        assert!(got.ends_with(" now"), "{got:?}");
    }

    #[test]
    fn every_shape_is_redacted_with_its_kind() {
        one("use sk-proj_abcDEF0123456789xyzXYZ now", "api-key");
        one("use AKIAIOSFODNN7ABCDEFG now", "aws-key");
        one(
            "use -----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY----- now",
            "private-key",
        );
        one(
            "use Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload now",
            "bearer-token",
        );
        one(
            &format!("use ghp_{} now", "a1B2c3D4e5".repeat(4)),
            "github-token",
        );
        one("use xoxb-1234-5678-abcdef now", "slack-token");
        one("use API_KEY=abcd1234efgh now", "secret");
        one("use api-key: q9w8e7r6t5 now", "secret");
        one("use password = hunter2hunter2 now", "secret");
        one("use Token:0123456789 now", "secret");
    }

    #[test]
    fn secret_bodies_never_survive() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n";
        assert_eq!(scrub(pem), "[redacted:private-key]");
        let s = scrub("Bearer ghp_0123456789abcdefghij0123456789abcdefgh");
        assert!(!s.contains("ghp_"), "{s}");
        assert_eq!(scrub(&s), s, "idempotent");
    }

    #[test]
    fn ordinary_prose_is_untouched() {
        for prose in [
            "Fix the token budget: the resident segment is capped at 4,000 chars.",
            "See crates/overseer-core/src/memory/index.rs at a522456b1ce7c84d6e383671ea05dce7f26a90d1.",
            "The password reset flow asks for a secret question; skip sk-short keys.",
            "Bearer tokens go in the Authorization header. AKIA prefixes mark AWS keys.",
        ] {
            assert!(matches!(scrub(prose), Cow::Borrowed(_)), "{prose}");
        }
    }
}
