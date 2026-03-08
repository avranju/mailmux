use anyhow::Result;
use regex::Regex;

use crate::config::FireflyAssetAccountConfig;

#[derive(Debug, Clone)]
pub struct MatchedAssetAccount {
    pub account_id: String,
    pub firefly_account_id: String,
    pub method: &'static str,
    pub matched_value: Option<String>,
}

pub trait AccountMatcher: Send + Sync {
    fn resolve_asset_account(&self, subject: &str, body: &str) -> Result<MatchedAssetAccount>;
}

pub struct DeterministicAccountMatcher {
    accounts: Vec<NormalizedAccount>,
    default_asset_account_id: Option<String>,
    card_re: Regex,
    account_re: Regex,
}

impl DeterministicAccountMatcher {
    pub fn new(
        accounts: &[FireflyAssetAccountConfig],
        default_asset_account_id: Option<String>,
    ) -> Result<Self> {
        let accounts = accounts
            .iter()
            .map(NormalizedAccount::from_config)
            .collect::<Vec<_>>();
        Ok(Self {
            accounts,
            default_asset_account_id,
            card_re: Regex::new(
                r"(?i)(?:debit\s*card(?:\s*ending)?|card\s*ending)\D{0,20}(\d{4})",
            )?,
            account_re: Regex::new(
                r"(?i)(?:account|a/c|acct)(?:\s*ending(?:\s*in)?|\s*ending)?\D{0,30}(?:xx+)?(\d{4,8})",
            )?,
        })
    }
}

impl AccountMatcher for DeterministicAccountMatcher {
    fn resolve_asset_account(&self, subject: &str, body: &str) -> Result<MatchedAssetAccount> {
        let text = format!("{subject}\n{body}");
        let text_lower = text.to_ascii_lowercase();

        for caps in self.card_re.captures_iter(&text) {
            let Some(m) = caps.get(1) else {
                continue;
            };
            let last4 = m.as_str().to_string();
            if let Some(acc) = self
                .accounts
                .iter()
                .find(|a| a.debit_card_last4.contains(&last4))
            {
                return Ok(acc.matched("debit_card_last4", Some(last4)));
            }
        }

        for caps in self.account_re.captures_iter(&text) {
            let Some(m) = caps.get(1) else {
                continue;
            };
            let raw = m.as_str();
            for acc in &self.accounts {
                if acc.matches_account_suffix(raw) {
                    return Ok(acc.matched("account_suffix", Some(raw.to_string())));
                }
            }
        }

        for acc in &self.accounts {
            if let Some(alias) = acc
                .aliases
                .iter()
                .find(|alias| text_lower.contains(alias.as_str()))
            {
                return Ok(acc.matched("alias", Some(alias.clone())));
            }
        }

        if let Some(default_id) = &self.default_asset_account_id {
            return Ok(MatchedAssetAccount {
                account_id: "default".to_string(),
                firefly_account_id: default_id.clone(),
                method: "default_fallback",
                matched_value: None,
            });
        }

        anyhow::bail!("unable to resolve Firefly asset account from email content")
    }
}

#[derive(Debug, Clone)]
struct NormalizedAccount {
    id: String,
    firefly_account_id: String,
    account_suffixes: Vec<String>,
    debit_card_last4: Vec<String>,
    aliases: Vec<String>,
}

impl NormalizedAccount {
    fn from_config(config: &FireflyAssetAccountConfig) -> Self {
        Self {
            id: config.id.trim().to_string(),
            firefly_account_id: config.firefly_account_id.trim().to_string(),
            account_suffixes: config
                .account_suffixes
                .iter()
                .map(|v| normalize_digits(v))
                .filter(|v| !v.is_empty())
                .collect(),
            debit_card_last4: config
                .debit_card_last4
                .iter()
                .map(|v| normalize_digits(v))
                .filter(|v| v.len() == 4)
                .collect(),
            aliases: config
                .aliases
                .iter()
                .map(|v| v.trim().to_ascii_lowercase())
                .filter(|v| !v.is_empty())
                .collect(),
        }
    }

    fn matches_account_suffix(&self, observed: &str) -> bool {
        let observed_digits = normalize_digits(observed);
        if observed_digits.is_empty() {
            return false;
        }
        self.account_suffixes
            .iter()
            .any(|suffix| observed_digits.ends_with(suffix))
    }

    fn matched(&self, method: &'static str, matched_value: Option<String>) -> MatchedAssetAccount {
        MatchedAssetAccount {
            account_id: self.id.clone(),
            firefly_account_id: self.firefly_account_id.clone(),
            method,
            matched_value,
        }
    }
}

fn normalize_digits(value: &str) -> String {
    value.chars().filter(char::is_ascii_digit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FireflyAssetAccountConfig;

    fn matcher() -> DeterministicAccountMatcher {
        DeterministicAccountMatcher::new(
            &[
                FireflyAssetAccountConfig {
                    id: "hdfc_9772".to_string(),
                    firefly_account_id: "12".to_string(),
                    account_suffixes: vec!["9772".to_string()],
                    debit_card_last4: vec!["7406".to_string()],
                    aliases: vec!["hdfc savings".to_string()],
                },
                FireflyAssetAccountConfig {
                    id: "sbm_3989".to_string(),
                    firefly_account_id: "42".to_string(),
                    account_suffixes: vec!["3989".to_string()],
                    debit_card_last4: vec![],
                    aliases: vec!["sbm bank".to_string()],
                },
            ],
            None,
        )
        .expect("matcher should build")
    }

    #[test]
    fn resolves_by_debit_card_last4() {
        let matched = matcher()
            .resolve_asset_account(
                "",
                "Rs.750.00 is debited from your HDFC Bank Debit Card ending 7406 at PAYZAPP",
            )
            .expect("must match");
        assert_eq!(matched.account_id, "hdfc_9772");
        assert_eq!(matched.method, "debit_card_last4");
    }

    #[test]
    fn resolves_by_account_suffix() {
        let matched = matcher()
            .resolve_asset_account("", "debited from account 9772 to VPA ...")
            .expect("must match");
        assert_eq!(matched.account_id, "hdfc_9772");
        assert_eq!(matched.method, "account_suffix");
    }
}
