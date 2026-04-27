//! Regression tests for soft-delete + trash + purge (yggdrasil-123).
//!
//! Requires Postgres + migrations applied:
//!     DATABASE_URL=postgres://ng@localhost:5432/ygg cargo test --test task_soft_delete -- --test-threads=1

use std::env;
use uuid::Uuid;
use ygg::models::repo::RepoRepo;
use ygg::models::task::{TaskCreate, TaskKind, TaskRepo, TaskStatus};

async fn make_task(pool: &sqlx::PgPool, suffix: &str, idx: usize) -> (Uuid, Uuid) {
    let prefix = format!("trash{suffix}");
    let repo = RepoRepo::new(pool)
        .register(None, &prefix, &prefix, Some(&format!("/tmp/{prefix}")))
        .await
        .unwrap();
    let labels: [String; 0] = [];
    let task = TaskRepo::new(pool)
        .create(
            repo.repo_id,
            None,
            TaskCreate {
                title: &format!("trash-test-{suffix}-{idx}"),
                description: "",
                acceptance: None,
                design: None,
                notes: None,
                kind: TaskKind::Task,
                priority: 2,
                assignee: None,
                labels: &labels,
                external_ref: None,
            },
        )
        .await
        .unwrap();
    (repo.repo_id, task.task_id)
}

async fn teardown(pool: &sqlx::PgPool, repo_id: Uuid) {
    sqlx::query("DELETE FROM repos WHERE repo_id = $1")
        .bind(repo_id)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
async fn soft_delete_hides_from_list_ready_blocked() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let (repo_id, task_id) = make_task(&pool, "hides", 1).await;
    let repo = TaskRepo::new(&pool);

    // Pre-delete: task surfaces in list + ready.
    let before_list = repo
        .list(Some(repo_id), Some(TaskStatus::Open))
        .await
        .unwrap();
    assert!(before_list.iter().any(|t| t.task_id == task_id));
    let before_ready = repo.ready(repo_id).await.unwrap();
    assert!(before_ready.iter().any(|t| t.task_id == task_id));

    // Soft-delete.
    assert!(repo.soft_delete(task_id).await.unwrap());

    // Post-delete: gone from list + ready.
    let after_list = repo
        .list(Some(repo_id), Some(TaskStatus::Open))
        .await
        .unwrap();
    assert!(!after_list.iter().any(|t| t.task_id == task_id));
    let after_ready = repo.ready(repo_id).await.unwrap();
    assert!(!after_ready.iter().any(|t| t.task_id == task_id));

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn restore_brings_task_back_into_views() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let (repo_id, task_id) = make_task(&pool, "restore", 1).await;
    let repo = TaskRepo::new(&pool);

    repo.soft_delete(task_id).await.unwrap();
    assert!(repo.restore(task_id).await.unwrap());

    let after = repo
        .list(Some(repo_id), Some(TaskStatus::Open))
        .await
        .unwrap();
    assert!(after.iter().any(|t| t.task_id == task_id));

    // Restoring an already-live row is a no-op.
    assert!(!repo.restore(task_id).await.unwrap());

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn list_trashed_returns_only_deleted_rows() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let (repo_id, alive_id) = make_task(&pool, "trashlist", 1).await;
    let repo = TaskRepo::new(&pool);

    // Create a second task in the same repo, then trash it.
    let labels: [String; 0] = [];
    let trashed = repo
        .create(
            repo_id,
            None,
            TaskCreate {
                title: "trashlist-victim",
                description: "",
                acceptance: None,
                design: None,
                notes: None,
                kind: TaskKind::Task,
                priority: 2,
                assignee: None,
                labels: &labels,
                external_ref: None,
            },
        )
        .await
        .unwrap();
    repo.soft_delete(trashed.task_id).await.unwrap();

    let bin = repo.list_trashed(Some(repo_id)).await.unwrap();
    let ids: Vec<Uuid> = bin.iter().map(|t| t.task_id).collect();
    assert!(ids.contains(&trashed.task_id));
    assert!(!ids.contains(&alive_id));

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn purge_drops_only_old_trashed_rows() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let (repo_id, fresh_id) = make_task(&pool, "purge", 1).await;
    let (_, old_id) = make_task(&pool, "purge", 2).await;
    let repo = TaskRepo::new(&pool);

    // Trash both, then back-date one beyond the purge horizon.
    repo.soft_delete(fresh_id).await.unwrap();
    repo.soft_delete(old_id).await.unwrap();
    sqlx::query("UPDATE tasks SET deleted_at = now() - interval '40 days' WHERE task_id = $1")
        .bind(old_id)
        .execute(&pool)
        .await
        .unwrap();

    // Purge anything older than 30 days, scoped to this repo.
    let n = repo.purge_older_than(30, Some(repo_id)).await.unwrap();
    assert_eq!(n, 1, "exactly the back-dated row must purge");

    let still_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE task_id = $1)")
            .bind(fresh_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(still_exists, "fresh trashed row must survive 30d purge");

    let old_gone: bool =
        sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM tasks WHERE task_id = $1)")
            .bind(old_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(old_gone, "back-dated row must be hard-deleted");

    teardown(&pool, repo_id).await;
}

#[tokio::test]
async fn ready_skips_deleted_blockers_so_dependents_unblock() {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = ygg::db::create_pool(&db_url).await.unwrap();
    let (repo_id, task_id) = make_task(&pool, "blockerdel", 1).await;
    let repo = TaskRepo::new(&pool);

    // Create a blocker; declare task depends on it.
    let labels: [String; 0] = [];
    let blocker = repo
        .create(
            repo_id,
            None,
            TaskCreate {
                title: "blockerdel-blocker",
                description: "",
                acceptance: None,
                design: None,
                notes: None,
                kind: TaskKind::Task,
                priority: 2,
                assignee: None,
                labels: &labels,
                external_ref: None,
            },
        )
        .await
        .unwrap();
    repo.add_dep(task_id, blocker.task_id).await.unwrap();

    // Pre-delete: task is blocked.
    let pre = repo.ready(repo_id).await.unwrap();
    assert!(!pre.iter().any(|t| t.task_id == task_id));

    // Trash the blocker → task unblocks.
    repo.soft_delete(blocker.task_id).await.unwrap();
    let post = repo.ready(repo_id).await.unwrap();
    assert!(post.iter().any(|t| t.task_id == task_id));

    teardown(&pool, repo_id).await;
}
