use std::io::Read;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::matcher::AccountMatcher;
use crate::metrics::Metrics;

mod auth;
mod balance;
mod category_cache;
mod config;
mod email;
mod endpoint;
mod input;
mod llm;
mod matcher;
mod metrics;
mod money;
mod payee;
mod reconciliation;
mod tagging;
mod transfer;
mod validation;

#[tokio::main]
async fn main() {
    // Log to stderr — stdout is read by mailmux as the processor result.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<_> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        if args == ["reconcile"] {
            let result = async {
                let config = config::Config::load()?;
                reconciliation::run(&config).await
            }
            .await;
            match result {
                Ok(report) => {
                    println!("{}", serde_json::to_string(&report).unwrap());
                    if !report.success {
                        std::process::exit(2);
                    }
                }
                Err(error) => {
                    eprintln!("reconciliation failed: {error:#}");
                    std::process::exit(1);
                }
            }
            return;
        }
        eprintln!("usage: mailtx [reconcile]");
        std::process::exit(1);
    }

    let mut m = Metrics::default();
    let success = match run(&mut m).await {
        Ok(()) => true,
        Err(e) => {
            eprintln!("error: {e:#}");
            if m.result.is_none() {
                m.result = Some("error");
            }
            false
        }
    };

    // Always write a ProcessorOutput JSON object to stdout so mailmux can
    // register the accumulated metrics in its Prometheus endpoint.
    let output = m.into_output(success);
    match serde_json::to_string(&output) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("failed to serialise processor output: {e}"),
    }

    if !success {
        std::process::exit(1);
    }
}

fn endpoint_http_client(timeout: std::time::Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .context("building bounded endpoint HTTP client")
}

