use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;

use crate::config::Config;
use crate::llm::TransactionData;

pub mod firefly;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionKind {
    Withdrawal,
    Deposit,
    /// An inter-account transfer. `asset_account_id` is the source; the
    /// destination is in `transfer_destination_account_id`.
    Transfer,
}

#[derive(Debug, Clone)]
pub struct CanonicalTransaction {
    pub amount: Decimal,
    pub currency_code: Option<String>,
    pub kind: TransactionKind,
    pub narration: String,
    pub occurred_at: DateTime<FixedOffset>,
    /// For Withdrawal/Transfer: the source asset account Firefly ID.
    /// For Deposit: the destination asset account Firefly ID.
    pub asset_account_id: String,
    /// Firefly account ID of the transfer destination. Only set when `kind == Transfer`.
    pub transfer_destination_account_id: Option<String>,
    pub tags: Vec<String>,
    pub category_name: Option<String>,
    /// Stable identity of the mailmux event that caused this write.
    pub external_id: Option<String>,
}

impl CanonicalTransaction {
    /// Stable ordering, trimmed nonempty tags, and no duplicates across sources.
    pub fn extend_tags(&mut self, values: impl IntoIterator<Item = String>) {
        self.tags.extend(values);
        let mut seen = std::collections::HashSet::new();
        self.tags = self
            .tags
            .drain(..)
            .map(|tag| tag.trim().to_string())
            .filter(|tag| !tag.is_empty() && seen.insert(tag.clone()))
            .collect();
    }
}

pub struct FetchedCategories {
    pub names: Vec<String>,
    pub request_count: usize,
}

pub struct PostReceipt {
    pub id: Option<String>,
}

#[allow(clippy::double_must_use)] // async_trait futures and Result are both must-use.
#[async_trait]
pub trait TransactionEndpoint: Send + Sync {
    fn name(&self) -> &'static str;
    async fn post_transaction(
        &self,
        client: &reqwest::Client,
        tx: &CanonicalTransaction,
    ) -> Result<PostReceipt>;

    /// Fetch the list of existing category names from the endpoint.
    /// Returns an empty vec by default for endpoints that don't support categories.
    async fn fetch_categories(&self, client: &reqwest::Client) -> Result<FetchedCategories> {
        let _ = client;
        Ok(FetchedCategories {
            names: vec![],
            request_count: 0,
        })
    }
}

pub fn build_endpoint(config: &Config) -> Box<dyn TransactionEndpoint> {
    Box::new(firefly::FireflyEndpoint::from_config(&config.firefly))
}

pub fn canonical_from_llm(
    data: &TransactionData,
    asset_account_id: String,
    tag: String,
    email_date: Option<DateTime<Utc>>,
    category_name: Option<String>,
    external_id: Option<String>,
    timezone: chrono_tz::Tz,
) -> Result<CanonicalTransaction> {
    if data.status != "found" {
        anyhow::bail!("LLM status must be 'found' before posting");
    }

    let amount = data
        .amount
        .ok_or_else(|| anyhow::anyhow!("LLM returned status=found but amount is null"))?;
    if amount.is_zero() {
        anyhow::bail!("transaction amount must be nonzero");
    }
    crate::money::minor_units(amount)?;
    let tx_type = data
        .transaction_type
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("LLM returned status=found but transaction_type is null"))?;

    let kind = match tx_type {
        "withdrawal" => TransactionKind::Withdrawal,
        "deposit" => TransactionKind::Deposit,
        other => anyhow::bail!("unsupported transaction_type from LLM: {other}"),
    };

    let narration = data
        .narration
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Bank transaction")
        .to_string();
    let occurred_at =
        parse_transaction_datetime(data.transaction_date.as_deref(), email_date, timezone)?;

    Ok(CanonicalTransaction {
        amount,
        currency_code: None,
        kind,
        narration,
        occurred_at,
        asset_account_id,
        transfer_destination_account_id: None,
        tags: vec![tag],
        category_name,
        external_id,
    })
}

pub fn parse_transaction_datetime(
    raw: Option<&str>,
    fallback: Option<DateTime<Utc>>,
    timezone: chrono_tz::Tz,
) -> Result<DateTime<FixedOffset>> {
    let fallback = || {
        fallback
            .unwrap_or_else(Utc::now)
            .with_timezone(&timezone)
            .fixed_offset()
    };
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(fallback());
    };
    if let Ok(datetime) = DateTime::parse_from_rfc3339(raw) {
        return Ok(datetime);
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return timezone
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
            .single()
            .map(|datetime| datetime.fixed_offset())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "transaction date is ambiguous or nonexistent in the configured timezone"
                )
            });
    }
    Ok(fallback())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn date_only_preserves_bank_calendar_date_and_explicit_offsets() {
        let india =
            parse_transaction_datetime(Some("2026-01-05"), None, chrono_tz::Asia::Kolkata).unwrap();
        assert_eq!(india.to_rfc3339(), "2026-01-05T00:00:00+05:30");
        let new_york =
            parse_transaction_datetime(Some("2026-07-05"), None, chrono_tz::America::New_York)
                .unwrap();
        assert_eq!(new_york.to_rfc3339(), "2026-07-05T00:00:00-04:00");
        let explicit =
            parse_transaction_datetime(Some("2026-01-05T17:00:00+02:00"), None, chrono_tz::UTC)
                .unwrap();
        assert_eq!(explicit.to_rfc3339(), "2026-01-05T17:00:00+02:00");
        assert!(
            parse_transaction_datetime(Some("2011-12-30"), None, chrono_tz::Pacific::Apia).is_err()
        );
    }
}
