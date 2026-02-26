pub mod builtin;
pub mod registry;
pub mod scheduler;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::db::emails::EmailRecord;
use crate::db::events::Event;

/// Output from a processor execution.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessorOutput {
    pub success: bool,
    pub message: Option<String>,
    pub metadata: Option<serde_json::Value>,
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
