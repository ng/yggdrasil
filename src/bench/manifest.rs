//! Scenario manifest loader. Reads `manifest.toml` and `grade.sh` from a
//! scenario directory.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub title: String,
    #[serde(default = "default_parallelism")]
    pub default_parallelism: u32,
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub title: String,
    pub prompt: String,
}

fn default_parallelism() -> u32 { 1 }

#[derive(Debug, Clone)]
pub struct LoadedManifest {
    pub manifest: Manifest,
    pub root: PathBuf,
}

impl LoadedManifest {
    pub fn load(root: impl AsRef<Path>) -> Result<Self, anyhow::Error> {
        let root = root.as_ref().to_path_buf();
        let path = root.join("manifest.toml");
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let manifest: Manifest = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        Ok(Self { manifest, root })
    }

    pub fn grader_path(&self) -> PathBuf {
        self.root.join("grade.sh")
    }

    pub fn seed_repo(&self) -> PathBuf {
        self.root.join("seed_repo")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_independent_parallel_n() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("benches/scenarios/independent-parallel-n");
        let m = LoadedManifest::load(&root).expect("load manifest");
        assert_eq!(m.manifest.id, "independent-parallel-n");
        assert_eq!(m.manifest.tasks.len(), 4);
        assert!(m.grader_path().exists(), "grade.sh should exist");
    }
}
