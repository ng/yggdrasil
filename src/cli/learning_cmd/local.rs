//! Offline adapters for the ordinary learn command contract.
use crate::{
    knowledge::{
        legacy::{self, Usage},
        matching::Filters,
        runtime::Context,
        service::{Creation, RuleInput},
        store::{Kind, Snapshot},
    },
    models::learning::Learning,
};
use anyhow::{Result, anyhow, ensure};
use uuid::Uuid;

pub fn create(
    context: &Context,
    mut input: RuleInput,
    global: bool,
    agent: Option<&str>,
    creation: Creation,
    json: bool,
) -> Result<()> {
    input.repo = if global {
        None
    } else {
        Some(context.writable_repo(&std::env::current_dir()?)?)
    };
    input.created_by = context.agent(agent.unwrap_or(&context.default_agent_name));
    let doc = context.service.create_rule(
        input,
        creation,
        crate::knowledge::document::timestamp_now(),
    )?;
    // A newly created rule has never been injected. Telemetry outages cannot
    // turn this acknowledged write into a reported failure.
    let usage = Usage {
        corpus_id: context.mappings.corpus_id,
        document_id: doc.key.id,
        applied_count: 0,
        last_applied_at: None,
    };
    let row = legacy::learning_json_model(&doc.document, &usage, &context.mappings)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&row)?);
        return Ok(());
    }
    let scope = super::format_scope_label(
        row.repo_id,
        row.file_glob.as_deref(),
        row.rule_id.as_deref(),
        &row.scope_tags,
    );
    if row.status == "pending" {
        println!(
            "{} [{}] {}\n  id {} — approve with `ygg learn approve {}`",
            if row.source == "proposed" {
                "Proposed"
            } else {
                "Pending"
            },
            scope,
            super::short(&row.text, 100),
            row.learning_id,
            row.learning_id
        );
    } else {
        println!("Learned [{}] {}", scope, super::short(&row.text, 100));
    }
    Ok(())
}

pub fn list(
    context: &Context,
    file: Option<&str>,
    rule: Option<&str>,
    all: bool,
    pending: bool,
    json: bool,
) -> Result<()> {
    let repo = if all {
        None
    } else {
        Some(context.repo(&std::env::current_dir()?)?)
    };
    let snapshot = if pending {
        context.service.pending(repo)?
    } else {
        context.service.list_rules(&Filters {
            repo,
            file,
            rule,
            ..Filters::default()
        })?
    };
    render(context, snapshot, pending, json)
}
fn render(context: &Context, snapshot: Snapshot, pending: bool, json: bool) -> Result<()> {
    for diagnostic in snapshot.diagnostics {
        eprintln!("knowledge: {diagnostic}");
    }
    let usage = context.usage_snapshot()?;
    let rows = snapshot
        .documents
        .iter()
        .map(|doc| {
            legacy::learning_json_model(
                &doc.document,
                &usage.for_document(&doc.document)?,
                &context.mappings,
            )
        })
        .collect::<Result<Vec<Learning>>>()?;
    if !rows.is_empty() {
        eprintln!("knowledge: usage totals are last-known local values");
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "count": rows.len(), "results": rows })
            )?
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "{}",
            if pending {
                "No pending learnings."
            } else {
                "No learnings match."
            }
        );
        return Ok(());
    }
    for row in &rows {
        let scope = super::format_scope_label(
            row.repo_id,
            row.file_glob.as_deref(),
            row.rule_id.as_deref(),
            &row.scope_tags,
        );
        if pending {
            println!(
                "  · {} [{} · {}] {}",
                row.learning_id,
                scope,
                row.source,
                super::short(&row.text, 100)
            );
        } else {
            let applied = if row.applied_count > 0 {
                let last = row
                    .last_applied_at
                    .map(|at| format!(", last {}", at.format("%Y-%m-%d")))
                    .unwrap_or_default();
                format!(" [×{}{last}]", row.applied_count)
            } else {
                String::new()
            };
            println!("  · [{scope}]{applied} {}", super::short(&row.text, 100));
        }
    }
    if pending {
        println!("\napprove: `ygg learn approve <id>` · reject: `ygg learn reject <id>`");
    }
    Ok(())
}
fn learning(context: &Context, id: Uuid) -> Result<crate::knowledge::store::RevisionedDocument> {
    let doc = context
        .service
        .get(id)?
        .ok_or_else(|| anyhow!("no learning with id {id}"))?;
    ensure!(
        doc.key.kind == Kind::Learning,
        "ID identifies a note, not a learning"
    );
    Ok(doc)
}
pub fn approve(context: &Context, id: Uuid, agent: Option<&str>) -> Result<()> {
    let doc = learning(context, id)?;
    let updated = context.service.approve(
        id,
        &doc.revision,
        context.approver(agent)?,
        crate::knowledge::document::timestamp_now(),
    )?;
    println!(
        "approved {id} → active {}",
        super::short(&updated.document.body, 80)
    );
    Ok(())
}
pub fn reject(context: &Context, id: Uuid, reason: Option<&str>) -> Result<()> {
    let doc = learning(context, id)?;
    context.service.reject(id, &doc.revision)?;
    match reason {
        Some(reason) => println!("rejected {id} — {reason}"),
        None => println!("rejected {id}"),
    }
    Ok(())
}
pub fn delete(context: &Context, id: Uuid) -> Result<()> {
    if context.service.get(id)?.is_some() {
        let doc = learning(context, id)?;
        context.service.delete(id, &doc.revision)?;
    }
    println!("deleted {id}");
    Ok(())
}

pub fn surface_for_edit(
    context: &Context,
    file: &str,
    agent: &str,
    session: &str,
) -> Result<Emission> {
    format_documents(crate::knowledge::injection::for_edit(
        context, file, agent, session,
    )?)
}

pub fn surface_for_files(
    context: &Context,
    repo: Uuid,
    files: &[String],
    agent: &str,
    kind: &str,
) -> Result<Emission> {
    format_documents(crate::knowledge::injection::for_task(
        context, repo, files, agent, kind,
    )?)
}

pub struct Emission {
    pub lines: Vec<String>,
    pub applications: Vec<crate::knowledge::telemetry::Application>,
}

fn format_documents(
    documents: Vec<crate::knowledge::store::RevisionedDocument>,
) -> Result<Emission> {
    let applications = documents
        .iter()
        .map(|doc| crate::knowledge::telemetry::Application {
            document: doc.key.id,
            application: Uuid::new_v4(),
            at: chrono::Utc::now(),
            // Malformed provenance cannot justify caching a fabricated zero baseline.
            imported: legacy::is_imported(&doc.document).unwrap_or(true),
        })
        .collect();
    let lines = documents
        .into_iter()
        .map(|doc| {
            let p = doc
                .document
                .profile()?
                .ok_or_else(|| anyhow!("rule profile required"))?;
            let scope = super::format_scope_label(
                p.repo,
                p.file_glob.as_deref(),
                p.rule_id.as_deref(),
                &serde_json::to_value(&p.scope_tags)?,
            );
            Ok(format!(
                "[ygg learning · {scope}] {}",
                super::short(&doc.document.body, 200)
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Emission {
        lines,
        applications,
    })
}
