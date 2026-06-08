use std::process::Stdio;
use tokio::process::Command;

/// Result of an RTK-proxied command execution.
#[derive(Debug)]
pub struct ExecutionResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub token_count: usize,
}

/// Executes commands through the RTK binary for token-optimized output.
pub struct Executor {
    rtk_path: String,
}

impl Executor {
    pub fn new(rtk_path: String) -> Self {
        Self { rtk_path }
    }

    /// Execute a command through RTK: `rtk <command> [args...]`
    pub async fn run(
        &self,
        command: &str,
        args: &[&str],
    ) -> Result<ExecutionResult, crate::YggError> {
        let output = Command::new(&self.rtk_path)
            .arg(command)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| crate::YggError::Executor {
                exit_code: -1,
                stderr: format!("failed to spawn rtk: {e}"),
            })?
            .wait_with_output()
            .await
            .map_err(|e| crate::YggError::Executor {
                exit_code: -1,
                stderr: format!("rtk execution error: {e}"),
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let token_count = estimate_tokens(&stdout) + estimate_tokens(&stderr);

        Ok(ExecutionResult {
            stdout,
            stderr,
            exit_code: output.status.code().unwrap_or(-1),
            token_count,
        })
    }
}

/// Approximate token count: chars / 4.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}
