use std::io::Read;

use anyhow::Result;
use tracing::{debug, info, warn};

mod config;
mod email;
mod endpoint;
mod input;
mod llm;

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

    let config = config::Config::from_env()?;
    let endpoint = endpoint::build_endpoint(&config);

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
        debug!(sender, "sender not in allow-list, skipping");
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

    let canonical_tx = endpoint::canonical_from_llm(&tx)?;
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
        narration = canonical_tx.narration.as_str(),
        "transaction posted successfully"
    );

    Ok(())
}
