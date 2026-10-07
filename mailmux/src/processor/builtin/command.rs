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

/// Kill shell-launched descendants when a processing future is cancelled.
/// The child is placed in its own group, so this never targets mailmux's group.
#[cfg(unix)]
struct ProcessGroupGuard {
    id: Option<libc::pid_t>,
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            // SAFETY: the positive child pid is also its dedicated process
            // group id. A negative pid directs SIGKILL to that group only.
            if unsafe { libc::kill(-id, libc::SIGKILL) } == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    warn!(process_group = id, %error, "failed to kill command process group");
                }
            }
        }
    }
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
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .with_context(|| format!("spawning command: {}", self.command))?;

        #[cfg(unix)]
        let mut process_group = ProcessGroupGuard {
            id: child.id().map(|id| id as libc::pid_t),
        };

        // Include stdin writes in the timeout: commands may never read input.
        // Dropping this future kills the child and, on Unix, its process group.
        let execution = async move {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(input_json.as_bytes())
                    .await
                    .context("writing to command stdin")?;
            }
            child
                .wait_with_output()
                .await
                .context("waiting for command output")
        };
        let output = tokio::time::timeout(self.timeout, execution)
            .await
            .map_err(|_| anyhow::anyhow!("command timed out after {:?}", self.timeout))??;

        #[cfg(unix)]
        {
            process_group.id = None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Always attempt to parse stdout as a structured ProcessorOutput first.
        // This preserves metadata and metrics for both zero and non-zero exits,
        // which is critical for commands like mailtx that emit failure metadata
        // before exiting non-zero.
        if let Ok(parsed) = serde_json::from_str::<ProcessorOutput>(stdout.as_ref()) {
            if output.status.success() {
                debug!(
                    processor = self.name,
                    stdout = %stdout,
                    "command completed successfully with structured output"
                );
            } else {
                let code = output.status.code().unwrap_or(-1);
                warn!(
                    processor = self.name,
                    exit_code = code,
                    stderr = %stderr,
                    "command exited non-zero but emitted structured output"
                );
            }
            return Ok(parsed);
        }

        // Fallback: stdout is not valid ProcessorOutput JSON.
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
                metrics: vec![],
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
                metrics: vec![],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn build_command_config(command: &str, args: Vec<&str>) -> ProcessorConfig {
        let mut config_map: HashMap<String, toml::Value> = HashMap::new();
        config_map.insert(
            "command".to_string(),
            toml::Value::String(command.to_string()),
        );
        if !args.is_empty() {
            config_map.insert(
                "args".to_string(),
                toml::Value::Array(
                    args.into_iter()
                        .map(|s| toml::Value::String(s.to_string()))
                        .collect(),
                ),
            );
        }
        ProcessorConfig {
            name: "test_command".into(),
            enabled: true,
            events: vec!["email_arrived".into()],
            sources: None,
            max_retries: 0,
            retry_backoff_secs: vec![],
            timeout_secs: 10,
            concurrency: 1,
            config: config_map,
        }
    }

    #[cfg(unix)]
    fn cancellation_event() -> Event {
        Event {
            id: 0,
            event_type: "email_arrived".into(),
            account_id: "test".into(),
            mailbox_name: "INBOX".into(),
            email_id: None,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        }
    }

    #[cfg(unix)]
    const SIDE_EFFECT_COMMAND: &str = r#"
cat >/dev/null
printf x > "$1/started"
(sleep 1; : > "$1/descendant") &
sleep 1
: > "$1/parent"
wait
"#;

    #[cfg(unix)]
    async fn assert_no_side_effects(dir: &std::path::Path) {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !dir.join("parent").exists(),
            "cancelled command kept running"
        );
        assert!(
            !dir.join("descendant").exists(),
            "cancelled command's descendant kept running"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_command_timeout_kills_child_and_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_command_config(
            "sh",
            vec![
                "-c",
                SIDE_EFFECT_COMMAND,
                "probe",
                dir.path().to_str().unwrap(),
            ],
        );
        let mut processor = CommandProcessor::new(&config);
        processor.timeout = Duration::from_millis(300);
        let error = processor
            .process(&cancellation_event(), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("command timed out"));
        assert!(dir.path().join("started").exists());
        assert_no_side_effects(dir.path()).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_external_cancellation_kills_child_and_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let config = build_command_config(
            "sh",
            vec![
                "-c",
                SIDE_EFFECT_COMMAND,
                "probe",
                dir.path().to_str().unwrap(),
            ],
        );
        let processor = CommandProcessor::new(&config);
        let task =
            tokio::spawn(async move { processor.process(&cancellation_event(), None).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_no_side_effects(dir.path()).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_command_timeout_includes_blocked_stdin_write() {
        let config = build_command_config("sh", vec!["-c", "sleep 5"]);
        let mut processor = CommandProcessor::new(&config);
        processor.timeout = Duration::from_millis(100);
        let mut event = cancellation_event();
        event.payload = serde_json::json!({"large": "x".repeat(1024 * 1024)});
        let result = tokio::time::timeout(Duration::from_secs(2), processor.process(&event, None))
            .await
            .expect("command's own timeout must cover blocked stdin");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("command timed out")
        );
    }

    #[tokio::test]
    async fn test_structured_stdout_nonzero_exit_preserves_metadata() {
        // Create a shell script that emits structured ProcessorOutput JSON on stdout
        // and then exits non-zero. This simulates mailtx failure behavior.
        let script_content = r#"#!/bin/sh
printf '{"success":false,"message":"firefly returned 500","metadata":{"outcome":"error","detail":"upstream unavailable"},"metrics":[{"name":"firefly_requests_total","kind":"counter","value":1.0,"labels":{"operation":"post_transaction","result":"error"}}]}'
exit 1
"#;
        let mut script_file = NamedTempFile::new().expect("create temp script");
        write!(script_file, "{script_content}").expect("write script");
        let script_path = script_file.path().to_string_lossy().to_string();

        let config = build_command_config("sh", vec![&script_path]);
        let processor = CommandProcessor::new(&config);

        let event = Event {
            id: 1,
            event_type: "email_arrived".into(),
            account_id: "test".into(),
            mailbox_name: "INBOX".into(),
            email_id: None,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };

        let output = processor
            .process(&event, None)
            .await
            .expect("process should succeed");

        // The structured output from the command should pass through unchanged,
        // preserving success=false, message, metadata, and metrics — even though
        // the command exited non-zero.
        assert!(!output.success);
        assert_eq!(output.message.as_deref(), Some("firefly returned 500"));

        let metadata = output.metadata.expect("metadata should be present");
        assert_eq!(
            metadata.get("outcome").and_then(|v| v.as_str()),
            Some("error")
        );
        assert_eq!(
            metadata.get("detail").and_then(|v| v.as_str()),
            Some("upstream unavailable")
        );

        assert_eq!(output.metrics.len(), 1);
        assert_eq!(output.metrics[0].name, "firefly_requests_total");
        assert_eq!(output.metrics[0].value, 1.0);
    }

    #[tokio::test]
    async fn test_structured_stdout_zero_exit_preserves_metadata() {
        // Create a shell script that emits structured ProcessorOutput JSON on stdout
        // and exits zero.
        let script_content = r#"#!/bin/sh
printf '{"success":true,"message":"posted","metadata":{"outcome":"posted","firefly_transaction_id":"tx-abc"},"metrics":[{"name":"emails_processed_total","kind":"counter","value":1.0,"labels":{"result":"posted"}}]}'
exit 0
"#;
        let mut script_file = NamedTempFile::new().expect("create temp script");
        write!(script_file, "{script_content}").expect("write script");
        let script_path = script_file.path().to_string_lossy().to_string();

        let config = build_command_config("sh", vec![&script_path]);
        let processor = CommandProcessor::new(&config);

        let event = Event {
            id: 1,
            event_type: "email_arrived".into(),
            account_id: "test".into(),
            mailbox_name: "INBOX".into(),
            email_id: None,
            payload: serde_json::json!({}),
            created_at: chrono::Utc::now(),
        };

        let output = processor
            .process(&event, None)
            .await
            .expect("process should succeed");

        assert!(output.success);
        assert_eq!(output.message.as_deref(), Some("posted"));

        let metadata = output.metadata.expect("metadata should be present");
        assert_eq!(
            metadata.get("outcome").and_then(|v| v.as_str()),
            Some("posted")
        );
        assert_eq!(
            metadata
                .get("firefly_transaction_id")
                .and_then(|v| v.as_str()),
            Some("tx-abc")
        );

        assert_eq!(output.metrics.len(), 1);
        assert_eq!(output.metrics[0].name, "emails_processed_total");
    }
}
