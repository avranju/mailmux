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

    async fn transaction_exists(
        &self,
        client: &reqwest::Client,
        external_id: &str,
    ) -> Result<bool> {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/v1/search/transactions/count", self.base_url),
            &[("external_identifier", external_id)],
        )
        .context("building Firefly external-ID lookup URL")?;
        let response = client
            .get(url)
            .header("Authorization", self.authorization_header_value())
            .header("Accept", "application/vnd.api+json")
            .send()
            .await
            .context("checking Firefly transaction external ID")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Firefly external-ID lookup returned {status}: {body}");
        }

        #[derive(Deserialize)]
        struct CountResponse {
            count: u64,
        }
        let result: CountResponse = response
            .json()
            .await
            .context("parsing Firefly external-ID lookup response")?;
        Ok(result.count > 0)
    }

    fn request_payload<'a>(&'a self, tx: &'a CanonicalTransaction) -> Result<TransactionStore<'a>> {
        let occurred_at = tx.occurred_at.to_rfc3339();
        let amount = format!("{:.2}", tx.amount.abs());
        let description = tx.narration.as_str();

        let split = match tx.kind {
            TransactionKind::Withdrawal => TransactionSplitStore {
                tx_type: "withdrawal",
                date: occurred_at,
                amount,
                description,
                source_id: Some(tx.asset_account_id.as_str()),
                source_name: None,
                destination_id: None,
                destination_name: None,
                currency_code: self.currency_code.as_deref(),
                tags: tx.tags.as_slice(),
                category_name: tx.category_name.as_deref(),
                external_id: tx.external_id.as_deref(),
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
                category_name: tx.category_name.as_deref(),
                external_id: tx.external_id.as_deref(),
            },
            TransactionKind::Transfer => {
                let destination_id =
                    tx.transfer_destination_account_id
                        .as_deref()
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "Transfer transaction is missing transfer_destination_account_id"
                            )
                        })?;
                TransactionSplitStore {
                    tx_type: "transfer",
                    date: occurred_at,
                    amount,
                    description,
                    source_id: Some(tx.asset_account_id.as_str()),
                    source_name: None,
                    destination_id: Some(destination_id),
                    destination_name: None,
                    currency_code: self.currency_code.as_deref(),
                    tags: tx.tags.as_slice(),
                    category_name: tx.category_name.as_deref(),
                    external_id: tx.external_id.as_deref(),
                }
            }
        };

        Ok(TransactionStore {
            error_if_duplicate_hash: self.error_if_duplicate_hash,
            apply_rules: self.apply_rules,
            fire_webhooks: self.fire_webhooks,
            transactions: vec![split],
        })
    }
}

#[async_trait]
impl TransactionEndpoint for FireflyEndpoint {
    fn name(&self) -> &'static str {
        "firefly"
    }

    async fn fetch_categories(&self, client: &reqwest::Client) -> Result<Vec<String>> {
        let mut categories = Vec::new();
        let mut page = 1u32;

        loop {
            let url = format!("{}/v1/categories?page={page}", self.base_url);
            let response = client
                .get(&url)
                .header("Authorization", self.authorization_header_value())
                .header("Accept", "application/vnd.api+json")
                .send()
                .await
                .context("fetching categories from Firefly")?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("firefly categories endpoint returned {status}: {body}");
            }

            let data: FireflyCategoryList = response
                .json()
                .await
                .context("parsing Firefly categories response")?;

            for item in &data.data {
                categories.push(item.attributes.name.clone());
            }

            // Stop when we've fetched all pages.
            if data.meta.pagination.current_page >= data.meta.pagination.total_pages {
                break;
            }
            page += 1;
        }

        Ok(categories)
    }

    async fn post_transaction(
        &self,
        client: &reqwest::Client,
        tx: &CanonicalTransaction,
    ) -> Result<PostReceipt> {
        if let Some(external_id) = tx.external_id.as_deref()
            && self.transaction_exists(client, external_id).await?
        {
            return Ok(PostReceipt { id: None });
        }

        let payload = self.request_payload(tx)?;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    category_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct FireflyTransactionSingle {
    data: FireflyTransactionRead,
}

#[derive(Deserialize)]
struct FireflyTransactionRead {
    id: String,
}

#[derive(Deserialize)]
struct FireflyCategoryList {
    data: Vec<FireflyCategoryRead>,
    meta: FireflyMeta,
}

#[derive(Deserialize)]
struct FireflyCategoryRead {
    attributes: FireflyCategoryAttributes,
}

#[derive(Deserialize)]
struct FireflyCategoryAttributes {
    name: String,
}

#[derive(Deserialize)]
struct FireflyMeta {
    pagination: FireflyPagination,
}

#[derive(Deserialize)]
struct FireflyPagination {
    current_page: u32,
    total_pages: u32,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::FireflyEndpoint;
    use crate::config::FireflyConfig;
    use crate::endpoint::{CanonicalTransaction, TransactionEndpoint, TransactionKind};

    fn endpoint() -> FireflyEndpoint {
        FireflyEndpoint::from_config(&FireflyConfig {
            base_url: "https://firefly.example/api".to_string(),
            access_token: "token".to_string(),
            allow_insecure_http: false,
            asset_accounts: vec![],
            default_asset_account_id: None,
            currency_code: None,
            apply_rules: false,
            fire_webhooks: true,
            error_if_duplicate_hash: true,
        })
    }

    fn transaction(external_id: &str) -> CanonicalTransaction {
        CanonicalTransaction {
            amount: 12.34,
            kind: TransactionKind::Withdrawal,
            narration: "Test purchase".to_string(),
            occurred_at: Utc::now(),
            asset_account_id: "42".to_string(),
            transfer_destination_account_id: None,
            tags: vec!["mailmux-mailtx".to_string()],
            category_name: None,
            external_id: Some(external_id.to_string()),
        }
    }

    #[tokio::test]
    async fn existing_external_id_skips_the_post() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 1024];
                let count = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"count\":1}")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let endpoint = FireflyEndpoint::from_config(&FireflyConfig {
            base_url: format!("http://{address}/api"),
            access_token: "token".to_string(),
            allow_insecure_http: true,
            asset_accounts: vec![],
            default_asset_account_id: None,
            currency_code: None,
            apply_rules: false,
            fire_webhooks: true,
            error_if_duplicate_hash: true,
        });
        let receipt = endpoint
            .post_transaction(&reqwest::Client::new(), &transaction("mailmux:event:42"))
            .await
            .unwrap();
        assert!(receipt.id.is_none());
        let request = request.await.unwrap();
        assert!(request.starts_with("GET /api/v1/search/transactions/count?"));
        assert!(request.contains("external_identifier=mailmux%3Aevent%3A42"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer token")
        );
    }

    #[test]
    fn payload_includes_stable_external_id_on_the_transaction_split() {
        let tx = transaction("mailmux:event:42");

        let endpoint = endpoint();
        let payload = endpoint.request_payload(&tx).unwrap();
        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json["transactions"][0]["external_id"], "mailmux:event:42");
        assert_eq!(json["error_if_duplicate_hash"], true);
    }
}
