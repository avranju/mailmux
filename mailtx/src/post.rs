use anyhow::{Context, Result};
use serde::Serialize;

use crate::llm::TransactionData;

#[derive(Serialize)]
struct TransactionPayload<'a> {
    amount: f64,
    transaction_type: &'a str,
    narration: &'a str,
}

pub async fn post_transaction(
    client: &reqwest::Client,
    url: &str,
    auth: &str,
    data: &TransactionData,
) -> Result<()> {
    let amount = data
        .amount
        .ok_or_else(|| anyhow::anyhow!("LLM returned status=found but amount is null"))?;
    let transaction_type = data
        .transaction_type
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("LLM returned status=found but transaction_type is null"))?;
    let narration = data.narration.as_deref().unwrap_or("");

    let payload = TransactionPayload {
        amount,
        transaction_type,
        narration,
    };

    let response = client
        .post(url)
        .header("Authorization", auth)
        .json(&payload)
        .send()
        .await
        .context("posting transaction to endpoint")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("endpoint returned {status}: {body}");
    }

    Ok(())
}
