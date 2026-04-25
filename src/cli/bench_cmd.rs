//! `ygg bench` subcommand. Specified in docs/eval-benchmarks.md. Scaffold
//! lands first (this file); scenario drivers land per yggdrasil-103+104.

use crate::bench::{harness_sha, scenarios, BenchRepo, BenchTaskResult};
use sqlx::PgPool;
use uuid::Uuid;

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const GRAY: &str = "\x1b[38;5;245m";

/// `ygg bench list` — print known scenarios and their implementation status.
pub fn list() {
    println!("{DIM}id{RESET}                          {DIM}status{RESET}      {DIM}title{RESET}");
    for s in scenarios::ALL {
        let status = if s.implemented {
            format!("{GREEN}ready{RESET}")
        } else {
            format!("{GRAY}stub{RESET} ")
        };
        println!("  {:<26} {status}    {}", s.id, s.title);
    }
    println!();
    println!("{DIM}Specs in docs/eval-benchmarks.md. Drivers land per yggdrasil-103/104.{RESET}");
}

/// `ygg bench report <run-id>` — print a single bench run.
pub async fn report(pool: &PgPool, run_id: Uuid) -> anyhow::Result<()> {
    let repo = BenchRepo::new(pool);
    let run = repo.get_run(run_id).await?
        .ok_or_else(|| anyhow::anyhow!("no bench run with id {run_id}"))?;
    let results = repo.list_task_results(run_id).await?;

    let pass_count = results.iter().filter(|r| r.passed).count();
    let total = results.len();
    let wall = results.iter().map(|r| r.wall_clock_s).max().unwrap_or(0);
    let tokens_in: i64 = results.iter().filter_map(|r| r.tokens_in).sum();
    let tokens_out: i64 = results.iter().filter_map(|r| r.tokens_out).sum();

    println!("Bench run: {run_id}");
    println!("  scenario:    {}", run.scenario);
    println!("  baseline:    {} (parallelism={})", run.baseline, run.parallelism);
    println!("  model:       {}", run.model);
    println!("  harness_sha: {}", run.harness_sha);
    if let Some(seed) = run.seed { println!("  seed:        {seed}"); }
    println!("  started_at:  {}", run.started_at);
    if let Some(ended) = run.ended_at {
        println!("  duration:    {}s",
            (ended - run.started_at).num_seconds().max(0));
    }
    let verdict = match run.passed {
        Some(true)  => format!("{GREEN}PASS{RESET}"),
        Some(false) => format!("{RED}FAIL{RESET}"),
        None        => format!("{DIM}in-progress{RESET}"),
    };
    println!("  verdict:     {verdict}");
    println!("  tasks:       {pass_count}/{total} passed");
    println!("  wall-clock:  {wall}s");
    println!("  tokens:      in={tokens_in} out={tokens_out}");
    if let Some(notes) = run.notes { println!("  notes:       {notes}"); }
    Ok(())
}

/// `ygg bench run <scenario>` — kick off a scenario. Scaffold-only:
/// records a bench_runs row + finalizes as failed when the driver isn't
/// wired. Real drivers land per yggdrasil-103.
pub async fn run(
    pool: &PgPool,
    scenario_id: &str,
    baseline: crate::bench::Baseline,
    parallelism: i32,
    model: &str,
    seed: Option<i64>,
) -> anyhow::Result<Uuid> {
    let spec = scenarios::find(scenario_id)
        .ok_or_else(|| anyhow::anyhow!("unknown scenario: {scenario_id}"))?;

    let repo = BenchRepo::new(pool);
    let run = repo.create_run(
        spec.id,
        baseline,
        parallelism,
        model,
        &harness_sha(),
        seed,
    ).await?;

    println!("created bench run {} ({})", run.run_id, spec.title);

    if !spec.implemented {
        let note = format!("scenario '{}' has no driver yet (yggdrasil-103+)", spec.id);
        repo.finalize(run.run_id, false, Some(&note)).await?;
        anyhow::bail!("scenario driver not implemented: {scenario_id}");
    }

    // Drivers land per yggdrasil-103 (vanilla-single, vanilla-tmux, ygg).
    // Until then, fall through to the not-implemented finalize above.
    Ok(run.run_id)
}

/// `ygg bench diff <run-a> <run-b>` — pairwise compare two runs. Refuses to
/// declare a winner if confidence intervals would overlap; for the scaffold
/// this is the single-sample form (k=1 case). Real CI computation lands with
/// the multi-run aggregation in yggdrasil-103.
pub async fn diff(pool: &PgPool, a: Uuid, b: Uuid) -> anyhow::Result<()> {
    let repo = BenchRepo::new(pool);
    let run_a = repo.get_run(a).await?
        .ok_or_else(|| anyhow::anyhow!("no bench run {a}"))?;
    let run_b = repo.get_run(b).await?
        .ok_or_else(|| anyhow::anyhow!("no bench run {b}"))?;

    if run_a.scenario != run_b.scenario {
        anyhow::bail!(
            "scenarios differ ({} vs {}); diff requires same scenario",
            run_a.scenario, run_b.scenario
        );
    }

    let res_a = repo.list_task_results(a).await?;
    let res_b = repo.list_task_results(b).await?;

    let wall_a = res_a.iter().map(|r| r.wall_clock_s).max().unwrap_or(0);
    let wall_b = res_b.iter().map(|r| r.wall_clock_s).max().unwrap_or(0);
    let pass_a = res_a.iter().filter(|r| r.passed).count();
    let pass_b = res_b.iter().filter(|r| r.passed).count();

    println!("Scenario:  {}", run_a.scenario);
    println!();
    println!("                  {DIM}{}{RESET}    {DIM}{}{RESET}", run_a.baseline, run_b.baseline);
    println!("wall-clock        {wall_a:>6}s  {wall_b:>6}s");
    println!("tasks-passed      {:>6}    {:>6}", pass_a, pass_b);
    println!();
    println!("{DIM}Single-sample diff. k>=4 + bootstrap CIs land with yggdrasil-103.{RESET}");
    println!("{DIM}Treat point estimates with skepticism until then.{RESET}");
    Ok(())
}

/// `ygg bench ci --tier <tier>` — CI-mode. Runs the scenario(s) for the tier,
/// exits non-zero if any regression detected against the rolling median.
/// Scaffold delegates to `run` for the smoke tier, returning whether it would
/// gate. Full regression gating lands with yggdrasil-103.
pub async fn ci(pool: &PgPool, tier: crate::bench::Tier) -> anyhow::Result<()> {
    use crate::bench::Tier;
    let scenarios = match tier {
        Tier::Smoke => vec!["independent-parallel-n"],
        Tier::Regression => vec![
            "independent-parallel-n",
            "dag-linear-3",
            "fan-out-fan-in",
            "contention",
        ],
        Tier::Overnight => scenarios::ALL.iter().map(|s| s.id).collect(),
    };
    let any_implemented = scenarios.iter()
        .any(|id| scenarios::find(id).is_some_and(|s| s.implemented));
    if !any_implemented {
        anyhow::bail!(
            "tier {} has no implemented scenarios yet; CI gating waits on yggdrasil-103",
            tier.as_str()
        );
    }
    // When drivers are wired, this loop actually runs each scenario with k=4
    // and compares against rolling median — see yggdrasil-103.
    Ok(())
}

#[allow(dead_code)]
fn unused_marker() -> Option<BenchTaskResult> { None }
