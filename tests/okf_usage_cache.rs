#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
#[path = "support/okf.rs"]
mod okf;
use uuid::Uuid;
use ygg::{
    config::database::KnowledgeConfig,
    knowledge::{runtime::Context, telemetry::Totals},
};

#[test]
fn usage_publishers_do_not_regress_and_corrupt_optional_state_recovers() {
    // Repeated fresh directories exercise first-creation lock races as well as
    // monotonic publication; every round races twenty independent contexts.
    for _ in 0..5 {
        concurrent_publishers();
    }
}

fn concurrent_publishers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (binding, _, _) = okf::fixture(root);
    okf::select(root, &binding);
    let config = KnowledgeConfig {
        data_dir: root.join("data"),
        knowledge_dir: root.join("bundle"),
        knowledge_policy_dir: root.join("policy"),
    };
    let id = Uuid::new_v4();
    let total = |count| Totals {
        corpus_id: binding.mappings.corpus_id,
        document_id: id,
        applied_count: count,
        last_applied_at: Some(chrono::DateTime::from_timestamp(count, 0).unwrap()),
        baseline_imported: false,
    };
    std::thread::scope(|scope| {
        for count in 1..=20 {
            let config = &config;
            let total = total(count);
            scope.spawn(move || {
                Context::open(config, "legacy-user")
                    .unwrap()
                    .unwrap()
                    .cache_usage(&[total])
                    .unwrap();
            });
        }
    });
    let context = Context::open(&config, "legacy-user").unwrap().unwrap();
    context.cache_usage(&[total(1)]).unwrap();
    assert_eq!(
        context.usage_snapshot().unwrap().totals[&id].applied_count,
        20
    );
    let mut overflow = total(i32::MAX as i64 + 1);
    overflow.last_applied_at = None;
    context.cache_usage(&[overflow]).unwrap();
    assert_eq!(
        context.usage_snapshot().unwrap().totals[&id].applied_count,
        20
    );
    let mut wrong_corpus = total(21);
    wrong_corpus.corpus_id = Uuid::new_v4();
    assert!(context.cache_usage(&[wrong_corpus]).is_err());
    assert_eq!(
        context.usage_snapshot().unwrap().totals[&id].applied_count,
        20
    );
    std::fs::write(root.join("policy/usage-snapshot.json"), "corrupt").unwrap();
    context.cache_usage(&[total(22)]).unwrap();
    assert_eq!(
        context.usage_snapshot().unwrap().totals[&id].applied_count,
        22
    );
}
