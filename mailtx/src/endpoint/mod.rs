use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};

use crate::config::Config;
use crate::llm::TransactionData;

pub mod firefly;

#[derive(Debug, Clone, Copy)]
pub enum TransactionKind {
    Withdrawal,
    Deposit,
}

#[derive(Debug, Clone)]
pub struct CanonicalTransaction {
    pub amount: f64,
    pub kind: TransactionKind,
    pub narration: String,
    pub occurred_at: DateTime<Utc>,
}

pub struct PostReceipt {
    pub id: Option<String>,
}

#[async_trait]
pub trait TransactionEndpoint: Send + Sync {
    fn name(&self) -> &'static str;
    async fn post_transaction(
        &self,
        client: &reqwest::Client,
        tx: &CanonicalTransaction,
    ) -> Result<PostReceipt>;
}

pub fn build_endpoint(config: &Config) -> Box<dyn TransactionEndpoint> {
    Box::new(firefly::FireflyEndpoint::from_config(&config.firefly))
}

pub fn canonical_from_llm(data: &TransactionData) -> Result<CanonicalTransaction> {
    if data.status != "found" {
        anyhow::bail!("LLM status must be 'found' before posting");
    }

    let amount = data
        .amount
        .ok_or_else(|| anyhow::anyhow!("LLM returned status=found but amount is null"))?;
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
    let occurred_at = parse_transaction_datetime(data.transaction_date.as_deref());

    Ok(CanonicalTransaction {
        amount,
        kind,
        narration,
        occurred_at,
    })
}

fn parse_transaction_datetime(raw: Option<&str>) -> DateTime<Utc> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Utc::now();
    };

    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return dt.with_timezone(&Utc);
    }

    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        && let Some(naive_dt) = date.and_hms_opt(0, 0, 0)
    {
        return DateTime::<Utc>::from_naive_utc_and_offset(naive_dt, Utc);
    }

    Utc::now()
}