async fn run(m: &mut Metrics) -> Result<()> {
    let config = config::Config::load()?;
    let llm_config = config.resolve_llm()?;
    let endpoint = endpoint::build_endpoint(&config);
    let http_client = endpoint_http_client(std::time::Duration::from_secs(30))?;

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
            m.result = Some("skipped_sender");
            return Ok(());
        }
    };

    let external_id = input.external_id()?;

    let sender = email.sender.as_deref().unwrap_or("");
    if !config.sender_allowed(sender) {
        info!(sender, "sender not in allow-list, skipping");
        m.result = Some("skipped_sender");
        return Ok(());
    }

    let Some(authenticated) = email::extract_authenticated(
        &email.raw_message_path,
        sender,
        &config.trusted_authserv_ids,
    )?
    else {
        warn!("email lacks a trusted aligned DMARC pass; skipping");
        m.result = Some("skipped_authentication");
        return Ok(());
    };
    let subject = authenticated.subject.as_str();
    let body = authenticated.body;
    info!(sender, "processing authenticated bank transaction email");

    // Open the pending transfer store (only when transfer_rules are configured).
    let pending_store = config
        .state_db
        .as_deref()
        .map(|path| transfer::PendingStore::open(path, config.transfer_match_window_hours))
        .transpose()?;

    if config.validation.enabled {
        let pending =
            validation::pending_review(config.state_db.as_deref().unwrap(), &external_id)?;
        if let Some(reasons) = pending {
            m.review_reasons = reasons;
            m.result = Some("held_for_review");
            return Ok(());
        }
    }

    let llm_client = llm::LlmClient::from_config(&llm_config)?;

    // Fetch existing categories from Firefly so the LLM can reuse them.
    let categories = match endpoint.fetch_categories(&http_client).await {
        Ok(cats) => {
            info!(
                count = cats.names.len(),
                cache_hit = cats.request_count == 0,
                "loaded Firefly categories"
            );
            for _ in 0..cats.request_count {
                m.firefly_requests.push(("get_categories", "success"));
            }
            cats.names
        }
        Err(e) => {
            m.firefly_requests.push(("get_categories", "error"));
            return Err(e);
        }
    };

    m.llm_calls += 1;
    let tx = match llm_client
        .extract_transaction(subject, &body, &categories)
        .await
    {
        Ok(tx) => {
            m.llm_result = Some("success");
            tx
        }
        Err(e) => {
            m.llm_result = Some("error");
            return Err(e);
        }
    };

    if tx.status == "not_found" {
        info!("LLM did not find transaction data in email, skipping");
        m.result = Some("no_transaction");
        return Ok(());
    }
    if tx.status != "found" {
        anyhow::bail!("LLM returned an invalid transaction status");
    }

    let category_name = tx
        .category
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let resolved = match account_matcher.resolve_asset_account(subject, &body) {
        Ok(r) => {
            m.account_match_method = Some(r.method);
            r
        }
        Err(e) => {
            m.account_match_method = Some("failed");
            return Err(e);
        }
    };

    if config.validation.enabled {
        let currency = resolved
            .currency_code
            .as_deref()
            .or(config.firefly.currency_code.as_deref());
        let mut reasons = validation::reasons(
            &tx,
            email.date,
            currency,
            resolved
                .timezone
                .unwrap_or(config.firefly.transaction_timezone),
            &body,
            &config.validation,
        )?;
        if config.validation.double_extraction {
            m.llm_calls += 1;
            let second = llm_client
                .verify_transaction(subject, &body, &categories)
                .await?;
            if validation::independent_disagrees(&tx, &second) {
                reasons.push("independent_extraction_disagrees");
            }
        }
        if !reasons.is_empty() {
            validation::hold(
                config.state_db.as_deref().unwrap(),
                &external_id,
                &reasons,
                None,
                &email.raw_message_path,
            )?;
            m.review_reasons = reasons.iter().map(|reason| reason.to_string()).collect();
            m.result = Some("held_for_review");
            warn!(reasons=?m.review_reasons,"transaction held for manual review");
            return Ok(());
        }
    }

    // Build the canonical transaction. For transfers this is reused as a base
    // (struct update) to keep all the shared fields (narration, occurred_at, etc.).
    let mut canonical_tx = endpoint::canonical_from_llm(
        &tx,
        resolved.firefly_account_id.clone(),
        config.tag.clone(),
        email.date,
        category_name,
        Some(external_id.clone()),
        resolved
            .timezone
            .unwrap_or(config.firefly.transaction_timezone),
    )?;

    canonical_tx.currency_code = resolved
        .currency_code
        .clone()
        .or_else(|| config.firefly.currency_code.clone());

    canonical_tx.extend_tags(
        config.tags.iter().cloned().chain(
            config
                .firefly
                .asset_accounts
                .iter()
                .filter(|account| account.firefly_account_id.trim() == resolved.firefly_account_id)
                .flat_map(|account| account.tags.iter().cloned()),
        ),
    );

    let source_id = config
        .firefly
        .asset_accounts
        .iter()
        .find(|account| account.firefly_account_id.trim() == resolved.firefly_account_id)
        .map(|account| account.id.as_str())
        .unwrap_or(&resolved.account_id);
    tagging::apply(&mut canonical_tx, source_id, &config.tagging);

    if config.balance_monitor.enabled || config.validation.enabled {
        if let Some(currency) = canonical_tx.currency_code.as_deref() {
            match balance::closing_balance(&body, currency) {
                Ok(Some(closing)) => {
                    let mut store =
                        balance::BalanceStore::open(config.state_db.as_deref().unwrap())?;
                    let units = money::minor_units(canonical_tx.amount)?;
                    let signed_units = if canonical_tx.kind == endpoint::TransactionKind::Withdrawal
                    {
                        -units
                    } else {
                        units
                    };
                    let status = store.observe(
                        balance::Observation {
                            external_id: &external_id,
                            account: &resolved.firefly_account_id,
                            currency,
                            timestamp: email
                                .date
                                .map(|date| date.timestamp())
                                .unwrap_or(canonical_tx.occurred_at.timestamp()),
                            signed_units,
                            balance_units: balance::signed_minor_units(closing)?,
                        },
                        config.balance_monitor.tolerance_minor_units,
                    )?;
                    m.balance_status = Some(status.as_str());
                    if config.tagging.workflow {
                        match status {
                            balance::Outcome::Reconciled => {
                                canonical_tx.extend_tags(["reconciled".into()])
                            }
                            balance::Outcome::Drift | balance::Outcome::OutOfOrder => {
                                canonical_tx.extend_tags(["needs-review".into()])
                            }
                            balance::Outcome::Initialized => {}
                        }
                    }
                    if matches!(
                        status,
                        balance::Outcome::Drift | balance::Outcome::OutOfOrder
                    ) {
                        warn!(
                            account_id = resolved.account_id,
                            status = status.as_str(),
                            "bank balance requires review"
                        );
                    }
                }
                Ok(None) => m.balance_status = Some("no_balance"),
                Err(_) => {
                    m.balance_status = Some("invalid_balance");
                    canonical_tx.extend_tags(["needs-review".into()]);
                    warn!("ambiguous or unexpected bank balance requires review");
                }
            }
        } else {
            m.balance_status = Some("unknown_currency");
            warn!("balance monitoring requires an account currency");
        }
    }

    if config.validation.enabled
        && matches!(
            m.balance_status,
            Some("drift" | "out_of_order" | "invalid_balance")
        )
    {
        validation::hold(
            config.state_db.as_deref().unwrap(),
            &external_id,
            &["balance_requires_review"],
            Some(&canonical_tx),
            &email.raw_message_path,
        )?;
        m.review_reasons = vec!["balance_requires_review".into()];
        m.result = Some("held_for_review");
        return Ok(());
    }

    let original_narration = canonical_tx.narration.clone();
    canonical_tx.narration = payee::normalize(&original_narration, &config.payee_rules);

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
            let currency_code = expired
                .currency_code
                .clone()
                .or_else(|| config.currency_for_account(&asset_account_id));
            let fallback = endpoint::CanonicalTransaction {
                currency_code,
                amount: expired.amount,
                kind,
                narration: expired.narration.clone(),
                occurred_at: expired.occurred_at,
                asset_account_id,
                transfer_destination_account_id: None,
                tags,
                category_name: expired.category.clone(),
                external_id: expired.external_id.clone(),
            };
            match endpoint.post_transaction(&http_client, &fallback).await {
                Ok(receipt) => {
                    warn!(
                        id = receipt.id.as_deref(),
                        amount = %expired.amount,
                        narration = expired.narration.as_str(),
                        "expired transfer leg flushed as unmatched transaction"
                    );
                    store.delete_after_post(expired.id);
                    m.firefly_requests.push(("post_transaction", "success"));
                    m.transfers_expired += 1;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        id = expired.id,
                        "failed to flush expired transfer leg; will retry next run"
                    );
                    m.firefly_requests.push(("post_transaction", "error"));
                }
            }
        }
    }

    // --- Transfer detection -------------------------------------------------
    // Check whether this transaction is a leg of a configured transfer route.
    let tx_type = tx.transaction_type.as_deref().unwrap_or("");
    if let Some(leg) = transfer::leg_from_tx_type(tx_type)
        && let Some(rule_match) =
            transfer::detect_transfer_rule(&config, &resolved.account_id, &leg, &original_narration)
    {
        let store = pending_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "transfer rule matched but state_db is not configured \
                 (this should have been caught at startup)"
            )
        })?;

        let amount_units = money::minor_units(canonical_tx.amount)?;

        if let Some(pending) = store.find_match(
            &rule_match.rule_id,
            leg.opposite_str(),
            amount_units,
            canonical_tx.currency_code.as_deref(),
        )? {
            // Both legs are in — post a Firefly transfer transaction.
            let transfer_tx = endpoint::CanonicalTransaction {
                kind: endpoint::TransactionKind::Transfer,
                asset_account_id: rule_match.source_firefly_id.clone(),
                transfer_destination_account_id: Some(rule_match.destination_firefly_id.clone()),
                ..canonical_tx
            };
            match endpoint.post_transaction(&http_client, &transfer_tx).await {
                Ok(receipt) => {
                    store.delete_after_post(pending.id);
                    m.firefly_requests.push(("post_transaction", "success"));
                    m.transfers_coalesced += 1;
                    m.result = Some("transfer_coalesced");
                    m.firefly_transaction_id = receipt.id.clone();
                    info!(
                        endpoint_transaction_id = receipt.id.as_deref(),
                        amount = %transfer_tx.amount,
                        source_firefly_account_id = transfer_tx.asset_account_id.as_str(),
                        destination_firefly_account_id =
                            transfer_tx.transfer_destination_account_id.as_deref(),
                        narration = transfer_tx.narration.as_str(),
                        "transfer transaction posted"
                    );
                }
                Err(e) => {
                    m.firefly_requests.push(("post_transaction", "error"));
                    return Err(e);
                }
            }
        } else {
            // First leg — store and wait for the counterpart.
            let inserted = store.insert(transfer::InsertLeg {
                rule_id: &rule_match.rule_id,
                leg: &rule_match.leg,
                amount_units,
                currency_code: canonical_tx.currency_code.as_deref(),
                narration: &canonical_tx.narration,
                category: canonical_tx.category_name.as_deref(),
                source_firefly_id: &rule_match.source_firefly_id,
                destination_firefly_id: &rule_match.destination_firefly_id,
                occurred_at: &canonical_tx.occurred_at,
                tags: &canonical_tx.tags,
                external_id: canonical_tx.external_id.as_deref(),
            })?;
            if inserted {
                m.transfers_stored += 1;
                m.result = Some("transfer_stored");
                info!(
                    rule_id = rule_match.rule_id.as_str(),
                    leg = leg.as_str(),
                    amount = %canonical_tx.amount,
                    narration = canonical_tx.narration.as_str(),
                    "first transfer leg stored, waiting for counterpart"
                );
            } else {
                m.result = Some("transfer_already_stored");
                info!(
                    external_id = canonical_tx.external_id.as_deref(),
                    "transfer leg was already stored, treating replay as success"
                );
            }
        }
        return Ok(());
    }
    // --- End transfer detection ---------------------------------------------

    // Regular (non-transfer) transaction.
    match endpoint.post_transaction(&http_client, &canonical_tx).await {
        Ok(receipt) => {
            m.firefly_requests.push(("post_transaction", "success"));
            m.result = Some("posted");
            m.firefly_transaction_id = receipt.id.clone();
            info!(
                endpoint = endpoint.name(),
                endpoint_transaction_id = receipt.id.as_deref(),
                amount = %canonical_tx.amount,
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
        }
        Err(e) => {
            m.firefly_requests.push(("post_transaction", "error"));
            return Err(e);
        }
    }

    Ok(())
}

#[cfg(test)]
mod http_timeout_tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn endpoint_http_deadline_stops_a_stalled_response() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (received, request) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 1024];
            assert!(peer.read(&mut buffer).await.unwrap() > 0);
            let _ = received.send(());
            std::future::pending::<()>().await;
        });
        let client = endpoint_http_client(std::time::Duration::from_millis(100))?;
        let error = client
            .get(format!("http://{addr}"))
            .send()
            .await
            .unwrap_err();
        request.await?;
        server.abort();
        assert!(error.is_timeout());
        Ok(())
    }
}
