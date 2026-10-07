use tracing::{debug, info};

use super::Processor;
use crate::config::ProcessorConfig;
use crate::db::events::Event;

struct RegisteredProcessor {
    processor: Box<dyn Processor>,
    config: ProcessorConfig,
}

/// Holds processors with their centralized source/subscription configuration.
pub struct ProcessorRegistry {
    processors: Vec<RegisteredProcessor>,
}

impl ProcessorRegistry {
    pub fn from_config(configs: &[ProcessorConfig]) -> Self {
        let mut processors = Vec::new();
        for config in configs {
            if !config.enabled {
                debug!(processor = config.name, "processor disabled, skipping");
                continue;
            }
            let processor: Option<Box<dyn Processor>> = match config.name.as_str() {
                "logger" => Some(Box::new(super::builtin::logger::LoggerProcessor::new(
                    config,
                ))),
                "command" => Some(Box::new(super::builtin::command::CommandProcessor::new(
                    config,
                ))),
                _name if config.config.contains_key("command") => Some(Box::new(
                    super::builtin::command::CommandProcessor::new(config),
                )),
                _ => None,
            };
            if let Some(processor) = processor {
                info!(processor = %config.name, "registered processor");
                processors.push(RegisteredProcessor {
                    processor,
                    config: config.clone(),
                });
            } else {
                info!(processor = %config.name, "unknown processor type, skipping");
            }
        }
        info!(count = processors.len(), "processor registry initialized");
        Self { processors }
    }

    pub fn processors_for_event(&self, event: &Event) -> Vec<&dyn Processor> {
        self.processors
            .iter()
            .filter(|p| p.config.matches_event(event))
            .map(|p| p.processor.as_ref())
            .collect()
    }

    pub fn processor_by_name(&self, name: &str) -> Option<&dyn Processor> {
        self.processors
            .iter()
            .find(|p| p.processor.name() == name)
            .map(|p| p.processor.as_ref())
    }

    pub fn require_processor_for_event(
        &self,
        name: &str,
        event: &Event,
    ) -> anyhow::Result<&dyn Processor> {
        let Some(registered) = self.processors.iter().find(|p| p.processor.name() == name) else {
            anyhow::bail!("processor '{}' is unavailable or disabled", name);
        };
        if !registered
            .config
            .events
            .iter()
            .any(|e| e == &event.event_type)
        {
            anyhow::bail!(
                "processor '{}' is not subscribed to event type '{}'",
                name,
                event.event_type
            );
        }
        if !registered
            .config
            .matches_source(&event.account_id, &event.mailbox_name)
        {
            anyhow::bail!(
                "processor '{}' excludes source account '{}' mailbox '{}'",
                name,
                event.account_id,
                event.mailbox_name
            );
        }
        Ok(registered.processor.as_ref())
    }

    #[cfg(test)]
    pub fn for_tests(processors: Vec<Box<dyn Processor>>) -> Self {
        Self::for_tests_with_configs(
            processors
                .into_iter()
                .map(|processor| {
                    let config = ProcessorConfig {
                        name: processor.name().to_string(),
                        enabled: true,
                        events: processor.subscribed_events().to_vec(),
                        sources: None,
                        max_retries: 0,
                        retry_backoff_secs: vec![],
                        timeout_secs: 30,
                        concurrency: 1,
                        config: Default::default(),
                    };
                    (processor, config)
                })
                .collect(),
        )
    }

