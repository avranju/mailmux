use std::collections::HashMap;

use serde::Serialize;

/// Metrics accumulated during a single mailtx invocation.
///
/// Converted to a [`ProcessorOutput`] JSON object at the end of every run and
/// written to stdout so that mailmux's command processor can register them in
/// its Prometheus endpoint.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Terminal outcome for this invocation.
    pub result: Option<&'static str>,
    /// Result of the LLM call, set whenever a call was attempted.
    pub llm_result: Option<&'static str>,
    /// Firefly API calls made this run: one `(operation, result)` entry per
    /// HTTP request.
    pub firefly_requests: Vec<(&'static str, &'static str)>,
    /// Account-resolution method used, set whenever resolution was attempted.
    pub account_match_method: Option<&'static str>,
    /// Number of transfer legs stored in the pending store this run.
    pub transfers_stored: u32,
    /// Number of transfers fully coalesced (both legs matched) this run.
    pub transfers_coalesced: u32,
    /// Number of expired transfer legs flushed as unmatched transactions this run.
    pub transfers_expired: u32,
    /// Firefly transaction ID from the primary post this run (regular or
    /// coalesced transfer). Set when a transaction is successfully posted.
    pub firefly_transaction_id: Option<String>,
}

/// Typed metadata embedded in ProcessorOutput to record the terminal outcome
/// and any associated Firefly transaction ID.
#[derive(Debug, Serialize)]
pub struct ProcessorMetadata {
    /// Terminal outcome string, e.g. `"posted"`, `"no_transaction"`, `"error"`.
    pub outcome: &'static str,
    /// Firefly transaction ID when a transaction was posted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firefly_transaction_id: Option<String>,
}

/// Mirrors mailmux's `ProcessorOutput` schema.
/// Serialised to stdout so the command processor can forward the metrics.
#[derive(Serialize)]
pub struct ProcessorOutput {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ProcessorMetadata>,
    pub metrics: Vec<ProcessorMetric>,
}

/// Mirrors mailmux's `ProcessorMetric` schema.
#[derive(Serialize)]
pub struct ProcessorMetric {
    pub name: String,
    /// `"counter"` or `"gauge"` — matches `MetricKind` serde output in mailmux.
    pub kind: &'static str,
    pub value: f64,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
}

impl Metrics {
    /// Convert into a [`ProcessorOutput`] ready to be serialised to stdout.
    pub fn into_output(self, success: bool) -> ProcessorOutput {
        let mut metrics: Vec<ProcessorMetric> = Vec::new();

        if let Some(result) = self.result {
            metrics.push(counter(
                "emails_processed_total",
                1.0,
                &[("result", result)],
            ));
        }

        if let Some(result) = self.llm_result {
            metrics.push(counter("llm_calls_total", 1.0, &[("result", result)]));
        }

        for (operation, result) in &self.firefly_requests {
            metrics.push(counter(
                "firefly_requests_total",
                1.0,
                &[("operation", operation), ("result", result)],
            ));
        }

        if let Some(method) = self.account_match_method {
            metrics.push(counter("account_match_total", 1.0, &[("method", method)]));
        }

        if self.transfers_stored > 0 {
            metrics.push(counter(
                "transfer_legs_stored_total",
                self.transfers_stored as f64,
                &[],
            ));
        }

        if self.transfers_coalesced > 0 {
            metrics.push(counter(
                "transfer_coalesced_total",
                self.transfers_coalesced as f64,
                &[],
            ));
        }

        if self.transfers_expired > 0 {
            metrics.push(counter(
                "transfer_expired_total",
                self.transfers_expired as f64,
                &[],
            ));
        }

        // Build outcome metadata from the terminal result.
        let outcome = self.result.unwrap_or("error");
        let metadata = ProcessorMetadata {
            outcome,
            firefly_transaction_id: self.firefly_transaction_id,
        };

        ProcessorOutput {
            success,
            message: None,
            metadata: Some(metadata),
            metrics,
        }
    }
}

fn counter(name: &str, value: f64, labels: &[(&str, &str)]) -> ProcessorMetric {
    ProcessorMetric {
        name: name.to_owned(),
        kind: "counter",
        value,
        labels: labels
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_posted_output_includes_transaction_id() {
        let m = Metrics {
            result: Some("posted"),
            firefly_transaction_id: Some("tx-abc-123".to_string()),
            ..Default::default()
        };

        let output = m.into_output(true);
        assert!(output.success);
        let metadata = output.metadata.expect("metadata should be present");
        assert_eq!(metadata.outcome, "posted");
        assert_eq!(
            metadata.firefly_transaction_id,
            Some("tx-abc-123".to_string())
        );
    }

    #[test]
    fn test_no_transaction_output_no_id() {
        let m = Metrics {
            result: Some("no_transaction"),
            ..Default::default()
        };

        let output = m.into_output(true);
        assert!(output.success);
        let metadata = output.metadata.expect("metadata should be present");
        assert_eq!(metadata.outcome, "no_transaction");
        assert!(metadata.firefly_transaction_id.is_none());
    }

    #[test]
    fn test_error_output_retains_outcome() {
        let m = Metrics {
            result: Some("error"),
            ..Default::default()
        };

        let output = m.into_output(false);
        assert!(!output.success);
        let metadata = output.metadata.expect("metadata should be present");
        assert_eq!(metadata.outcome, "error");
        assert!(metadata.firefly_transaction_id.is_none());
    }

    #[test]
    fn test_skipped_sender_output() {
        let m = Metrics {
            result: Some("skipped_sender"),
            ..Default::default()
        };

        let output = m.into_output(true);
        let metadata = output.metadata.expect("metadata should be present");
        assert_eq!(metadata.outcome, "skipped_sender");
    }

    #[test]
    fn test_transfer_coalesced_output_includes_id() {
        let m = Metrics {
            result: Some("transfer_coalesced"),
            firefly_transaction_id: Some("tx-transfer-456".to_string()),
            transfers_coalesced: 1,
            ..Default::default()
        };

        let output = m.into_output(true);
        let metadata = output.metadata.expect("metadata should be present");
        assert_eq!(metadata.outcome, "transfer_coalesced");
        assert_eq!(
            metadata.firefly_transaction_id,
            Some("tx-transfer-456".to_string())
        );
    }

    #[test]
    fn test_transfer_stored_output_no_id() {
        let m = Metrics {
            result: Some("transfer_stored"),
            transfers_stored: 1,
            ..Default::default()
        };

        let output = m.into_output(true);
        let metadata = output.metadata.expect("metadata should be present");
        assert_eq!(metadata.outcome, "transfer_stored");
        assert!(metadata.firefly_transaction_id.is_none());
    }
}
