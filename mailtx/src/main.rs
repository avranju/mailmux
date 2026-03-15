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
mod transfer;

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
    let http_client = reqwest::Client::new();

    // Open the pending transfer store (only when transfer_rules are configured).
    let pending_store = config
        .state_db
        .as_deref()
        .map(|path| transfer::PendingStore::open(path, config.transfer_match_window_hours))
        .transpose()?;

    // Flush any transfer legs that exceeded the match window before processing
    // the current email. They are posted as regular transactions with the
    // "unmatched-transfer" tag so nothing is silently lost.
    if let Some(store) = &pending_store {
        for expired in store.drain_expired()? {
            let (kind, asset_account_id) = match expired.leg {
                transfer::Leg::Withdrawal => (
                    endpoint::TransactionKind::Withdrawal,
                    expired.source_firefly_id.clone(),
                ),
                transfer::Leg::Deposit => (
                    endpoint::TransactionKind::Deposit,
                    expired.destination_firefly_id.clone(),
                ),
            };
            let mut tags = expired.tags.clone();
            tags.push("unmatched-transfer".to_string());
            let fallback = endpoint::CanonicalTransaction {
                amount: expired.amount,
                kind,
                narration: expired.narration.clone(),
                occurred_at: expired.occurred_at,
                asset_account_id,
                transfer_destination_account_id: None,
                tags,
                category_name: expired.category.clone(),
            };
            match endpoint.post_transaction(&http_client, &fallback).await {
                Ok(receipt) => {
                    warn!(
                        id = receipt.id.as_deref(),
                        amount = expired.amount,
                        narration = expired.narration.as_str(),
                        "expired transfer leg flushed as unmatched transaction"
                    );
                    store.delete(expired.id)?;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        id = expired.id,
                        "failed to flush expired transfer leg; will retry next run"
                    );
                }
            }
        }
    }

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

    // Fetch existing categories from Firefly so the LLM can reuse them.
    let categories = endpoint.fetch_categories(&http_client).await?;
    info!(count = categories.len(), "cached Firefly categories");

    let tx =
        llm::extract_transaction(&llm_client, &config.llm_model, subject, &body, &categories)
            .await?;

    if tx.status != "found" {
        info!("LLM did not find transaction data in email, skipping");
        return Ok(());
    }

    let category_name = tx
        .category
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let resolved = account_matcher.resolve_asset_account(subject, &body)?;

    // Build the canonical transaction. For transfers this is reused as a base
    // (struct update) to keep all the shared fields (narration, occurred_at, etc.).
    let canonical_tx = endpoint::canonical_from_llm(
        &tx,
        resolved.firefly_account_id.clone(),
        config.tag.clone(),
        email.date,
        category_name,
    )?;

    // --- Transfer detection -------------------------------------------------
    // Check whether this transaction is a leg of a configured transfer route.
    let tx_type = tx.transaction_type.as_deref().unwrap_or("");
    if let Some(leg) = transfer::leg_from_tx_type(tx_type)
        && let Some(rule_match) =
            transfer::detect_transfer_rule(&config, &resolved.account_id, &leg, &canonical_tx.narration)
    {
        let store = pending_store.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "transfer rule matched but state_db is not configured \
                     (this should have been caught at startup)"
                )
            })?;

            let amount_units = (canonical_tx.amount.abs() * 100.0).round() as i64;

            if let Some(pending) =
                store.find_match(&rule_match.rule_id, leg.opposite_str(), amount_units)?
            {
                // Both legs are in — post a Firefly transfer transaction.
                let transfer_tx = endpoint::CanonicalTransaction {
                    kind: endpoint::TransactionKind::Transfer,
                    asset_account_id: rule_match.source_firefly_id.clone(),
                    transfer_destination_account_id: Some(
                        rule_match.destination_firefly_id.clone(),
                    ),
                    ..canonical_tx
                };
                let receipt = endpoint.post_transaction(&http_client, &transfer_tx).await?;
                store.delete(pending.id)?;
                info!(
                    endpoint_transaction_id = receipt.id.as_deref(),
                    amount = transfer_tx.amount,
                    source_firefly_account_id = transfer_tx.asset_account_id.as_str(),
                    destination_firefly_account_id =
                        transfer_tx.transfer_destination_account_id.as_deref(),
                    narration = transfer_tx.narration.as_str(),
                    "transfer transaction posted"
                );
            } else {
                // First leg — store and wait for the counterpart.
                store.insert(transfer::InsertLeg {
                    rule_id: &rule_match.rule_id,
                    leg: &rule_match.leg,
                    amount_units,
                    narration: &canonical_tx.narration,
                    category: canonical_tx.category_name.as_deref(),
                    source_firefly_id: &rule_match.source_firefly_id,
                    destination_firefly_id: &rule_match.destination_firefly_id,
                    occurred_at: &canonical_tx.occurred_at,
                    tags: &canonical_tx.tags,
                })?;
                info!(
                    rule_id = rule_match.rule_id.as_str(),
                    leg = leg.as_str(),
                    amount = canonical_tx.amount,
                    narration = canonical_tx.narration.as_str(),
                    "first transfer leg stored, waiting for counterpart"
                );
        }
        return Ok(());
    }
    // --- End transfer detection ---------------------------------------------

    // Regular (non-transfer) transaction — existing behaviour.
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
            endpoint::TransactionKind::Transfer => "transfer",
        },
        resolved_account_id = resolved.account_id.as_str(),
        resolved_firefly_asset_account_id = resolved.firefly_account_id.as_str(),
        account_match_method = resolved.method,
        account_match_value = resolved.matched_value.as_deref(),
        narration = canonical_tx.narration.as_str(),
        category = canonical_tx.category_name.as_deref().unwrap_or("(none)"),
        "transaction posted successfully"
    );

    Ok(())
}
