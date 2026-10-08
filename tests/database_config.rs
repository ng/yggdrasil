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
