//! Auto-classify task titles into (kind, priority, labels) via a small
//! Ollama call. See yggdrasil-6. Used by `ygg task create` to suggest
//! defaults; explicit user-provided flags always win.
//!
//! Fails silently — if the model is down, times out, or emits garbage,
//! we return None and the caller falls back to the CLI defaults.

use reqwest;
use serde::{Deserialize, Serialize};
use tracing::debug;

const DEFAULT_MODEL: &str = "llama3.2:1b";
const TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Clone, Default)]
pub struct Suggestion {
    pub kind: Option<String>,       // "task"|"bug"|"feature"|"chore"|"epic"
    pub priority: Option<i16>,      // 0..=4
    pub labels: Vec<String>,
}

pub async fn suggest(title: &str, description: Option<&str>) -> Option<Suggestion> {
    if std::env::var("YGG_TASK_CLASSIFY").ok().as_deref() == Some("off") {
        return None;
    }
    let base_url = std::env::var("OLLAMA_BASE_URL")
        .unwrap_or_else(|_| "http://localhost:11434".into());
    let model = std::env::var("YGG_TASK_CLASSIFY_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());

    let body = format!(
        "Classify this software engineering task. Respond with JSON only:\n\
         {{\"kind\": \"task|bug|feature|chore|epic\",\n\
          \"priority\": 0-4,\n\
          \"labels\": [up to 4 short topic tags like \"auth\", \"migrations\", \"retrieval\", \"security\", \"cli\"]}}\n\n\
         Priority guide: 0=critical outage, 1=blocks a release, 2=normal, 3=nice-to-have, 4=backlog.\n\
         Kind guide: bug=something is broken, feature=new capability, chore=cleanup/infra, epic=multi-task work.\n\n\
         Title: {title}\n\
         Description: {}\n\n\
         JSON:",
        description.unwrap_or("")
    );

    #[derive(Serialize)]
    struct Req<'a> {
        model: &'a str,
        prompt: String,
        format: &'static str,
        stream: bool,
        options: Opts,
        keep_alive: &'a str,
    }
    #[derive(Serialize)]
    struct Opts { temperature: f32, num_predict: u32 }
    #[derive(Deserialize)]
    struct Resp { response: String }

    let req = Req {
        model: &model,
        prompt: body,
        format: "json",
        stream: false,
        options: Opts { temperature: 0.0, num_predict: 120 },
        keep_alive: "30m",
    };

    let http = reqwest::Client::new();
    let fut = async {
        let resp = http.post(format!("{}/api/generate", base_url.trim_end_matches('/')))
            .json(&req).send().await.ok()?;
        if !resp.status().is_success() { return None; }
        resp.json::<Resp>().await.ok().map(|r| r.response)
    };

    let raw = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MS),
        fut,
    ).await.ok().flatten()?;

    parse_suggestion(&raw)
}

fn parse_suggestion(raw: &str) -> Option<Suggestion> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let obj = v.as_object()?;
    let mut out = Suggestion::default();

    if let Some(k) = obj.get("kind").and_then(|v| v.as_str()) {
        let k = k.trim().to_lowercase();
        if matches!(k.as_str(), "task" | "bug" | "feature" | "chore" | "epic") {
            out.kind = Some(k);
        }
    }

    if let Some(p) = obj.get("priority").and_then(|v| v.as_i64()) {
        if (0..=4).contains(&p) {
            out.priority = Some(p as i16);
        }
    } else if let Some(p_str) = obj.get("priority").and_then(|v| v.as_str()) {
        // Handle "P2" or "2" strings.
        let trimmed = p_str.trim().trim_start_matches(['P', 'p']);
        if let Ok(n) = trimmed.parse::<i16>() {
            if (0..=4).contains(&n) { out.priority = Some(n); }
        }
    }

    if let Some(arr) = obj.get("labels").and_then(|v| v.as_array()) {
        for l in arr {
            if let Some(s) = l.as_str() {
                let clean = s.trim().to_lowercase();
                if !clean.is_empty() && clean.len() < 32 && out.labels.len() < 4 {
                    out.labels.push(clean);
                }
            }
        }
    }

    // Don't return Some if nothing was extracted.
    if out.kind.is_none() && out.priority.is_none() && out.labels.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_shape() {
        let raw = r#"{"kind":"bug","priority":1,"labels":["auth","migrations"]}"#;
        let s = parse_suggestion(raw).unwrap();
        assert_eq!(s.kind, Some("bug".into()));
        assert_eq!(s.priority, Some(1));
        assert_eq!(s.labels, vec!["auth".to_string(), "migrations".into()]);
    }

    #[test]
    fn parses_p_prefix_priority() {
        let raw = r#"{"kind":"feature","priority":"P2","labels":["retrieval"]}"#;
        let s = parse_suggestion(raw).unwrap();
        assert_eq!(s.priority, Some(2));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_suggestion("not json").is_none());
    }

    #[test]
    fn drops_invalid_kind() {
        let raw = r#"{"kind":"whatever","priority":2,"labels":["x"]}"#;
        let s = parse_suggestion(raw).unwrap();
        assert_eq!(s.kind, None);
        assert_eq!(s.priority, Some(2));
    }
}
