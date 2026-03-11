use std::io::Read;

use crate::matcher::AccountMatcher;
use anyhow::Result;
use tracing::{info, warn};

mod config;
mod email;
mod endpoint;
mod input;
mod llm;
mod matcher;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    // Log to stderr — stdout is read by mailmux as the processor result.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = config::Config::load()?;
    let endpoint = endpoint::build_endpoint(&config);
    let account_matcher = matcher::DeterministicAccountMatcher::new(
        &config.firefly.asset_accounts,
        config.firefly.default_asset_account_id.clone(),
    )?;

    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| anyhow::anyhow!("reading stdin: {e}"))?;

    let input: input::Input =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing stdin JSON: {e}"))?;

    let email = match &input.email {
        Some(e) => e,
        None => {
            warn!(
                event_id = input.event.id,
                "no email record attached to event, skipping"
            );
            return Ok(());
        }
    };

    let sender = email.sender.as_deref().unwrap_or("");
    if !config.sender_allowed(sender) {
        info!(sender, "sender not in allow-list, skipping");
        return Ok(());
    }

    let subject = email.subject.as_deref().unwrap_or("");
    info!(sender, subject, "processing bank transaction email");

    let body = email::extract_body(&email.raw_message_path)?;

    let llm_client = genai::Client::default();
    let http_client = reqwest::Client::new();

    let tx = llm::extract_transaction(&llm_client, &config.llm_model, subject, &body).await?;

    if tx.status != "found" {
        info!("LLM did not find transaction data in email, skipping");
        return Ok(());
    }

    let resolved = account_matcher.resolve_asset_account(subject, &body)?;
    let canonical_tx = endpoint::canonical_from_llm(&tx, resolved.firefly_account_id.clone(), config.tag.clone(), email.date)?;
    let receipt = endpoint
        .post_transaction(&http_client, &canonical_tx)
        .await?;

    info!(
        endpoint = endpoint.name(),
        endpoint_transaction_id = receipt.id.as_deref(),
        amount = canonical_tx.amount,
        transaction_type = match canonical_tx.kind {
            endpoint::TransactionKind::Deposit => "deposit",
            endpoint::TransactionKind::Withdrawal => "withdrawal",
        },
        resolved_account_id = resolved.account_id.as_str(),
        resolved_firefly_asset_account_id = resolved.firefly_account_id.as_str(),
        account_match_method = resolved.method,
        account_match_value = resolved.matched_value.as_deref(),
        narration = canonical_tx.narration.as_str(),
        "transaction posted successfully"
    );

    Ok(())
}
