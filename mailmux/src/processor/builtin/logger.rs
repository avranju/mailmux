use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

use crate::config::ProcessorConfig;
use crate::db::emails::EmailRecord;
use crate::db::events::Event;
use crate::processor::{Processor, ProcessorOutput};

/// A simple processor that logs event details.
/// Serves as a reference implementation and testing tool.
pub struct LoggerProcessor {
    name: String,
    events: Vec<String>,
}

impl LoggerProcessor {
    pub fn new(config: &ProcessorConfig) -> Self {
        Self {
            name: config.name.clone(),
            events: config.events.clone(),
        }
    }
}

#[async_trait]
impl Processor for LoggerProcessor {
    fn name(&self) -> &str {
        &self.name
    }

    fn subscribed_events(&self) -> &[String] {
        &self.events
    }

    async fn process(&self, event: &Event, email: Option<&EmailRecord>) -> Result<ProcessorOutput> {
        info!(
            processor = self.name,
            event_id = event.id,
            event_type = event.event_type,
            account = event.account_id,
            mailbox = event.mailbox_name,
            email_id = ?event.email_id,
            subject = ?email.and_then(|e| e.subject.as_deref()),
            sender = ?email.and_then(|e| e.sender.as_deref()),
            payload = %event.payload,
            "logger processor: event received"
        );

        Ok(ProcessorOutput {
            success: true,
            message: Some("logged".to_string()),
            metadata: None,
        })
    }
}
