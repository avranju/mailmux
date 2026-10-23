use crate::config::Config;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReconciliationConfig {
    pub reference_file: Option<String>,
    pub max_transactions: usize,
    pub tolerance_minor_units: u64,
}
impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            reference_file: None,
            max_transactions: 1000,
            tolerance_minor_units: 1,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct References {
    accounts: Vec<Reference>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    firefly_account_id: String,
    currency_code: String,
    start: NaiveDate,
    end: NaiveDate,
    opening_balance: String,
    closing_balance: String,
}
#[derive(Deserialize)]
struct Page {
    data: Vec<Group>,
    meta: Meta,
}
#[derive(Deserialize)]
struct Group {
    id: String,
    attributes: Attributes,
}
#[derive(Deserialize)]
struct Attributes {
    transactions: Vec<Split>,
}
#[derive(Deserialize)]
struct Split {
    transaction_journal_id: Option<String>,
    date: chrono::DateTime<chrono::FixedOffset>,
    amount: String,
    currency_code: String,
    foreign_amount: Option<String>,
    foreign_currency_code: Option<String>,
    source_id: String,
    destination_id: String,
}
#[derive(Deserialize)]
struct Meta {
    pagination: Pagination,
}
#[derive(Deserialize)]
struct Pagination {
    current_page: usize,
    total_pages: usize,
}
#[derive(Serialize)]
pub struct Report {
    pub success: bool,
    pub accounts: Vec<AccountReport>,
}
#[derive(Serialize)]
pub struct AccountReport {
    account_id: String,
    currency_code: String,
    start: NaiveDate,
    end: NaiveDate,
    transactions_checked: usize,
    status: &'static str,
    expected_closing: String,
    computed_closing: String,
    difference: String,
}

async fn page(
    config: &Config,
    client: &reqwest::Client,
    reference: &Reference,
    number: usize,
    limit: usize,
) -> Result<Page> {
    let mut url = reqwest::Url::parse(&format!(
        "{}/v1/accounts/{}/transactions",
        config.firefly.base_url.trim_end_matches('/'),
        reference.firefly_account_id
    ))?;
    url.query_pairs_mut()
        .append_pair("start", &reference.start.to_string())
        .append_pair("end", &reference.end.to_string())
        .append_pair("page", &number.to_string())
        .append_pair("limit", &limit.to_string());
    let token = config.firefly.access_token.trim();
    let auth = if token.starts_with("Bearer ") {
        token.to_string()
    } else {
        format!("Bearer {token}")
    };
    let mut response = client
        .get(url)
        .header("Authorization", auth)
        .header("Accept", "application/vnd.api+json")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("reconciliation HTTP request failed"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "reconciliation API returned HTTP {}",
            response.status().as_u16()
        );
    }
    const MAX_BODY: usize = 2 * 1024 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY as u64)
    {
        anyhow::bail!("reconciliation response exceeds size limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("reading reconciliation response failed"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_BODY {
            anyhow::bail!("reconciliation response exceeds size limit");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| anyhow::anyhow!("invalid reconciliation API response"))
}

fn delta(split: &Split, reference: &Reference) -> Result<Option<i128>> {
    let source = split.source_id == reference.firefly_account_id;
    let destination = split.destination_id == reference.firefly_account_id;
    if !source && !destination {
        return Ok(None);
    }
    if split.date.date_naive() < reference.start || split.date.date_naive() > reference.end {
        anyhow::bail!("API returned a transaction outside the reference interval");
    }
    let raw = if split.currency_code == reference.currency_code {
        split.amount.as_str()
    } else if split.foreign_currency_code.as_deref() == Some(&reference.currency_code) {
        split
            .foreign_amount
            .as_deref()
            .context("missing native foreign amount")?
    } else {
        anyhow::bail!("transaction has no amount in the reference currency");
    };
    let amount = crate::money::minor_units(
        Decimal::from_str_exact(raw).context("invalid API transaction amount")?,
    )? as i128;
    Ok(Some(match (source, destination) {
        (true, false) => -amount,
        (false, true) => amount,
        _ => 0,
    }))
}

