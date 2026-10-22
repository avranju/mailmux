use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use rust_decimal::Decimal;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BalanceConfig {
    pub enabled: bool,
    pub tolerance_minor_units: u64,
}
impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tolerance_minor_units: 1,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Initialized,
    Reconciled,
    Drift,
    OutOfOrder,
}
impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Initialized => "initialized",
            Self::Reconciled => "reconciled",
            Self::Drift => "drift",
            Self::OutOfOrder => "out_of_order",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "initialized" => Self::Initialized,
            "reconciled" => Self::Reconciled,
            "drift" => Self::Drift,
            "out_of_order" => Self::OutOfOrder,
            _ => anyhow::bail!("invalid stored balance outcome"),
        })
    }
}

/// Only explicit post-transaction balance labels count as evidence. Multiple
/// different figures or an unexpected stated currency require human review.
pub fn closing_balance(body: &str, currency: &str) -> Result<Option<Decimal>> {
    let re = regex::Regex::new(
        r"(?i)(?:closing\s+balance|avl\.?\s+bal(?:ance)?|available\s+balance)[\s:=-]*(?:(INR|USD|Rs\.?|₹|\$)\s*)?(-?\d[\d,]*(?:\.\d+)?)(?:\s*(INR|USD))?",
    )?;
    let mut result = None;
    for capture in re.captures_iter(body) {
        if let Some(code) = capture.get(1).or_else(|| capture.get(3)) {
            let code = code.as_str().to_ascii_uppercase();
            let actual = match code.as_str() {
                "RS" | "RS." | "₹" => "INR",
                "$" => "USD",
                other => other,
            };
            if actual != currency {
                anyhow::bail!("balance currency differs from account currency");
            }
        }
        let amount = Decimal::from_str_exact(&capture[2].replace(',', ""))?;
        crate::money::minor_units(amount)?;
        if result.is_some_and(|previous| previous != amount) {
            anyhow::bail!("multiple different closing balances");
        }
        result = Some(amount);
    }
    Ok(result)
}

pub fn signed_minor_units(amount: Decimal) -> Result<i64> {
    let units = crate::money::minor_units(amount)?;
    Ok(if amount.is_sign_negative() {
        -units
    } else {
        units
    })
}

pub struct Observation<'a> {
    pub external_id: &'a str,
    pub account: &'a str,
    pub currency: &'a str,
    pub timestamp: i64,
    pub signed_units: i64,
    pub balance_units: i64,
}

pub struct BalanceStore {
    conn: Connection,
}
impl BalanceStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).context("opening balance state")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS balance_observations (
            external_id TEXT PRIMARY KEY,account TEXT NOT NULL,currency TEXT NOT NULL,
            timestamp INTEGER NOT NULL,balance_units INTEGER NOT NULL,signed_units INTEGER NOT NULL,outcome TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS balance_checkpoints (
            account TEXT NOT NULL,currency TEXT NOT NULL,timestamp INTEGER NOT NULL,balance_units INTEGER NOT NULL,
            PRIMARY KEY(account,currency));")?;
        Ok(Self { conn })
    }
    /// Persist bank observations independently of import success. A replay keeps
    /// its original verdict; older observations never rewind the latest baseline.
    pub fn observe(&mut self, event: Observation<'_>, tolerance: u64) -> Result<Outcome> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(status) = tx
            .query_row(
                "SELECT outcome FROM balance_observations WHERE external_id=?1",
                [event.external_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Outcome::parse(&status);
        }
        let previous: Option<(i64,i64)> = tx.query_row("SELECT timestamp,balance_units FROM balance_checkpoints WHERE account=?1 AND currency=?2",params![event.account,event.currency],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let status = match previous {
            None => Outcome::Initialized,
            Some((timestamp, _)) if event.timestamp <= timestamp => Outcome::OutOfOrder,
            Some((_, balance)) => {
                if ((balance as i128 + event.signed_units as i128) - event.balance_units as i128)
                    .abs()
                    <= tolerance as i128
                {
                    Outcome::Reconciled
                } else {
                    Outcome::Drift
                }
            }
        };
        tx.execute(
            "INSERT INTO balance_observations VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                event.external_id,
                event.account,
                event.currency,
                event.timestamp,
                event.balance_units,
                event.signed_units,
                status.as_str()
            ],
        )?;
        if status != Outcome::OutOfOrder {
            tx.execute("INSERT INTO balance_checkpoints VALUES (?1,?2,?3,?4) ON CONFLICT(account,currency) DO UPDATE SET timestamp=excluded.timestamp,balance_units=excluded.balance_units",params![event.account,event.currency,event.timestamp,event.balance_units])?;
        }
        tx.commit()?;
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_closing_balance_is_exact_and_currency_checked() {
        assert_eq!(
            closing_balance("Avl Bal: Rs. 1,23,456.78", "INR").unwrap(),
            Some(Decimal::from_str_exact("123456.78").unwrap())
        );
        assert!(closing_balance("Closing balance USD 40", "INR").is_err());
        assert!(closing_balance("Closing balance 40; Closing balance 50", "USD").is_err());
        assert_eq!(closing_balance("purchase Rs 42", "INR").unwrap(), None);
    }
    #[test]
    fn detects_gaps_deposits_replays_and_out_of_order_without_rewinding() {
        let mut store = BalanceStore::open(":memory:").unwrap();
        let mut observe = |id: &str, ts, delta, balance, currency| {
            store
                .observe(
                    Observation {
                        external_id: id,
                        account: "a",
                        currency,
                        timestamp: ts,
                        signed_units: delta,
                        balance_units: balance,
                    },
                    1,
                )
                .unwrap()
        };
        assert_eq!(observe("a", 1, -100, 900, "INR"), Outcome::Initialized);
        assert_eq!(observe("b", 2, -100, 800, "INR"), Outcome::Reconciled);
        assert_eq!(observe("b", 2, -100, 800, "INR"), Outcome::Reconciled);
        assert_eq!(observe("old", 1, -100, 100, "INR"), Outcome::OutOfOrder);
        assert_eq!(observe("c", 3, 200, 1000, "INR"), Outcome::Reconciled);
        assert_eq!(observe("d", 4, -100, 700, "INR"), Outcome::Drift);
        assert_eq!(observe("e", 5, -100, 600, "INR"), Outcome::Reconciled);
        assert_eq!(observe("usd", 5, -100, 200, "USD"), Outcome::Initialized);
    }
}
