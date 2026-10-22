use metrics::{counter, describe_counter, describe_gauge, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing::error;

/// Initialize the metrics exporter and describe all metrics.
/// Returns a handle that can render the /metrics endpoint.
pub fn init() -> Option<metrics_exporter_prometheus::PrometheusHandle> {
    let recorder = PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    match metrics::set_global_recorder(recorder) {
        Ok(()) => {
            describe_all();
            Some(handle)
        }
        Err(e) => {
            error!(error = %e, "failed to set global metrics recorder");
            None
        }
    }
}

fn describe_all() {
    describe_counter!(
        "mailmux_messages_ingested_total",
        "Total number of email messages ingested"
    );
    describe_counter!(
        "mailmux_events_created_total",
        "Total number of events created"
    );
    describe_counter!(
        "mailmux_processor_runs_total",
        "Total number of processor executions"
    );
    describe_gauge!(
        "mailmux_active_connections",
        "Number of active IMAP connections"
    );
    describe_gauge!(
        "mailmux_mailboxes_monitored",
        "Number of mailboxes being monitored"
    );
    describe_counter!(
        "mailmux_idle_heartbeat_catches_total",
        "Messages found by the periodic heartbeat sync that IDLE failed to deliver"
    );
    describe_counter!(
        "mailmux_sync_failures_total",
        "Total number of sync cycle failures, categorized by error type"
    );
}

// --- Convenience functions for recording metrics ---

pub fn inc_messages_ingested(account: &str, mailbox: &str) {
    counter!("mailmux_messages_ingested_total", "account" => account.to_owned(), "mailbox" => mailbox.to_owned())
        .increment(1);
}

pub fn inc_events_created(event_type: &str) {
    counter!("mailmux_events_created_total", "event_type" => event_type.to_owned()).increment(1);
}

pub fn inc_processor_runs(processor: &str, status: &str) {
    counter!("mailmux_processor_runs_total", "processor" => processor.to_owned(), "status" => status.to_owned())
        .increment(1);
}

pub fn set_mailboxes_monitored(account: &str, count: f64) {
    gauge!("mailmux_mailboxes_monitored", "account" => account.to_owned()).set(count);
}

pub fn add_idle_heartbeat_catches(account: &str, mailbox: &str, count: u64) {
    counter!("mailmux_idle_heartbeat_catches_total", "account" => account.to_owned(), "mailbox" => mailbox.to_owned())
        .increment(count);
}

/// Categorize an error into a human-readable error type for metric labels.
/// Authentication failures are handled separately (the watcher logs an error
/// and exits immediately), so they are intentionally excluded here.
pub fn classify_error(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("timed out") {
        "connection_timeout"
    } else if lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("network unreachable")
        || lower.contains("no such host")
        || lower.contains("tls")
    {
        "network"
    } else {
        "other"
    }
}

pub fn inc_sync_failures(account: &str, mailbox: &str, error_type: &str) {
    counter!("mailmux_sync_failures_total", "account" => account.to_owned(), "mailbox" => mailbox.to_owned(), "error_type" => error_type.to_owned())
        .increment(1);
}

/// Record metrics emitted by a processor in its `ProcessorOutput`.
/// Each metric is namespaced as `mailmux_proc_{processor_name}_{metric_name}`.
pub fn record_processor_metrics(
    processor_name: &str,
    metrics: &[crate::processor::ProcessorMetric],
) {
    use crate::processor::MetricKind;

    for m in metrics {
        let full_name = format!("mailmux_proc_{}_{}", processor_name, m.name);
        let labels: Vec<metrics::Label> = m
            .labels
            .iter()
            .map(|(k, v)| metrics::Label::new(k.clone(), v.clone()))
            .collect();

        match m.kind {
            MetricKind::Counter => {
                counter!(full_name, labels).increment(m.value as u64);
            }
            MetricKind::Gauge => {
                gauge!(full_name, labels).set(m.value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processor::{MetricKind, ProcessorMetric};

    #[test]
    fn processor_labels_survive_the_output_and_reuse_the_same_series() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            for _ in 0..2 {
                let output = vec![ProcessorMetric {
                    name: "posted".into(),
                    kind: MetricKind::Counter,
                    value: 1.0,
                    labels: [(String::from("bank"), String::from("test"))].into(),
                }];
                record_processor_metrics("mailtx", &output);
            }
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("mailmux_proc_mailtx_posted{bank=\"test\"} 2"),
            "{rendered}"
        );
    }
}
