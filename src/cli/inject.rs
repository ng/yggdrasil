use crate::config::AppConfig;
use crate::embed::Embedder;
use crate::models::agent::AgentRepo;
use crate::models::event::{EventKind, EventRepo};
use crate::models::node::{NodeKind, NodeRepo};
use crate::lock::LockManager;

use tracing::{debug, info, warn};

/// Called by the UserPromptSubmit hook.
///
/// Flow:
///   1. If `prompt_text` is provided: embed it, write a UserMessage node, update head_node_id
///   2. Similarity-search across ALL agents for related past context
///   3. Surface high-similarity hits as `[ygg memory]` lines
///   4. Append active lock list
///
/// Returns nothing — output goes to stdout where the hook captures it for injection.
pub async fn execute(
    pool: &sqlx::PgPool,
    config: &AppConfig,
    agent_name: &str,
    prompt_text: Option<&str>,
) -> Result<(), anyhow::Error> {
    let agent_repo = AgentRepo::new(pool);
    let node_repo = NodeRepo::new(pool);
    let event_repo = EventRepo::new(pool);

    let agent = match agent_repo.get_by_name(agent_name).await? {
        Some(a) => a,
        None => {
            debug!("inject: agent '{}' not registered — skipping", agent_name);
            return Ok(());
        }
    };

    debug!(
        "inject: agent='{}' state={} tokens={} head_node={:?}",
        agent_name, agent.current_state, agent.context_tokens, agent.head_node_id
    );

    let mut output: Vec<String> = Vec::new();

    // ── context pressure warning ──────────────────────────────────────────────
    let pressure_pct = if config.context_limit_tokens > 0 {
        (agent.context_tokens as f64 / config.context_limit_tokens as f64 * 100.0) as u32
    } else {
        0
    };
    if pressure_pct > 75 {
        output.push(format!(
            "[ygg] Context pressure: {}% — digest will trigger at 100%",
            pressure_pct
        ));
    }

    // ── vector search ─────────────────────────────────────────────────────────
    if let Some(prompt) = prompt_text {
        let embedder = Embedder::default_ollama();
        let ollama_alive = embedder.health_check().await;
        debug!("inject: ollama health={}", ollama_alive);

        if ollama_alive {
            // Truncate prompt to ~1500 chars — all-minilm has a 256-token limit
            let query_text = if prompt.len() > 1500 { &prompt[..1500] } else { prompt };
            debug!("inject: embedding {} chars", query_text.len());

            let embed_start = std::time::Instant::now();
            let embed_result = embedder.embed_cached(pool, query_text).await;
            let embed_ms = embed_start.elapsed().as_millis() as u64;

            let cached = matches!(&embed_result, Ok((_, true)));
            let _ = event_repo.emit(
                if cached { EventKind::EmbeddingCacheHit } else { EventKind::EmbeddingCall },
                agent_name,
                Some(agent.agent_id),
                serde_json::json!({
                    "model": "all-minilm",
                    "input_chars": query_text.len(),
                    "latency_ms": embed_ms,
                    "success": embed_result.is_ok(),
                    "purpose": "prompt_embed",
                    "cached": cached,
                }),
            ).await;

            match embed_result {
                Err(e) => warn!("inject: embed failed ({embed_ms}ms): {e}"),
                Ok((query_vec, _was_cached)) => {
                    // Write this prompt as a UserMessage node and advance head_node_id
                    let node = node_repo.insert(
                        agent.head_node_id,
                        agent.agent_id,
                        NodeKind::UserMessage,
                        serde_json::json!({ "text": prompt }),
                        estimate_tokens(prompt),
                    ).await?;

                    node_repo.set_embedding(node.id, query_vec.clone()).await?;

                    let new_tokens = agent.context_tokens + node.token_count;
                    agent_repo.update_head(agent.agent_id, node.id, new_tokens).await?;

                    info!(
                        "inject: wrote node {} ({}tok), head advanced",
                        node.id, node.token_count
                    );

                    // Defense in depth: redact the event snippet too.
                    let (redacted_prompt, _) = crate::redaction::redact_str(prompt);
                    let snippet = if redacted_prompt.len() > 80 {
                        redacted_prompt[..80].to_string()
                    } else {
                        redacted_prompt.clone()
                    };
                    let snippet: &str = &snippet;
                    let _ = event_repo.emit(
                        EventKind::NodeWritten,
                        agent_name,
                        Some(agent.agent_id),
                        serde_json::json!({
                            "node_id": node.id,
                            "kind": "user_message",
                            "tokens": node.token_count,
                            "snippet": snippet,
                        }),
                    ).await;

                    // Hybrid retrieval (yggdrasil-8): union pgvector top-k
                    // with tsvector full-text top-k via reciprocal rank
                    // fusion. Falls back to vector-only if hybrid errors
                    // (e.g. pre-migration DB without content_tsv column).
                    let kinds = [NodeKind::UserMessage, NodeKind::Directive, NodeKind::Digest];
                    let hits = match node_repo
                        .hybrid_search_global(&query_vec, query_text, &kinds, 8, 0.6)
                        .await
                    {
                        Ok(h) => h,
                        Err(e) => {
                            debug!("inject: hybrid search failed ({e}), falling back to vector-only");
                            node_repo
                                .similarity_search_global(&query_vec, &kinds, 8, 0.6)
                                .await?
                        }
                    };

                    debug!("inject: global search returned {} hits", hits.len());

                    for hit in &hits {
                        debug!(
                            "inject: hit agent={} dist={:.3} sim={:.3} kind={:?}",
                            hit.agent_name, hit.distance, hit.similarity(), hit.kind
                        );
                    }

                    // Exclude the node we just wrote (distance ≈ 0), surface the rest
                    let mut candidates: Vec<crate::models::node::SearchHit> = hits.into_iter()
                        .filter(|h| h.id != node.id && h.distance > 0.01)
                        .collect();

                    // Disclosure gate — drop candidates we already surfaced
                    // to THIS agent recently. Cooldown is measured in TOKENS
                    // of context consumed since the last disclosure (matches
                    // the habituation principle in docs/design-principles.md).
                    // Approximated via cumulative node.token_count emitted
                    // after the hit. Env:
                    //   YGG_DISCLOSURE_COOLDOWN_TOKENS (default 4000 — ~1 user
                    //     prompt + 1 assistant turn at our typical turn sizes)
                    //   YGG_DISCLOSURE_COOLDOWN_SECS   (legacy time fallback)
                    //   YGG_DISCLOSURE_MODE = tokens|time|both (default both)
                    let cooldown_tokens: i64 = std::env::var("YGG_DISCLOSURE_COOLDOWN_TOKENS")
                        .ok().and_then(|v| v.parse().ok()).unwrap_or(4000);
                    let cooldown_secs: i64 = std::env::var("YGG_DISCLOSURE_COOLDOWN_SECS")
                        .ok().and_then(|v| v.parse().ok()).unwrap_or(1800);
                    let mode = std::env::var("YGG_DISCLOSURE_MODE").unwrap_or_else(|_| "both".into());

                    if !candidates.is_empty() && (cooldown_tokens > 0 || cooldown_secs > 0) {
                        let cand_ids: Vec<uuid::Uuid> = candidates.iter().map(|c| c.id).collect();

                        // Two conditions in one query: source_node_id was
                        // surfaced recently in time AND tokens-since-hit is
                        // below budget. Either gate alone suppresses depending
                        // on mode.
                        let sql = r#"
                            WITH cands AS (SELECT UNNEST($1::uuid[]) AS id),
                            hits AS (
                                SELECT (payload->>'source_node_id')::uuid AS src_id,
                                       MAX(created_at) AS last_hit_at
                                FROM events
                                WHERE event_kind::text = 'similarity_hit'
                                  AND agent_id = $2
                                  AND payload->>'source_node_id' IS NOT NULL
                                  AND (payload->>'source_node_id')::uuid = ANY($1)
                                GROUP BY src_id
                            ),
                            scored AS (
                                SELECT h.src_id,
                                       EXTRACT(EPOCH FROM (now() - h.last_hit_at))::bigint AS age_secs,
                                       COALESCE((
                                           SELECT SUM(token_count)::bigint FROM nodes n
                                           WHERE n.agent_id = $2 AND n.created_at > h.last_hit_at
                                       ), 0) AS tokens_since
                                FROM hits h
                            )
                            SELECT src_id FROM scored
                            WHERE ($3 = 'time'   AND age_secs   < $4)
                               OR ($3 = 'tokens' AND tokens_since < $5)
                               OR ($3 = 'both'   AND age_secs   < $4 AND tokens_since < $5)
                        "#;
                        let recent_ids: Vec<uuid::Uuid> = sqlx::query_scalar(sql)
                            .bind(&cand_ids)
                            .bind(agent.agent_id)
                            .bind(&mode)
                            .bind(cooldown_secs)
                            .bind(cooldown_tokens)
                            .fetch_all(pool)
                            .await
                            .unwrap_or_default();

                        if !recent_ids.is_empty() {
                            let before = candidates.len();
                            candidates.retain(|c| !recent_ids.contains(&c.id));
                            debug!(
                                "inject: disclosure gate ({mode}) suppressed {} candidate(s)",
                                before - candidates.len()
                            );
                        }
                    }

                    // Mechanical scoring — the primary precision mechanism.
                    // Rank by cosine × kind × age × repo × agent, soft-cap
                    // at max_hits, drop below floor. Bias is permissive:
                    // most things pass through, stronger signals rise.
                    let scorer = crate::scoring::Scorer::from_env();
                    // Best-effort repo_id lookup per candidate — nodes
                    // predating ADR 0009 have NULL repo_id, which maps to
                    // "neutral" in the scorer (no penalty).
                    let candidate_ids: Vec<uuid::Uuid> = candidates.iter().map(|h| h.id).collect();
                    let hit_repo_ids: Vec<Option<uuid::Uuid>> = if candidate_ids.is_empty() {
                        vec![]
                    } else {
                        let rows: Vec<(uuid::Uuid, Option<uuid::Uuid>)> = sqlx::query_as(
                            "SELECT id, repo_id FROM nodes WHERE id = ANY($1)"
                        )
                        .bind(&candidate_ids)
                        .fetch_all(pool)
                        .await
                        .unwrap_or_default();
                        candidate_ids.iter().map(|id| {
                            rows.iter().find(|(rid, _)| rid == id).and_then(|(_, repo)| *repo)
                        }).collect()
                    };
                    let current_repo_id = crate::cli::task_cmd::resolve_cwd_repo(pool).await
                        .ok()
                        .map(|r| r.repo_id);

                    let scored = scorer.score(
                        &candidates,
                        agent.agent_id,
                        current_repo_id,
                        &hit_repo_ids,
                        chrono::Utc::now(),
                    );

                    // Emit a scoring_decision event ONLY for dropped candidates —
                    // kept candidates get a similarity_hit further down, which
                    // carries the total score. Keeps ygg logs --follow readable
                    // while preserving the drop-reason breakdown that ygg eval
                    // needs.
                    for (hit, sd) in candidates.iter().zip(scored.iter()) {
                        if !sd.dropped { continue; }
                        let snippet = extract_snippet_around(&hit.content, Some(query_text));
                        let _ = event_repo.emit(
                            EventKind::ScoringDecision,
                            agent_name,
                            Some(agent.agent_id),
                            serde_json::json!({
                                "source_agent": hit.agent_name,
                                "kind": format!("{:?}", hit.kind).to_lowercase(),
                                "similarity": hit.similarity(),
                                "components": sd.scores,
                                "kept": false,
                                "drop_reason": sd.drop_reason,
                                "snippet": snippet,
                            }),
                        ).await;
                    }

                    // Optional LLM classifier overlay — only runs on the
                    // survivors. Default off; set YGG_CLASSIFIER=on.
                    let classifier = crate::classifier::Classifier::from_env();
                    let survivor_indices: Vec<usize> = scored.iter().enumerate()
                        .filter(|(_, s)| !s.dropped)
                        .map(|(i, _)| i)
                        .collect();
                    let survivor_snippets: Vec<String> = survivor_indices.iter()
                        .map(|&i| extract_snippet(&candidates[i].content))
                        .collect();
                    let classifier_decisions = if classifier.is_enabled() && !survivor_snippets.is_empty() {
                        let refs: Vec<&str> = survivor_snippets.iter().map(String::as_str).collect();
                        Some(classifier.classify_batch(prompt, &refs).await)
                    } else {
                        None
                    };

                    // Build sorted emission order: kept candidates by score descending.
                    let mut emit_order: Vec<usize> = survivor_indices.clone();
                    emit_order.sort_by(|&a, &b| {
                        scored[b].scores.total.partial_cmp(&scored[a].scores.total).unwrap_or(std::cmp::Ordering::Equal)
                    });

                    for (pos_in_survivors, &i) in emit_order.iter().enumerate() {
                        let hit = &candidates[i];
                        // If classifier is enabled and dropped this one, skip.
                        let classifier_kept = classifier_decisions.as_ref()
                            .and_then(|d| {
                                let j = survivor_indices.iter().position(|&k| k == i)?;
                                d.get(j).map(|dec| dec.kept || dec.bypassed)
                            })
                            .unwrap_or(true);
                        if !classifier_kept { continue; }

                        let age = format_age(hit.created_at);
                        let snippet = extract_snippet_around(&hit.content, Some(query_text));
                        let _ = pos_in_survivors; // reserved for future rank-aware logging
                        let _ = event_repo.emit(
                            EventKind::SimilarityHit,
                            agent_name,
                            Some(agent.agent_id),
                            serde_json::json!({
                                "source_agent": hit.agent_name,
                                "source_node_id": hit.id,
                                "distance": hit.distance,
                                "similarity": hit.similarity(),
                                "total_score": scored[i].scores.total,
                                "snippet": snippet,
                            }),
                        ).await;
                        output.push(format!(
                            "[{} · {} · {}] {}",
                            strength_label(scored[i].scores.total),
                            hit.agent_name,
                            age,
                            snippet,
                        ));
                    }
                }
            }
        } else {
            debug!("inject: ollama unavailable — vector search skipped");
        }
    } else {
        debug!("inject: no prompt text — vector search skipped");
    }

    // ── lock status ───────────────────────────────────────────────────────────
    let lock_mgr = LockManager::new(pool, config.lock_ttl_secs);
    let locks = lock_mgr.list_agent_locks(agent.agent_id).await?;
    if !locks.is_empty() {
        let lock_list: Vec<String> = locks.iter().map(|l| l.resource_key.clone()).collect();
        output.push(format!("[ygg locks] holding: {}", lock_list.join(", ")));
    }

    if !output.is_empty() {
        println!("{}", output.join("\n"));
    }

    // Epoch reflection check (yggdrasil-10). Fires a background digest
    // if transcript growth has crossed the threshold since the last one.
    // Non-blocking; returns instantly if nothing is due.
    crate::epoch::maybe_fire(pool, agent.agent_id, agent_name).await;

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn estimate_tokens(text: &str) -> i32 {
    // Rough approximation: ~4 chars per token
    (text.len() / 4).max(1) as i32
}

fn extract_snippet(content: &serde_json::Value) -> String {
    extract_snippet_around(content, None)
}

/// Pick a snippet from node content that's most informative for the user.
/// When `query` is provided, we centre the window on the first query-token
/// match (query-centered snippet — yggdrasil-15); otherwise we take the
/// head of the string.
fn extract_snippet_around(content: &serde_json::Value, query: Option<&str>) -> String {
    let text = content.get("text")
        .or_else(|| content.get("directive"))
        .or_else(|| content.get("summary"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| content.as_str().unwrap_or("(no text)"));

    const WINDOW: usize = 120;

    if text.len() <= WINDOW {
        return text.to_string();
    }

    if let Some(q) = query {
        // Find the earliest char-offset match of any query word (>= 3 chars,
        // not a stopword). Case-insensitive.
        let hay_lower = text.to_lowercase();
        let mut best: Option<usize> = None;
        for word in q.split(|c: char| !c.is_alphanumeric()) {
            if word.len() < 4 { continue; }
            let w = word.to_lowercase();
            if matches!(w.as_str(),
                "what" | "when" | "where" | "which" | "with" | "from" | "does" | "this" | "that" | "have" | "will"
            ) { continue; }
            if let Some(pos) = hay_lower.find(&w) {
                best = Some(best.map(|b| b.min(pos)).unwrap_or(pos));
            }
        }
        if let Some(pos) = best {
            // Center a WINDOW-size slice around pos, snap to char boundaries.
            let half = WINDOW / 2;
            let start = pos.saturating_sub(half);
            let end = (start + WINDOW).min(text.len());
            // Adjust start/end to UTF-8 char boundaries.
            let start = (0..=start).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
            let end = (end..=text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
            let prefix = if start > 0 { "…" } else { "" };
            let suffix = if end < text.len() { "…" } else { "" };
            return format!("{prefix}{}{suffix}", &text[start..end]);
        }
    }

    // Fallback: head of the string.
    let cut = (0..=117).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
    format!("{}…", &text[..cut])
}

/// Map a mechanical total score to a human-readable strength band.
/// Labels chosen to make "why was this surfaced?" legible without requiring
/// the reader to know the cosine distribution or weight tuning.
fn strength_label(total: f64) -> &'static str {
    if total >= 0.6       { "strong recall" }
    else if total >= 0.3  { "recall" }
    else                  { "faint recall" }
}

fn format_age(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - ts).num_seconds();
    if secs < 60 { return format!("{secs}s ago"); }
    let mins = secs / 60;
    if mins < 60 { return format!("{mins}m ago"); }
    let hours = mins / 60;
    if hours < 24 { return format!("{hours}h ago"); }
    format!("{}d ago", hours / 24)
}
