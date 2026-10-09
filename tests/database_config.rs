use ygg::config::database::{DatabaseTarget, DeploymentConfig, Environment, UserSettings};

fn resolve(toml: &str, vars: &[(&str, &str)]) -> Result<DeploymentConfig, ygg::YggError> {
    let mut env = Environment::from([("HOME".into(), "/users/test".into())]);
    env.extend(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    DeploymentConfig::resolve(&toml::from_str::<UserSettings>(toml).unwrap(), &env)
}

#[test]
fn resolution_matrix_preserves_existing_installations() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/knowledge/configuration.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let vars: Vec<_> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str().unwrap()))
            .collect();
        let actual = resolve(case["toml"].as_str().unwrap(), &vars);
        match case["target"].as_str().unwrap() {
            "managed" => assert!(
                matches!(
                    actual.unwrap().database,
                    DatabaseTarget::ManagedLocal { .. }
                ),
                "{case}"
            ),
            "external" => match actual.unwrap().database {
                DatabaseTarget::External { url } => {
                    assert_eq!(url, case["url"].as_str().unwrap(), "{case}")
                }
                _ => panic!("{case}"),
            },
            "error" => assert!(actual.is_err(), "{case}"),
            _ => panic!("invalid fixture"),
        }
    }
}

#[test]
fn configuration_loading_never_starts_or_creates_a_cluster() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join(".env"),
        "DATABASE_URL=postgres://legacy/old\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("config.toml"),
        "[database]\nurl='postgres://configured/db'\n",
    )
    .unwrap();
    let env = Environment::from([
        (
            "YGG_CONFIG_DIR".into(),
            temp.path().to_str().unwrap().into(),
        ),
        (
            "YGG_DATA_DIR".into(),
            temp.path().join("absent").to_str().unwrap().into(),
        ),
        ("DATABASE_URL".into(), "postgres://explicit/db".into()),
    ]);
    let config = DeploymentConfig::load(env.clone()).unwrap();
    assert_eq!(
        config.database,
        DatabaseTarget::External {
            url: "postgres://explicit/db".into()
        }
    );
    assert!(!config.data_dir.exists());
    let mut legacy_env = env;
    legacy_env.remove("DATABASE_URL");
    assert_eq!(
        DeploymentConfig::load(legacy_env).unwrap().database,
        DatabaseTarget::External {
            url: "postgres://legacy/old".into()
        }
    );
}

#[test]
fn profiles_and_knowledge_are_independent_of_database_location() {
    let config = resolve(
        "data_dir='/data'\nknowledge_dir='/private/notes'",
        &[
            ("YGG_PROFILE", "work"),
            ("DATABASE_URL", "postgres://host/db"),
        ],
    )
    .unwrap();
    assert_eq!(config.data_dir.to_str(), Some("/data/profiles/work"));
    assert_eq!(config.knowledge_dir.to_str(), Some("/private/notes"));
    for profile in ["..", "a/b", "", "/tmp", "a.b"] {
        assert!(resolve("", &[("YGG_PROFILE", profile)]).is_err());
    }
    assert!(resolve("", &[("YGG_DATA_DIR", "relative")]).is_err());
    assert!(resolve("", &[("YGG_KNOWLEDGE_DIR", "relative")]).is_err());
}

#[test]
fn diagnostics_do_not_disclose_connection_secrets() {
    let config = resolve(
        "",
        &[("DATABASE_URL", "postgres://person:supersecret@host/db")],
    )
    .unwrap();
    assert!(!format!("{:?}", config.database).contains("supersecret"));
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("config.toml"),
        "[database]\nmode='supersecret'\n",
    )
    .unwrap();
    let result = DeploymentConfig::load(Environment::from([(
        "YGG_CONFIG_DIR".into(),
        temp.path().to_str().unwrap().into(),
    )]));
    assert!(!result.err().unwrap().to_string().contains("supersecret"));
}

