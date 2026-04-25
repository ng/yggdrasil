//! Secret redaction for node content, event payloads, and log snippets.
//!
//! See yggdrasil-18 / ADR (pending). Policy:
//!
//!   - Run **at write time** on all `NodeRepo::insert` paths so secrets
//!     never land in the DB.
//!   - Also run on event payload snippets as defense-in-depth.
//!   - Also run at display time on log snippets pulled from already-written
//!     rows (covers pre-migration data + handles pattern additions over
//!     time).
//!   - Kill switch via `YGG_REDACTION=off` for debugging.
//!   - Every redaction emits a `RedactionApplied` event so we can audit.
//!
//! Patterns are deliberately high-precision — false positives on real prose
//! would be worse than the occasional miss. Broader patterns (generic
//! password/token=value) are gated behind `YGG_REDACTION=strict`.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Single pattern + its redaction label. Label shows up in the replacement
/// string (`[redacted:<kind>]`) and in the event payload so analytics can
/// split by secret type.
struct Pattern {
    kind: &'static str,
    regex: &'static Lazy<Regex>,
}

// Each regex gets its own named static so it can be referenced from a
// const slice. Lazy<Regex> isn't usable in a const-array literal because
// of interior mutability, but `&'static Lazy<Regex>` is.
static RX_ANTHROPIC: Lazy<Regex> = Lazy::new(|| Regex::new(r"sk-ant-[A-Za-z0-9_\-]{20,}").unwrap());
static RX_OPENAI: Lazy<Regex> = Lazy::new(|| Regex::new(r"sk-[A-Za-z0-9]{20,}").unwrap());
static RX_AWS_ACCESS: Lazy<Regex> = Lazy::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").unwrap());
static RX_GITHUB: Lazy<Regex> = Lazy::new(|| Regex::new(r"gh[pousr]_[A-Za-z0-9]{36,}").unwrap());
static RX_SLACK: Lazy<Regex> = Lazy::new(|| Regex::new(r"xox[aboprs]-[A-Za-z0-9\-]{10,}").unwrap());
static RX_JWT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{5,}").unwrap()
});
static RX_PRIVATE_KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?s)-----BEGIN [A-Z ]+PRIVATE KEY-----.*?-----END [A-Z ]+PRIVATE KEY-----")
        .unwrap()
});
static RX_CRED_ASSIGN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(password|passwd|pwd|secret|api[_-]?key|token|auth)\s*[:=]\s*['"]?([A-Za-z0-9_\-\.=/+]{8,})['"]?"#,
    )
    .unwrap()
});

// PRECISION patterns — run by default. Order matters: more specific
// patterns first (Anthropic before OpenAI prefix overlap).
static PATTERNS_PRECISE: &[Pattern] = &[
    Pattern {
        kind: "anthropic_key",
        regex: &RX_ANTHROPIC,
    },
    Pattern {
        kind: "openai_key",
        regex: &RX_OPENAI,
    },
    Pattern {
        kind: "aws_access_key",
        regex: &RX_AWS_ACCESS,
    },
    Pattern {
        kind: "github_token",
        regex: &RX_GITHUB,
    },
    Pattern {
        kind: "slack_token",
        regex: &RX_SLACK,
    },
    Pattern {
        kind: "jwt",
        regex: &RX_JWT,
    },
    Pattern {
        kind: "private_key",
        regex: &RX_PRIVATE_KEY,
    },
];

// STRICT patterns — gated behind YGG_REDACTION=strict. More aggressive,
// more false positives, but catches env-var-style assignments.
static PATTERNS_STRICT: &[Pattern] = &[Pattern {
    kind: "credential_assignment",
    regex: &RX_CRED_ASSIGN,
}];

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RedactionResult {
    pub counts: HashMap<String, u32>,
    pub total: u32,
}

impl RedactionResult {
    pub fn is_clean(&self) -> bool {
        self.total == 0
    }
}

/// Check the runtime mode. Returns (enabled, strict).
fn mode() -> (bool, bool) {
    match std::env::var("YGG_REDACTION").ok().as_deref() {
        Some("off" | "0" | "false") => (false, false),
        Some("strict") => (true, true),
        _ => (true, false),
    }
}

/// Redact secrets from a string. Returns the redacted string and a report
/// of what was found. Falls through unchanged when `YGG_REDACTION=off`.
pub fn redact_str(input: &str) -> (String, RedactionResult) {
    let (enabled, strict) = mode();
    if !enabled {
        return (input.to_string(), RedactionResult::default());
    }

    let mut out = input.to_string();
    let mut result = RedactionResult::default();

    for p in PATTERNS_PRECISE {
        apply_pattern(&mut out, p, &mut result);
    }
    if strict {
        for p in PATTERNS_STRICT {
            apply_pattern(&mut out, p, &mut result);
        }
    }

    (out, result)
}