    #[cfg(test)]
    pub fn for_tests_with_configs(processors: Vec<(Box<dyn Processor>, ProcessorConfig)>) -> Self {
        Self {
            processors: processors
                .into_iter()
                .filter(|(_, config)| config.enabled)
                .map(|(processor, config)| RegisteredProcessor { processor, config })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::emails::EmailRecord;
    use crate::processor::ProcessorOutput;
    use anyhow::Result;
    use async_trait::async_trait;
    use chrono::Utc;
    use serde_json::json;

    struct TestProcessor {
        name: String,
        events: Vec<String>,
    }

    #[async_trait]
    impl Processor for TestProcessor {
        fn name(&self) -> &str {
            &self.name
        }
        fn subscribed_events(&self) -> &[String] {
            &self.events
        }
        async fn process(&self, _: &Event, _: Option<&EmailRecord>) -> Result<ProcessorOutput> {
            unreachable!()
        }
    }

    fn event(account_id: &str, mailbox_name: &str, event_type: &str) -> Event {
        Event {
            id: 1,
            event_type: event_type.into(),
            account_id: account_id.into(),
            mailbox_name: mailbox_name.into(),
            email_id: None,
            payload: json!({}),
            created_at: Utc::now(),
        }
    }

    fn registration(
        name: &str,
        enabled: bool,
        events: &[&str],
        sources: Option<Vec<crate::config::ProcessorSource>>,
    ) -> (Box<dyn Processor>, ProcessorConfig) {
        let events: Vec<String> = events.iter().map(|e| (*e).into()).collect();
        let processor = Box::new(TestProcessor {
            name: name.into(),
            events: events.clone(),
        });
        let config = ProcessorConfig {
            name: name.into(),
            enabled,
            events,
            sources,
            max_retries: 0,
            retry_backoff_secs: vec![],
            timeout_secs: 30,
            concurrency: 1,
            config: Default::default(),
        };
        (processor, config)
    }

    fn source(account: &str, mailboxes: Option<&[&str]>) -> crate::config::ProcessorSource {
        crate::config::ProcessorSource {
            account: account.into(),
            mailboxes: mailboxes.map(|xs| xs.iter().map(|x| (*x).into()).collect()),
        }
    }

    #[test]
    fn named_and_list_eligibility_share_exact_paired_source_rules() {
        let registry = ProcessorRegistry::for_tests_with_configs(vec![
            registration("all", true, &["email_arrived"], None),
            registration(
                "account",
                true,
                &["email_arrived"],
                Some(vec![source("personal", None)]),
            ),
            registration(
                "paired",
                true,
                &["email_arrived"],
                Some(vec![
                    source("personal", Some(&["INBOX", "Archive"])),
                    source("work", Some(&["INBOX"])),
                ]),
            ),
            registration(
                "overlap",
                true,
                &["email_arrived"],
                Some(vec![
                    source("personal", None),
                    source("personal", Some(&["INBOX"])),
                ]),
            ),
            registration("wrong-type", true, &["other"], None),
            registration("disabled", false, &["email_arrived"], None),
        ]);
        let inbox = event("personal", "INBOX", "email_arrived");
        let names: Vec<_> = registry
            .processors_for_event(&inbox)
            .iter()
            .map(|p| p.name())
            .collect();
        assert_eq!(names, ["all", "account", "paired", "overlap"]);
        for name in ["all", "account", "paired", "overlap"] {
            assert_eq!(
                registry
                    .require_processor_for_event(name, &inbox)
                    .unwrap()
                    .name(),
                name
            );
        }
        assert!(
            registry
                .require_processor_for_event("disabled", &inbox)
                .is_err()
        );
        assert!(
            registry
                .require_processor_for_event("missing", &inbox)
                .is_err()
        );
        assert!(
            registry
                .require_processor_for_event("wrong-type", &inbox)
                .err()
                .unwrap()
                .to_string()
                .contains("not subscribed")
        );
        assert!(
            registry
                .require_processor_for_event("paired", &event("personal", "inbox", "email_arrived"))
                .is_err()
        );
        assert!(
            registry
                .require_processor_for_event("paired", &event("Personal", "INBOX", "email_arrived"))
                .is_err()
        );
        assert!(
            registry
                .require_processor_for_event(
                    "paired",
                    &event("personal", "Archive", "email_arrived")
                )
                .is_ok()
        );
        assert!(
            registry
                .require_processor_for_event("paired", &event("work", "INBOX", "email_arrived"))
                .is_ok()
        );
        assert_eq!(
            registry
                .processors_for_event(&event("personal", "INBOX", "email_deleted"))
                .len(),
            0
        );
    }
}
