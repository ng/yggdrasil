use anyhow::{Result, ensure};
use std::{io::Read, path::Path};

pub async fn dry_run(mapping_file: Option<&Path>, json: bool) -> Result<()> {
    let mappings = mapping_file
        .map(|path| -> Result<crate::knowledge::legacy::Mappings> {
            let file = std::fs::File::open(path)?;
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 1024 * 1024,
                "mapping file exceeds 1 MiB limit"
            );
            Ok(serde_json::from_slice(&bytes)?)
        })
        .transpose()?;
    let config = crate::config::AppConfig::from_env()?;
    let pool = crate::db::connect(&config.database).await?;
    let report = crate::knowledge::inventory::assess(&pool, mappings.as_ref()).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Source database: {} (generation {}, {})",
            report.source.database_id, report.source.generation, report.source.backend
        );
        println!(
            "{} notes, {} learnings, {} source owners, {} repositories",
            report.notes,
            report.learnings,
            report.owners.len(),
            report.repositories.len()
        );
        println!(
            "Source owners: {:?}",
            report.owners.keys().collect::<Vec<_>>()
        );
        println!("Legacy repositories: {:?}", report.repositories);
        for row in &report.rows {
            for issue in &row.issues {
                println!("{} {:?}: {issue}", row.table, row.id);
            }
        }
        println!(
            "Row verification: {}. No files published or storage mode changed.",
            if report.rows_verified {
                "passed"
            } else {
                "unresolved"
            }
        );
    }
    ensure!(
        report.rows_verified,
        "migration inventory has unresolved mappings or round-trip failures; review report"
    );
    Ok(())
}