fn apply_pattern(text: &mut String, p: &Pattern, acc: &mut RedactionResult) {
    let count = p.regex.find_iter(text).count() as u32;
    if count == 0 {
        return;
    }
    *text = p
        .regex
        .replace_all(text, format!("[redacted:{}]", p.kind))
        .into_owned();
    *acc.counts.entry(p.kind.to_string()).or_insert(0) += count;
    acc.total += count;
}

/// Walk a JSON value, redacting every string leaf. Returns the redacted
/// value and an aggregated report. Used at node-content write time.
pub fn redact_json(mut value: serde_json::Value) -> (serde_json::Value, RedactionResult) {
    let mut acc = RedactionResult::default();
    redact_json_in_place(&mut value, &mut acc);
    (value, acc)
}

fn redact_json_in_place(v: &mut serde_json::Value, acc: &mut RedactionResult) {
    match v {
        serde_json::Value::String(s) => {
            let (new_s, r) = redact_str(s);
            if !r.is_clean() {
                *s = new_s;
                for (k, n) in r.counts {
                    *acc.counts.entry(k).or_insert(0) += n;
                }
                acc.total += r.total;
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                redact_json_in_place(item, acc);
            }
        }
        serde_json::Value::Object(obj) => {
            for (_, val) in obj.iter_mut() {
                redact_json_in_place(val, acc);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_openai_key() {
        let (out, r) = redact_str("key: sk-abc1234567890abcdef1234567890");
        assert!(out.contains("[redacted:openai_key]"));
        assert_eq!(r.total, 1);
    }

    #[test]
    fn catches_anthropic_key_before_openai() {
        let (out, r) = redact_str("key: sk-ant-abcdef1234567890abcdef");
        assert!(out.contains("[redacted:anthropic_key]"));
        assert_eq!(r.counts.get("anthropic_key"), Some(&1));
        assert_eq!(r.counts.get("openai_key"), None);
    }

    #[test]
    fn catches_aws_access_key() {
        let (out, r) = redact_str("my key is AKIAIOSFODNN7EXAMPLE");
        assert!(out.contains("[redacted:aws_access_key]"));
        assert_eq!(r.total, 1);
    }

    #[test]
    fn catches_github_token() {
        let (out, r) =
            redact_str("export GITHUB_TOKEN=ghp_0123456789abcdef0123456789abcdef01234567");
        assert!(out.contains("[redacted:github_token]"));
        assert_eq!(r.total, 1);
    }

    #[test]
    fn catches_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NSJ9.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let (out, r) = redact_str(jwt);
        assert!(out.contains("[redacted:jwt]"));
        assert_eq!(r.total, 1);
    }

    #[test]
    fn catches_private_key_multiline() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAy...\n-----END RSA PRIVATE KEY-----";
        let (out, r) = redact_str(pem);
        assert!(out.contains("[redacted:private_key]"));
        assert_eq!(r.total, 1);
    }

    #[test]
    fn leaves_prose_alone() {
        let (out, r) = redact_str("The user asked how to handle secrets safely.");
        assert_eq!(out, "The user asked how to handle secrets safely.");
        assert_eq!(r.total, 0);
    }

    #[test]
    fn walks_json_strings() {
        let v = serde_json::json!({
            "text": "key: sk-abc1234567890abcdef1234567890",
            "meta": { "nested": "AKIAIOSFODNN7EXAMPLE" },
            "tags": ["fine", "ghp_0123456789abcdef0123456789abcdef01234567"]
        });
        let (red, r) = redact_json(v);
        assert_eq!(r.total, 3);
        let s = red.to_string();
        assert!(s.contains("[redacted:openai_key]"));
        assert!(s.contains("[redacted:aws_access_key]"));
        assert!(s.contains("[redacted:github_token]"));
    }

    #[test]
    fn kill_switch_off() {
        // Can only verify the result type isn't populated when env var is set —
        // we don't test env mutations here to avoid test isolation issues.
        // Instead verify that a clean string passes through.
        let (out, r) = redact_str("nothing to see here");
        assert_eq!(out, "nothing to see here");
        assert!(r.is_clean());
    }

    #[test]
    fn strict_catches_env_assignment() {
        // Strict mode is env-gated; we test the strict pattern directly via
        // apply_pattern to avoid mutating env in tests.
        let mut text = "PASSWORD=hunter2longenough".to_string();
        let mut acc = RedactionResult::default();
        apply_pattern(&mut text, &PATTERNS_STRICT[0], &mut acc);
        assert!(text.contains("[redacted:credential_assignment]"));
    }
}
