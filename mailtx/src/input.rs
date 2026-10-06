use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

/// The JSON payload written to stdin by mailmux's command processor.
/// Only fields we actually use are declared; serde ignores the rest.
#[derive(Debug, Deserialize)]
pub struct Input {
    pub event: Event,
    pub email: Option<EmailRecord>,
}

impl Input {
    /// Keep persisted event identities compatible with existing transactions.
    /// Transient backfill events share id 0, so use the durable email identity.
    pub fn external_id(&self) -> Result<String> {
        if self.event.payload.backfill || self.event.id == 0 {
            let email_id = self
                .email
                .as_ref()
                .and_then(|email| email.id)
                .filter(|id| *id > 0)
                .context("backfill requires a positive email.id for transaction identity")?;
            Ok(format!("mailmux:email:{email_id}"))
        } else {
            Ok(format!("mailmux:event:{}", self.event.id))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct Event {
    pub id: i64,
    #[serde(default)]
    pub payload: EventPayload,
}

#[derive(Debug, Default, Deserialize)]
pub struct EventPayload {
    #[serde(default)]
    pub backfill: bool,
}

#[derive(Debug, Deserialize)]
pub struct EmailRecord {
    pub id: Option<i64>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub date: Option<DateTime<Utc>>,
    pub raw_message_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backfill_input(email_id: serde_json::Value) -> Input {
        serde_json::from_value(serde_json::json!({
            "event": {"id": 0, "payload": {"backfill": true}},
            "email": {"id": email_id, "raw_message_path": "/tmp/test.eml"}
        }))
        .unwrap()
    }

    #[test]
    fn backfill_identity_is_distinct_per_email_and_stable_on_rerun() {
        let first = backfill_input(serde_json::json!(42)).external_id().unwrap();
        let second = backfill_input(serde_json::json!(43)).external_id().unwrap();
        assert_eq!(first, "mailmux:email:42");
        assert_eq!(second, "mailmux:email:43");
        assert_ne!(first, second);
        assert_eq!(
            first,
            backfill_input(serde_json::json!(42)).external_id().unwrap()
        );
    }

    #[test]
    fn normal_event_identity_preserves_legacy_input_and_existing_keys() {
        let input: Input = serde_json::from_value(serde_json::json!({
            "event": {"id": 42},
            "email": {"raw_message_path": "/tmp/test.eml"}
        }))
        .unwrap();
        assert_eq!(input.external_id().unwrap(), "mailmux:event:42");
        assert_ne!(
            input.external_id().unwrap(),
            backfill_input(serde_json::json!(42)).external_id().unwrap()
        );
    }

    #[test]
    fn backfill_rejects_missing_or_invalid_email_identity() {
        for id in [
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(-1),
        ] {
            assert!(backfill_input(id).external_id().is_err());
        }
    }

    #[test]
    fn unmarked_transient_event_cannot_fall_back_to_shared_event_identity() {
        let input: Input = serde_json::from_value(serde_json::json!({
            "event": {"id": 0}, "email": null
        }))
        .unwrap();
        assert!(input.external_id().is_err());
    }
}
