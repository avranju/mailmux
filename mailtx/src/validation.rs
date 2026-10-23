use crate::{endpoint::CanonicalTransaction, llm::TransactionData};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ValidationConfig {
    pub enabled: bool,
    pub double_extraction: bool,
    pub date_window_days: u32,
    pub regex_tolerance_percent: u32,
    pub max_amounts: BTreeMap<String, String>,
}
impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            double_extraction: true,
            date_window_days: 7,
            regex_tolerance_percent: 5,
            max_amounts: BTreeMap::from([("INR".into(), "500000".into())]),
        }
    }
}
impl ValidationConfig {
    pub fn validate(&self) -> Result<()> {
        if self.date_window_days > 365 || self.regex_tolerance_percent > 100 {
            anyhow::bail!("validation date window or tolerance is too large");
        }
        for (currency, amount) in &self.max_amounts {
            let amount =
                Decimal::from_str_exact(amount).context("invalid validation amount limit")?;
            if currency.len() != 3
                || !currency.bytes().all(|c| c.is_ascii_uppercase())
                || amount <= Decimal::ZERO
            {
                anyhow::bail!(
                    "validation limits require uppercase currencies and positive amounts"
                );
            }
            crate::money::minor_units(amount)?;
        }
        Ok(())
    }
}

pub fn reasons(
    data: &TransactionData,
    email_date: Option<DateTime<Utc>>,
    currency: Option<&str>,
    timezone: chrono_tz::Tz,
    body: &str,
    config: &ValidationConfig,
) -> Result<Vec<&'static str>> {
    let mut reasons = vec![];
    let Some(amount) = data.amount else {
        return Ok(vec!["missing_amount"]);
    };
    if amount.is_zero() {
        reasons.push("zero_amount");
    }
    if data
        .narration
        .as_deref()
        .is_none_or(|s| s.trim().is_empty())
    {
        reasons.push("missing_narration");
    }
    let date = data.transaction_date.as_deref().filter(|raw| {
        DateTime::parse_from_rfc3339(raw).is_ok()
            || chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d").is_ok()
    });
    match date {
        None => reasons.push("missing_or_invalid_transaction_date"),
        Some(raw) => match crate::endpoint::parse_transaction_datetime(Some(raw), None, timezone) {
            Err(_) => reasons.push("invalid_local_transaction_date"),
            Ok(date) => match email_date {
                None => reasons.push("missing_email_date"),
                Some(email_date) => {
                    if (date.with_timezone(&timezone).date_naive()
                        - email_date.with_timezone(&timezone).date_naive())
                    .num_days()
                    .unsigned_abs()
                        > config.date_window_days as u64
                    {
                        reasons.push("date_outside_email_window");
                    }
                }
            },
        },
    }
    match currency {
        None => reasons.push("unknown_account_currency"),
        Some(currency) => {
            if let Some(limit) = config.max_amounts.get(currency)
                && amount.abs() > Decimal::from_str_exact(limit)?
            {
                reasons.push("amount_above_limit");
            }
        }
    }
    let money =
        regex::Regex::new(r"(?i)(INR|Rs\.?|₹|USD|\$|EUR|€|GBP|£)\s*([0-9][0-9,]*(?:\.[0-9]+)?)")?;
    let mut prominent = None;
    for capture in money.captures_iter(body) {
        let code = capture[1].to_ascii_uppercase();
        let actual = match code.as_str() {
            "RS" | "RS." | "₹" => "INR",
            "$" => "USD",
            "€" => "EUR",
            "£" => "GBP",
            other => other,
        };
        if currency.is_some_and(|expected| expected != actual)
            && !reasons.contains(&"unexpected_email_currency")
        {
            reasons.push("unexpected_email_currency");
        }
        // Ignore explicitly labelled balances for the amount cross-check.
        let start = capture.get(0).unwrap().start();
        let line_prefix = body[..start]
            .rsplit('\n')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if prominent.is_none()
            && !line_prefix.contains("balance")
            && !line_prefix.contains("avl bal")
        {
            prominent = Some(Decimal::from_str_exact(&capture[2].replace(',', ""))?);
        }
    }
    if let Some(evidence) = prominent {
        let expected = crate::money::minor_units(amount)? as i128;
        let observed = crate::money::minor_units(evidence)? as i128;
        let tolerance = (expected * config.regex_tolerance_percent as i128 / 100).max(1);
        if (expected - observed).abs() > tolerance {
            reasons.push("email_amount_disagrees");
        }
    }
    Ok(reasons)
}

