pub mod builtin;
pub mod registry;
pub mod scheduler;

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::db::emails::EmailRecord;
use crate::db::events::Event;

/// The kind of metric a processor wants to emit.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Counter,
    Gauge,
}

/// A single metric emitted by a processor as part of its output.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessorMetric {
    /// Metric name, e.g. `"notifications_sent"`. Will be namespaced by the scheduler.
    pub name: String,
    pub kind: MetricKind,
    pub value: f64,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// Output from a processor execution.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessorOutput {
    pub success: bool,
    pub message: Option<String>,
    pub metadata: Option<serde_json::Value>,
    /// Optional metrics to record after this run.
    #[serde(default)]
    pub metrics: Vec<ProcessorMetric>,
}

/// Trait that all processors must implement.
#[async_trait]
pub trait Processor: Send + Sync {
    /// The processor's unique name.
    fn name(&self) -> &str;

    /// Which event types this processor is interested in.
    fn subscribed_events(&self) -> &[String];

    /// Process a single event. May optionally receive the associated email record.
    async fn process(&self, event: &Event, email: Option<&EmailRecord>) -> Result<ProcessorOutput>;
}
