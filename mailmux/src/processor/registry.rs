use tracing::{debug, info};

use super::Processor;
use crate::config::ProcessorConfig;

/// Holds all registered processors and matches events to them.
pub struct ProcessorRegistry {
    processors: Vec<Box<dyn Processor>>,
}

impl ProcessorRegistry {
    /// Build the registry from config, instantiating known built-in processors.
    pub fn from_config(configs: &[ProcessorConfig]) -> Self {
        let mut processors: Vec<Box<dyn Processor>> = Vec::new();

        for config in configs {
            if !config.enabled {
                debug!(processor = config.name, "processor disabled, skipping");
                continue;
            }

            match config.name.as_str() {
                "logger" => {
                    processors.push(Box::new(
                        super::builtin::logger::LoggerProcessor::new(config),
                    ));
                    info!(processor = "logger", "registered built-in logger processor");
                }
                "command" => {
                    processors.push(Box::new(
                        super::builtin::command::CommandProcessor::new(config),
                    ));
                    info!(processor = "command", "registered command processor");
                }
                name => {
                    // Treat unknown processor types as command processors
                    // if they have a "command" in their config
                    if config.config.contains_key("command") {
                        processors.push(Box::new(
                            super::builtin::command::CommandProcessor::new(config),
                        ));
                        info!(processor = name, "registered as command processor");
                    } else {
                        info!(processor = name, "unknown processor type, skipping");
                    }
                }
            }
        }

        info!(count = processors.len(), "processor registry initialized");
        Self { processors }
    }

    /// Find all processors that are subscribed to the given event type.
    pub fn processors_for_event(&self, event_type: &str) -> Vec<&dyn Processor> {
        self.processors
            .iter()
            .filter(|p| p.subscribed_events().iter().any(|e| e == event_type))
            .map(|p| p.as_ref())
            .collect()
    }

}