#[test]
fn application_config_uses_user_defaults_and_redacts_database() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join(".env"), "DATABASE_URL=postgres://user:secret@host/db\nLOCK_TTL_SECS=123\nRTK_BINARY_PATH=custom-rtk\n").unwrap();
    let mut env = Environment::from([
        ("HOME".into(), temp.path().to_str().unwrap().into()),
        ("YGG_CONFIG_DIR".into(), config.to_str().unwrap().into()),
        ("LOCK_TTL_SECS".into(), "456".into()),
    ]);
    let app = ygg::config::AppConfig::from_environment(env.clone()).unwrap();
    assert_eq!(app.lock_ttl_secs, 456);
    assert_eq!(app.rtk_binary_path, "custom-rtk");
    assert!(matches!(app.database, DatabaseTarget::External { .. }));
    assert!(!format!("{app:?}").contains("secret"));
    env.insert("YGG_DB_MODE".into(), "managed".into());
    assert!(ygg::config::AppConfig::from_environment(env).is_err());
    assert!(!temp.path().join("Library").exists());
    assert!(!temp.path().join(".local").exists());
}

#[test]
fn external_owner_is_separate_redacted_and_environment_overrides_config() {
    let config = resolve("[database]\nurl='postgres://runtime@localhost/app'\nowner_url='postgres://owner:private@localhost/app'", &[]).unwrap();
    assert!(
        matches!(config.database, DatabaseTarget::External { ref url } if url.contains("runtime"))
    );
    assert!(!format!("{:?}", config.owner_url).contains("private"));
    let config = resolve("[database]\nurl='postgres://runtime@localhost/app'\nowner_url='postgres://old@localhost/app'", &[("YGG_DATABASE_OWNER_URL", "postgres://new@localhost/app")]).unwrap();
    assert!(config.owner_url.unwrap().as_str().contains("new"));
    assert!(
        resolve(
            "",
            &[("YGG_DATABASE_OWNER_URL", "postgres://owner@localhost/app")]
        )
        .is_err()
    );
    assert!(
        resolve(
            "[database]\nurl='postgres://runtime@localhost/app'",
            &[("YGG_DATABASE_OWNER_URL", "")]
        )
        .is_err()
    );
}

#[test]
fn policy_path_is_separate_profile_scoped_and_explicitly_overridable() {
    let default = resolve("data_dir='/data'", &[("YGG_PROFILE", "work")]).unwrap();
    assert_eq!(
        default.knowledge_policy_dir.to_str(),
        Some("/data/profiles/work/knowledge-policy")
    );
    let configured = resolve(
        "knowledge_dir='/corpus'\nknowledge_policy_dir='/policy'",
        &[("DATABASE_URL", "postgres://host/db")],
    )
    .unwrap();
    assert_eq!(configured.knowledge_policy_dir.to_str(), Some("/policy"));
    let overridden = resolve(
        "knowledge_policy_dir='/policy'",
        &[("YGG_KNOWLEDGE_POLICY_DIR", "/other-policy")],
    )
    .unwrap();
    assert_eq!(
        overridden.knowledge_policy_dir.to_str(),
        Some("/other-policy")
    );
    assert!(resolve("", &[("YGG_KNOWLEDGE_POLICY_DIR", "relative")]).is_err());
    assert!(
        resolve(
            "knowledge_dir='/corpus'\nknowledge_policy_dir='/corpus/policy'",
            &[]
        )
        .is_err()
    );
}

#[test]
fn knowledge_paths_resolve_independently_of_database_validation() {
    use ygg::config::database::{DeploymentConfig, Environment, KnowledgeConfig, UserSettings};
    let settings = UserSettings::default();
    let mut env = Environment::from([
        ("YGG_DATA_DIR".into(), "/tmp/ygg-independent".into()),
        ("YGG_PROFILE".into(), "offline".into()),
        ("YGG_DB_MODE".into(), "external".into()),
    ]);
    assert!(DeploymentConfig::resolve(&settings, &env).is_err());
    let knowledge = KnowledgeConfig::resolve(&settings, &env).unwrap();
    assert_eq!(
        knowledge.knowledge_dir,
        std::path::Path::new("/tmp/ygg-independent/profiles/offline/knowledge")
    );
    env.insert(
        "DATABASE_URL".into(),
        "postgres://unreachable.invalid/db".into(),
    );
    let deployment = DeploymentConfig::resolve(&settings, &env).unwrap();
    assert_eq!(knowledge.knowledge_dir, deployment.knowledge_dir);
    assert_eq!(
        knowledge.knowledge_policy_dir,
        deployment.knowledge_policy_dir
    );
    env.insert(
        "YGG_KNOWLEDGE_POLICY_DIR".into(),
        knowledge.knowledge_dir.to_string_lossy().into_owned(),
    );
    assert!(KnowledgeConfig::resolve(&settings, &env).is_err());
}
