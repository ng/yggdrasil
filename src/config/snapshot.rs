//! Private source-configuration evidence. Never expose raw settings/credentials in
//! command reports. Restores retain this evidence without selecting its endpoint.
use super::database::{DatabaseTarget, DeploymentConfig, Environment, UserSettings};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
fn relevant(key: &str) -> bool {
    key.starts_with("YGG_")
        || key.starts_with("PG")
        || matches!(
            key,
            "HOME"
                | "XDG_CONFIG_HOME"
                | "XDG_DATA_HOME"
                | "DATABASE_URL"
                | "CONTEXT_LIMIT_TOKENS"
                | "CONTEXT_HARD_CAP_TOKENS"
                | "LOCK_TTL_SECS"
                | "HEARTBEAT_INTERVAL_SECS"
                | "WATCHER_INTERVAL_SECS"
                | "RTK_BINARY_PATH"
        )
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Origin {
    directory: PathBuf,
    overrides: Environment,
}
impl Origin {
    pub fn new(env: &Environment) -> Result<Self> {
        Ok(Self {
            directory: super::database::config_dir(env)?,
            overrides: env
                .iter()
                .filter(|(k, _)| relevant(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        })
    }
    fn files(&self) -> Result<(Option<String>, Option<String>)> {
        ensure!(
            self.directory.is_absolute(),
            "absolute user configuration directory required"
        );
        Ok((
            read(&self.directory.join("config.toml"))?,
            read(&self.directory.join(".env"))?,
        ))
    }
    pub fn resolve(&self) -> Result<DeploymentConfig> {
        let (toml, dotenv) = self.files()?;
        let values = parse_dotenv(dotenv.as_deref())?;
        let mut config = resolve(&self.overrides, toml.as_deref(), &values)?;
        config.configuration_source = Some(self.clone());
        Ok(config)
    }
}
fn read(path: &Path) -> Result<Option<String>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("cannot safely read user configuration input"),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.nlink() == 1 && meta.uid() == unsafe { libc::geteuid() },
        "configuration input must be an owned regular non-hardlinked file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= MAX_INPUT_BYTES,
        "configuration input exceeds 1 MiB"
    );
    Ok(Some(String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!("configuration input must be UTF-8")
    })?))
}
fn parse_dotenv(dotenv: Option<&str>) -> Result<Environment> {
    let mut values = Environment::new();
    if let Some(dotenv) = dotenv {
        // Bound expansion amplification as well as raw input. Every prior value
        // is bounded below, so a single parser step cannot grow without limit.
        ensure!(
            dotenv.bytes().filter(|b| *b == b'$').count() <= 64,
            "configuration environment exceeds substitution limit"
        );
        let mut total = 0usize;
        for row in dotenvy::from_read_iter(dotenv.as_bytes()) {
            let (key, value) =
                row.map_err(|_| anyhow::anyhow!("invalid saved user environment"))?;
            ensure!(
                value.len() <= MAX_INPUT_BYTES,
                "expanded configuration value exceeds limit"
            );
            total = total
                .checked_add(key.len() + value.len())
                .ok_or_else(|| anyhow::anyhow!("configuration expansion exceeds limit"))?;
            ensure!(
                total <= 4 * MAX_INPUT_BYTES && values.len() < 4096,
                "configuration environment exceeds expansion limit"
            );
            values.entry(key).or_insert(value);
        }
    }
    Ok(values)
}
fn resolve(
    overrides: &Environment,
    toml: Option<&str>,
    values: &Environment,
) -> Result<DeploymentConfig> {
    let settings: UserSettings = toml
        .map(toml::from_str)
        .transpose()
        .map_err(|_| anyhow::anyhow!("invalid saved user configuration"))?
        .unwrap_or_default();
    let mut env = overrides.clone();
    for (key, value) in values {
        env.entry(key.clone()).or_insert(value.clone());
    }
    DeploymentConfig::resolve(&settings, &env)
        .map_err(|_| anyhow::anyhow!("saved configuration cannot resolve a deployment"))
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Effective {
    mode: String,
    url: Option<String>,
    owner_url: Option<String>,
    data_dir: PathBuf,
    knowledge_dir: PathBuf,
    knowledge_policy_dir: PathBuf,
}
impl Effective {
    fn of(config: &DeploymentConfig) -> Self {
        let (mode, url) = match &config.database {
            DatabaseTarget::ManagedLocal { .. } => ("managed", None),
            DatabaseTarget::External { url } => ("external", Some(url.clone())),
        };
        Self {
            mode: mode.into(),
            url,
            owner_url: config.owner_url.as_ref().map(|s| s.as_str().into()),
            data_dir: config.data_dir.clone(),
            knowledge_dir: config.knowledge_dir.clone(),
            knowledge_policy_dir: config.knowledge_policy_dir.clone(),
        }
    }
    fn resolve(&self) -> Result<DeploymentConfig> {
        let mut env = Environment::from([
            ("YGG_DB_MODE".into(), self.mode.clone()),
            (
                "YGG_DATA_DIR".into(),
                self.data_dir.to_string_lossy().into_owned(),
            ),
            (
                "YGG_KNOWLEDGE_DIR".into(),
                self.knowledge_dir.to_string_lossy().into_owned(),
            ),
            (
                "YGG_KNOWLEDGE_POLICY_DIR".into(),
                self.knowledge_policy_dir.to_string_lossy().into_owned(),
            ),
        ]);
        if let Some(url) = &self.url {
            env.insert("DATABASE_URL".into(), url.clone());
        }
        if let Some(url) = &self.owner_url {
            env.insert("YGG_DATABASE_OWNER_URL".into(), url.clone());
        }
        resolve(&env, None, &Environment::new())
    }
}
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    version: u32,
    effective: Effective,
    origin: Option<Origin>,
    config_toml: Option<String>,
    dotenv: Option<String>,
    dotenv_values: Environment,
    backup_policy_directory: Option<PathBuf>,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigurationSnapshot([redacted])")
    }
}
impl Snapshot {
    pub fn capture(config: &DeploymentConfig, policy_override: Option<&Path>) -> Result<Self> {
        if let DatabaseTarget::ManagedLocal { data_dir } = &config.database {
            ensure!(
                data_dir == &config.data_dir,
                "inconsistent managed data directory"
            );
        }
        let (config_toml, dotenv) = config
            .configuration_source
            .as_ref()
            .map(Origin::files)
            .transpose()?
            .unwrap_or((None, None));
        let snapshot = Self {
            version: 1,
            effective: Effective::of(config),
            origin: config.configuration_source.clone(),
            config_toml,
            dotenv_values: parse_dotenv(dotenv.as_deref())?,
            dotenv,
            backup_policy_directory: policy_override.map(Path::to_path_buf),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported configuration snapshot version"
        );
        ensure!(
            self.backup_policy_directory
                .as_ref()
                .is_none_or(|p| p.is_absolute()),
            "absolute backup policy directory required"
        );
        ensure!(
            self.config_toml
                .as_ref()
                .is_none_or(|s| s.len() <= MAX_INPUT_BYTES)
                && self
                    .dotenv
                    .as_ref()
                    .is_none_or(|s| s.len() <= MAX_INPUT_BYTES),
            "configuration snapshot input exceeds limit"
        );
        ensure!(
            Effective::of(&self.effective.resolve()?) == self.effective,
            "inconsistent effective configuration"
        );
        ensure!(
            self.dotenv.is_some() || self.dotenv_values.is_empty(),
            "unbound expanded configuration values"
        );
        if let Some(origin) = &self.origin {
            ensure!(
                super::database::config_dir(&origin.overrides)? == origin.directory,
                "inconsistent configuration origin"
            );
            ensure!(
                origin.directory.is_absolute() && origin.overrides.keys().all(|k| relevant(k)),
                "unsupported configuration input origin"
            );
            ensure!(
                Effective::of(&resolve(
                    &origin.overrides,
                    self.config_toml.as_deref(),
                    &self.dotenv_values
                )?) == self.effective,
                "configuration inputs changed since deployment selection"
            );
        } else {
            ensure!(
                self.config_toml.is_none()
                    && self.dotenv.is_none()
                    && self.dotenv_values.is_empty(),
                "unbound configuration input files"
            );
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= MAX_SNAPSHOT_BYTES,
            "configuration snapshot exceeds size limit"
        );
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_SNAPSHOT_BYTES,
            "configuration snapshot exceeds size limit"
        );
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("invalid saved configuration snapshot"))?;
        value.validate()?;
        Ok(value)
    }
    /// Live-source verification is explicit; offline archive verification never
    /// reads source-machine paths or imports its environment into this process.
    pub fn verify_sources(&self) -> Result<()> {
        self.validate()?;
        if let Some(origin) = &self.origin {
            let (toml, dotenv) = origin.files()?;
            ensure!(
                toml == self.config_toml
                    && dotenv == self.dotenv
                    && parse_dotenv(dotenv.as_deref())? == self.dotenv_values,
                "user configuration changed since source backup"
            );
        }
        Ok(())
    }
    pub fn verify_selection(&self, config: &DeploymentConfig) -> Result<()> {
        ensure!(
            Effective::of(config) == self.effective,
            "selected deployment differs from backed-up configuration"
        );
        self.verify_sources()
    }
}
