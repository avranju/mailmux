use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::FireflyConfig;
use crate::endpoint::{CanonicalTransaction, PostReceipt, TransactionEndpoint, TransactionKind};

pub struct FireflyEndpoint {
    base_url: String,
    access_token: String,
    currency_code: Option<String>,
    apply_rules: bool,
    fire_webhooks: bool,
    error_if_duplicate_hash: bool,
}

impl FireflyEndpoint {
    pub fn from_config(config: &FireflyConfig) -> Self {
        Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            access_token: config.access_token.trim().to_string(),
            currency_code: config.currency_code.clone(),
            apply_rules: config.apply_rules,
            fire_webhooks: config.fire_webhooks,
            error_if_duplicate_hash: config.error_if_duplicate_hash,
        }
    }

    fn url(&self) -> String {
        format!("{}/v1/transactions", self.base_url)
    }

    fn authorization_header_value(&self) -> String {
        if self.access_token.starts_with("Bearer ") {
            self.access_token.clone()
        } else {
            format!("Bearer {}", self.access_token)
        }
    }

    fn request_payload<'a>(&'a self, tx: &'a CanonicalTransaction) -> TransactionStore<'a> {
        let occurred_at = tx.occurred_at.to_rfc3339();
        let amount = format!("{:.2}", tx.amount.abs());
        let description = tx.narration.as_str();

        let split = match tx.kind {
            TransactionKind::Withdrawal => TransactionSplitStore {
                tx_type: "withdrawal",
                date: occurred_at.clone(),
                amount,
                description,
                source_id: Some(tx.asset_account_id.as_str()),
                source_name: None,
                destination_id: None,
                destination_name: None,
                currency_code: self.currency_code.as_deref(),
                tags: tx.tags.as_slice(),
            },
            TransactionKind::Deposit => TransactionSplitStore {
                tx_type: "deposit",
                date: occurred_at,
                amount,
                description,
                source_id: None,
                source_name: None,
                destination_id: Some(tx.asset_account_id.as_str()),
                destination_name: None,
                currency_code: self.currency_code.as_deref(),
                tags: tx.tags.as_slice(),
            },
        };

        TransactionStore {
            error_if_duplicate_hash: self.error_if_duplicate_hash,
            apply_rules: self.apply_rules,
            fire_webhooks: self.fire_webhooks,
            transactions: vec![split],
        }
    }
}

#[async_trait]
impl TransactionEndpoint for FireflyEndpoint {
    fn name(&self) -> &'static str {
        "firefly"
    }

    async fn post_transaction(
        &self,
        client: &reqwest::Client,
        tx: &CanonicalTransaction,
    ) -> Result<PostReceipt> {
        let payload = self.request_payload(tx);
        let response = client
            .post(self.url())
            .header("Authorization", self.authorization_header_value())
            .header("Accept", "application/vnd.api+json")
            .json(&payload)
            .send()
            .await
            .context("posting transaction to Firefly endpoint")?;

        if response.status() != reqwest::StatusCode::OK {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("firefly endpoint returned {status}: {body}");
        }

        let data: FireflyTransactionSingle = response
            .json()
            .await
            .context("parsing Firefly success response")?;

        Ok(PostReceipt {
            id: Some(data.data.id),
        })
    }
}

#[derive(Serialize)]
struct TransactionStore<'a> {
    error_if_duplicate_hash: bool,
    apply_rules: bool,
    fire_webhooks: bool,
    transactions: Vec<TransactionSplitStore<'a>>,
}

#[derive(Serialize)]
struct TransactionSplitStore<'a> {
    #[serde(rename = "type")]
    tx_type: &'a str,
    date: String,
    amount: String,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    currency_code: Option<&'a str>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    tags: &'a [String],
}

#[derive(Deserialize)]
struct FireflyTransactionSingle {
    data: FireflyTransactionRead,
}

#[derive(Deserialize)]
struct FireflyTransactionRead {
    id: String,
}
