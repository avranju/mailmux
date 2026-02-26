use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::config::ProcessorConfig;
use crate::db::emails::EmailRecord;
use crate::db::events::Event;
use crate::processor::{Processor, ProcessorOutput};

/// A processor that executes an external CLI command.
/// Passes event data as JSON on stdin, reads stdout for the result.
pub struct CommandProcessor {
    name: String,
    events: Vec<String>,
    command: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    timeout: Duration,
}

impl CommandProcessor {
    pub fn new(config: &ProcessorConfig) -> Self {
        let command = config
            .config
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let args: Vec<String> = config
            .config
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let env: Vec<(String, String)> = config
            .config
            .get("env")
            .and_then(|v| v.as_table())
            .map(|table| {
                table
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.to_string(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        Self {
            name: config.name.clone(),
            events: config.events.clone(),
            command,
            args,
            env,
            timeout: Duration::from_secs(config.timeout_secs),
        }
    }
}

#[async_trait]
impl Processor for CommandProcessor {
    fn name(&self) -> &str {
        &self.name
    }

    fn subscribed_events(&self) -> &[String] {
        &self.events
    }

    async fn process(&self, event: &Event, email: Option<&EmailRecord>) -> Result<ProcessorOutput> {
        let input = serde_json::json!({
            "event": event,
            "email": email,
        });
        let input_json = serde_json::to_string(&input).context("serializing event to JSON")?;

        debug!(
            processor = self.name,
            command = self.command,
            "executing command processor"
        );

        let mut command = tokio::process::Command::new(&self.command);
        command
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = command
            .spawn()
            .with_context(|| format!("spawning command: {}", self.command))?;

        // Write JSON to stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input_json.as_bytes())
                .await
                .context("writing to command stdin")?;
            // Drop stdin to close it
        }

        // Wait for the process with a timeout
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("command timed out after {:?}", self.timeout))?
            .context("waiting for command output")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            debug!(
                processor = self.name,
                stdout = %stdout,
                "command completed successfully"
            );
            Ok(ProcessorOutput {
                success: true,
                message: if stdout.is_empty() {
                    None
                } else {
                    Some(stdout.into_owned())
                },
                metadata: None,
            })
        } else {
            let code = output.status.code().unwrap_or(-1);
            warn!(
                processor = self.name,
                exit_code = code,
                stderr = %stderr,
                "command failed"
            );
            Ok(ProcessorOutput {
                success: false,
                message: Some(format!(
                    "exit code {code}: {}",
                    if stderr.is_empty() {
                        stdout.into_owned()
                    } else {
                        stderr.into_owned()
                    }
                )),
                metadata: None,
            })
        }
    }
}
