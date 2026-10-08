//! Side-effect-free deployment resolution. Loading configuration never starts a
//! database, installs binaries, or reads a repository-local environment file.
use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::Deserialize;

use crate::YggError;

#[derive(Clone, PartialEq, Eq)]
pub enum DatabaseTarget {
    ManagedLocal { data_dir: PathBuf },
    External { url: String },
}

// Database URLs can contain passwords and TLS key paths. Never derive Debug.
impl fmt::Debug for DatabaseTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManagedLocal { data_dir } => f
                .debug_struct("ManagedLocal")
                .field("data_dir", data_dir)
                .finish(),
            Self::External { .. } => f.write_str("External { url: [redacted] }"),
        }
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseMode {
    Managed,
    External,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSettings {
    pub mode: Option<DatabaseMode>,
    pub url: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserSettings {
    #[serde(default)]
    pub database: DatabaseSettings,
    pub data_dir: Option<PathBuf>,
    pub knowledge_dir: Option<PathBuf>,
    pub profile: Option<String>,
}

/// Snapshot environment inputs rather than mutating the process environment.
/// This also permits deterministic configuration tests without unsafe set_var.
pub type Environment = BTreeMap<String, String>;

pub struct DeploymentConfig {
    pub database: DatabaseTarget,
    pub data_dir: PathBuf,
    pub knowledge_dir: PathBuf,
}

fn error(message: &str) -> YggError {
    YggError::Config(message.into())
}

fn absolute(path: PathBuf, key: &str) -> Result<PathBuf, YggError> {
    if !path.is_absolute() {
        return Err(error(&format!("{key} must be an absolute path")));
    }
    Ok(path)
}

pub fn config_dir(env: &Environment) -> Result<PathBuf, YggError> {
    if let Some(path) = env.get("YGG_CONFIG_DIR") {
        return absolute(path.into(), "YGG_CONFIG_DIR");
    }
    if let Some(path) = env.get("XDG_CONFIG_HOME") {
        return absolute(PathBuf::from(path).join("ygg"), "XDG_CONFIG_HOME");
    }
    let home = env
        .get("HOME")
        .ok_or_else(|| error("HOME or YGG_CONFIG_DIR required"))?;
    absolute(PathBuf::from(home).join(".config/ygg"), "HOME")
}

impl DeploymentConfig {
    /// Read only user-owned configuration. The legacy .env supplies defaults;
    /// the inherited environment wins, including deliberately empty values.
    pub fn load(env: Environment) -> Result<Self, YggError> {
        let dir = config_dir(&env)?;
        Self::from_user_environment(&user_environment(env)?, &dir)
    }

    pub(super) fn from_user_environment(
        env: &Environment,
        dir: &std::path::Path,
    ) -> Result<Self, YggError> {
        let settings = match std::fs::read_to_string(dir.join("config.toml")) {
            // Do not echo TOML parse errors: source excerpts can contain secrets.
            Ok(text) => toml::from_str(&text).map_err(|_| error("invalid user config.toml"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => UserSettings::default(),
            Err(_) => return Err(error("cannot read user config.toml")),
        };
        Self::resolve(&settings, env)
    }

    pub fn resolve(settings: &UserSettings, env: &Environment) -> Result<Self, YggError> {
        let mode = match env.get("YGG_DB_MODE").map(String::as_str) {
            Some("managed") => Some(DatabaseMode::Managed),
            Some("external") => Some(DatabaseMode::External),
            Some(_) => return Err(error("YGG_DB_MODE must be managed or external")),
            None => settings.database.mode,
        };
        let url = env.get("DATABASE_URL").or(settings.database.url.as_ref());
        if url.is_some_and(|url| url.trim().is_empty()) {
            return Err(error("external database URL is empty"));
        }
        if mode == Some(DatabaseMode::Managed) && url.is_some() {
            return Err(error(
                "managed database mode conflicts with external URL; repair configuration",
            ));
        }
        if mode == Some(DatabaseMode::External) && url.is_none() {
            return Err(error(
                "external database mode requires DATABASE_URL or database.url",
            ));
        }
        let profile = env
            .get("YGG_PROFILE")
            .or(settings.profile.as_ref())
            .map(String::as_str)
            .unwrap_or("default");
        if profile.is_empty()
            || profile.len() > 64
            || !profile
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(error(
                "profile must contain 1-64 ASCII letters, digits, hyphens or underscores",
            ));
        }
        let base = if let Some(path) = env.get("YGG_DATA_DIR") {
            PathBuf::from(path)
        } else if let Some(path) = &settings.data_dir {
            path.clone()
        } else if let Some(path) = env.get("XDG_DATA_HOME") {
            PathBuf::from(path).join("ygg")
        } else {
            let home = env
                .get("HOME")
                .ok_or_else(|| error("HOME or YGG_DATA_DIR required"))?;
            if cfg!(target_os = "macos") {
                PathBuf::from(home).join("Library/Application Support/ygg")
            } else {
                PathBuf::from(home).join(".local/share/ygg")
            }
        };
        let base = absolute(base, "data directory")?;
        let data_dir = if profile == "default" {
            base
        } else {
            base.join("profiles").join(profile)
        };
        let knowledge_dir = absolute(
            env.get("YGG_KNOWLEDGE_DIR")
                .map(PathBuf::from)
                .or_else(|| settings.knowledge_dir.clone())
                .unwrap_or_else(|| data_dir.join("knowledge")),
            "knowledge directory",
        )?;
        let database = match url {
            Some(url) => DatabaseTarget::External { url: url.clone() },
            None => DatabaseTarget::ManagedLocal {
                data_dir: data_dir.clone(),
            },
        };
        Ok(Self {
            database,
            data_dir,
            knowledge_dir,
        })
    }
}

/// Merge only user-level defaults without changing the process environment.
pub(super) fn user_environment(mut env: Environment) -> Result<Environment, YggError> {
    let dir = config_dir(&env)?;
    match dotenvy::from_path_iter(dir.join(".env")) {
        Ok(entries) => {
            for entry in entries {
                let (key, value) = entry.map_err(|_| error("invalid user .env"))?;
                env.entry(key).or_insert(value);
            }
        }
        Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(error("cannot read user .env")),
    }
    Ok(env)
}
