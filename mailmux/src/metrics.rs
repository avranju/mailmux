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
