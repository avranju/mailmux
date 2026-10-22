use crate::endpoint::CanonicalTransaction;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaggingConfig {
    pub source: bool,
    pub month: bool,
    pub workflow: bool,
}
impl Default for TaggingConfig {
    fn default() -> Self {
        Self {
            source: true,
            month: true,
            workflow: true,
        }
    }
}

pub fn apply(tx: &mut CanonicalTransaction, source: &str, config: &TaggingConfig) {
    let mut tags = vec![];
    let slug = source
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if config.source && !slug.is_empty() {
        tags.push(format!("mailmux-{slug}"));
    }
    if config.month {
        tags.push(format!("mailmux-{}", tx.occurred_at.format("%Y-%m")));
    }
    if config.workflow && tx.category_name.is_some() {
        tags.push("auto-categorized".into());
    }
    tx.extend_tags(tags);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tags_follow_local_booking_month_and_do_not_claim_reconciliation() {
        let data = crate::llm::parse_transaction(r#"{"status":"found","amount":1,"transaction_type":"withdrawal","narration":"Shop","transaction_date":"2026-03-01T00:01:00+05:30","category":"Food"}"#).unwrap();
        let mut tx = crate::endpoint::canonical_from_llm(
            &data,
            "1".into(),
            "mailmux-mailtx".into(),
            None,
            Some("Food".into()),
            None,
            chrono_tz::UTC,
        )
        .unwrap();
        apply(&mut tx, "HDFC Salary", &TaggingConfig::default());
        assert_eq!(
            tx.tags,
            [
                "mailmux-mailtx",
                "mailmux-hdfc-salary",
                "mailmux-2026-03",
                "auto-categorized"
            ]
        );
        apply(&mut tx, "HDFC Salary", &TaggingConfig::default());
        assert_eq!(tx.tags.len(), 4);
        tx.tags.clear();
        apply(
            &mut tx,
            "HDFC",
            &TaggingConfig {
                source: false,
                month: false,
                workflow: false,
            },
        );
        assert!(tx.tags.is_empty());
    }
}