pub fn independent_disagrees(first: &TransactionData, second: &TransactionData) -> bool {
    second.status != "found"
        || first.amount.map(|v| v.abs()) != second.amount.map(|v| v.abs())
        || first.transaction_type != second.transaction_type
}

pub fn pending_review(path: &str, external_id: &str) -> Result<Option<Vec<String>>> {
    use rusqlite::OptionalExtension;
    let db = rusqlite::Connection::open(path)?;
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='review_queue')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    db.query_row(
        "SELECT reasons FROM review_queue WHERE external_id=?1",
        [external_id],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|json| serde_json::from_str(&json).context("parsing queued review reasons"))
    .transpose()
}

pub fn hold(
    path: &str,
    external_id: &str,
    reasons: &[&str],
    tx: Option<&CanonicalTransaction>,
    raw_message_path: &str,
) -> Result<()> {
    let db = rusqlite::Connection::open(path).context("opening review queue")?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS review_queue (external_id TEXT PRIMARY KEY,reasons TEXT NOT NULL,transaction_json TEXT,raw_message_path TEXT NOT NULL,created_at INTEGER NOT NULL);")?;
    let payload=tx.map(|tx|serde_json::json!({"amount":tx.amount.to_string(),"currency_code":tx.currency_code,"narration":tx.narration,"date":tx.occurred_at.to_rfc3339(),"account":tx.asset_account_id,"tags":tx.tags})).map(|value|value.to_string());
    db.execute("INSERT INTO review_queue VALUES (?1,?2,?3,?4,?5) ON CONFLICT(external_id) DO UPDATE SET reasons=excluded.reasons,transaction_json=excluded.transaction_json,raw_message_path=excluded.raw_message_path",rusqlite::params![external_id,serde_json::to_string(reasons)?,payload,raw_message_path,Utc::now().timestamp()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn data() -> TransactionData {
        crate::llm::parse_transaction(r#"{"status":"found","amount":100,"transaction_type":"withdrawal","narration":"Shop","transaction_date":"2026-01-05","category":null}"#).unwrap()
    }
    #[test]
    fn amount_date_currency_and_independent_checks_are_exact() {
        let config = ValidationConfig::default();
        let date = Some("2026-01-05T10:00:00Z".parse().unwrap());
        assert!(
            reasons(
                &data(),
                date,
                Some("INR"),
                chrono_tz::UTC,
                "Paid ₹100; closing balance ₹900",
                &config
            )
            .unwrap()
            .is_empty()
        );
        let mut bad = data();
        bad.amount = Some(Decimal::from(500001));
        bad.transaction_date = Some("2025-01-01".into());
        bad.narration = None;
        let found = reasons(
            &bad,
            date,
            Some("INR"),
            chrono_tz::UTC,
            "Paid USD 100",
            &config,
        )
        .unwrap();
        for reason in [
            "amount_above_limit",
            "date_outside_email_window",
            "missing_narration",
            "unexpected_email_currency",
            "email_amount_disagrees",
        ] {
            assert!(found.contains(&reason));
        }
        assert!(independent_disagrees(&bad, &data()));
        assert!(!independent_disagrees(&data(), &data()));
        let mut absent = data();
        absent.transaction_date = None;
        assert!(
            reasons(&absent, date, None, chrono_tz::UTC, "", &config)
                .unwrap()
                .contains(&"missing_or_invalid_transaction_date")
        );
        assert!(
            reasons(&data(), None, Some("INR"), chrono_tz::UTC, "", &config)
                .unwrap()
                .contains(&"missing_email_date")
        );
    }
}