pub async fn run(config: &Config) -> Result<Report> {
    let settings = &config.reconciliation;
    if !(1..=10000).contains(&settings.max_transactions) {
        anyhow::bail!("reconciliation.max_transactions must be 1..=10000");
    }
    let path = settings
        .reference_file
        .as_deref()
        .context("reconciliation.reference_file is required")?;
    let file = std::fs::File::open(path).context("opening reconciliation reference")?;
    if file.metadata()?.len() > 1024 * 1024 {
        anyhow::bail!("reconciliation reference exceeds size limit");
    }
    use std::io::Read;
    let mut bytes = vec![];
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        anyhow::bail!("reconciliation reference exceeds size limit");
    }
    let references: References = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid reconciliation reference JSON"))?;
    if references.accounts.is_empty() || references.accounts.len() > 128 {
        anyhow::bail!("reference must contain 1..=128 accounts");
    }
    let mut accounts = HashSet::new();
    // Validate all references before any network request.
    for reference in &references.accounts {
        if reference.start > reference.end
            || reference.firefly_account_id.is_empty()
            || !reference
                .firefly_account_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            || !accounts.insert(&reference.firefly_account_id)
        {
            anyhow::bail!("invalid or repeated reconciliation reference account");
        }
        if !config
            .firefly
            .asset_accounts
            .iter()
            .any(|account| account.firefly_account_id.trim() == reference.firefly_account_id)
            || config
                .currency_for_account(&reference.firefly_account_id)
                .as_deref()
                != Some(&reference.currency_code)
        {
            anyhow::bail!("reference account or currency does not match configuration");
        }
        for balance in [&reference.opening_balance, &reference.closing_balance] {
            crate::balance::signed_minor_units(
                Decimal::from_str_exact(balance).context("invalid reference balance")?,
            )?;
        }
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut reports = vec![];
    for reference in references.accounts {
        let mut total = 0i128;
        let mut count = 0;
        let mut groups = 0;
        let mut seen = HashSet::new();
        let mut complete = true;
        let mut number = 1;
        let mut total_pages = None;
        let page_limit = settings.max_transactions.min(100);
        loop {
            let response = page(config, &client, &reference, number, page_limit).await?;
            let pagination = response.meta.pagination;
            if number == 1
                && pagination.current_page == 1
                && pagination.total_pages == 0
                && response.data.is_empty()
            {
                break;
            }
            if pagination.current_page != number
                || pagination.total_pages < number
                || total_pages.is_some_and(|total| total != pagination.total_pages)
                || (response.data.is_empty() && number < pagination.total_pages)
            {
                anyhow::bail!("inconsistent reconciliation pagination");
            }
            total_pages = Some(pagination.total_pages);
            for group in response.data {
                if groups >= settings.max_transactions {
                    complete = false;
                    break;
                }
                groups += 1;
                if group.attributes.transactions.is_empty() {
                    anyhow::bail!("empty reconciliation transaction group");
                }
                for (index, split) in group.attributes.transactions.iter().enumerate() {
                    let key = split
                        .transaction_journal_id
                        .clone()
                        .unwrap_or_else(|| format!("{}:{index}", group.id));
                    if !seen.insert(key) {
                        anyhow::bail!("duplicate transaction across reconciliation pages");
                    }
                    if let Some(change) = delta(split, &reference)? {
                        total += change;
                        count += 1;
                    }
                }
            }
            if number == pagination.total_pages {
                break;
            }
            if groups >= settings.max_transactions {
                complete = false;
                break;
            }
            number += 1;
        }
        let opening = crate::balance::signed_minor_units(Decimal::from_str_exact(
            &reference.opening_balance,
        )?)? as i128;
        let expected = crate::balance::signed_minor_units(Decimal::from_str_exact(
            &reference.closing_balance,
        )?)? as i128;
        let computed = opening + total;
        let difference = computed - expected;
        let status = if !complete {
            "incomplete"
        } else if difference.abs() <= settings.tolerance_minor_units as i128 {
            "matched"
        } else {
            "drift"
        };
        if status != "matched" {
            tracing::warn!(
                account_id = reference.firefly_account_id,
                status,
                "periodic reconciliation requires attention"
            );
        }
        reports.push(AccountReport {
            account_id: reference.firefly_account_id,
            currency_code: reference.currency_code,
            start: reference.start,
            end: reference.end,
            transactions_checked: count,
            status,
            expected_closing: format!("{:.2}", Decimal::from_i128_with_scale(expected, 2)),
            computed_closing: format!("{:.2}", Decimal::from_i128_with_scale(computed, 2)),
            difference: format!("{:.2}", Decimal::from_i128_with_scale(difference, 2)),
        });
    }
    Ok(Report {
        success: reports.iter().all(|report| report.status == "matched"),
        accounts: reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_foreign_and_internal_transfer_amounts_are_explicit_and_exact() {
        let reference = Reference {
            firefly_account_id: "1".into(),
            currency_code: "INR".into(),
            start: "2026-01-01".parse().unwrap(),
            end: "2026-01-31".parse().unwrap(),
            opening_balance: "0".into(),
            closing_balance: "0".into(),
        };
        let mut split:Split=serde_json::from_value(serde_json::json!({"date":"2026-01-01T12:00:00Z","amount":"2","currency_code":"USD","foreign_amount":"170.29","foreign_currency_code":"INR","source_id":"2","destination_id":"1"})).unwrap();
        assert_eq!(delta(&split, &reference).unwrap(), Some(17029));
        split.source_id = "1".into();
        assert_eq!(delta(&split, &reference).unwrap(), Some(0));
        split.destination_id = "2".into();
        assert_eq!(delta(&split, &reference).unwrap(), Some(-17029));
        split.foreign_amount = None;
        assert!(delta(&split, &reference).is_err());
        split.foreign_currency_code = Some("EUR".into());
        assert!(delta(&split, &reference).is_err());
    }
}
