//! `ygg remember "..."` — persist a durable note scoped to the current repo
//! (or --global). Re-added after ADR 0015: this is a plain note store, NOT the
//! removed embedding/similarity corpus. Notes surface deterministically in
//! `ygg prime` (SessionStart) and `ygg remember --list`.

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Ordinary CLI adapter for a selected local OKF corpus. It neither creates a
/// corpus nor connects to PostgreSQL, including for agent/provenance lookups.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn remember_local(
    context: &crate::knowledge::runtime::Context,
    text: &str,
    global: bool,
    agent_name: &str,
    json: bool,
) -> anyhow::Result<()> {
    let text = text.trim();
    anyhow::ensure!(
        !text.is_empty(),
        "nothing to remember — pass a note, or use --list to read them"
    );
    let repo = if global {
        None
    } else {
        Some(context.writable_repo(&std::env::current_dir()?)?)
    };
    let doc = context.service.create_note(
        repo,
        text.into(),
        context.agent(agent_name),
        crate::knowledge::document::timestamp_now(),
    )?;
    let memory = crate::knowledge::legacy::note_json_model(&doc.document, &context.mappings)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&memory)?);
    } else if global {
        println!("Remembered · global (visible from every repo)");
    } else {
        println!("Remembered · scoped to this repo");
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn list_local(
    context: &crate::knowledge::runtime::Context,
    all: bool,
    limit: i64,
    json: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(limit >= 0, "note limit cannot be negative");
    let repo = if all {
        None
    } else {
        Some(context.repo(&std::env::current_dir()?)?)
    };
    let snapshot = context.service.notes(repo, all, limit.try_into()?)?;
    for diagnostic in snapshot.diagnostics {
        eprintln!("knowledge: {diagnostic}");
    }
    let rows = snapshot
        .documents
        .iter()
        .map(|d| crate::knowledge::legacy::note_json_model(&d.document, &context.mappings))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"count":rows.len(), "results":rows}))?
        );
    } else if rows.is_empty() {
        println!("No notes. Write one with `ygg remember \"...\"`.");
    } else {
        for m in rows {
            println!(
                "  · [{} · {}] {}",
                if m.repo_id.is_none() {
                    "global"
                } else {
                    "repo"
                },
                m.created_at.format("%Y-%m-%d"),
                short(&m.text, 120)
            );
        }
    }
    Ok(())
}
