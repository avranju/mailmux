use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use crate::config::Config;

// ---------------------------------------------------------------------------
// Leg
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leg {
    Withdrawal,
    Deposit,
}

impl Leg {
    pub fn as_str(&self) -> &'static str {
        match self {
            Leg::Withdrawal => "withdrawal",
            Leg::Deposit => "deposit",
        }
    }

    pub fn opposite_str(&self) -> &'static str {
        match self {
            Leg::Withdrawal => "deposit",
            Leg::Deposit => "withdrawal",
        }
    }
}

/// Convert an LLM `transaction_type` string to a `Leg`. Returns `None` for
/// any value other than `"withdrawal"` or `"deposit"`.
pub fn leg_from_tx_type(tx_type: &str) -> Option<Leg> {
    match tx_type {
        "withdrawal" => Some(Leg::Withdrawal),
        "deposit" => Some(Leg::Deposit),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Rule matching
// ---------------------------------------------------------------------------

pub struct RuleMatch {
    pub rule_id: String,
    pub leg: Leg,
    pub source_firefly_id: String,
    pub destination_firefly_id: String,
}

/// Try to match the current transaction leg against the configured transfer
/// rules. Returns `None` if no rule matches — the caller should treat the
/// transaction as a regular withdrawal/deposit.
pub fn detect_transfer_rule(
    config: &Config,
    resolved_account_id: &str,
    leg: &Leg,
    narration: &str,
) -> Option<RuleMatch> {
    let narration_lower = narration.to_ascii_lowercase();

    for rule in &config.transfer_rules {
        let (account_matches, keywords) = match leg {
            Leg::Withdrawal => (
                rule.source_account == resolved_account_id,
                &rule.withdrawal_keywords,
            ),
            Leg::Deposit => (
                rule.destination_account == resolved_account_id,
                &rule.deposit_keywords,
            ),
        };

        if !account_matches {
            continue;
        }

        if keywords
            .iter()
            .all(|kw| narration_lower.contains(&kw.to_ascii_lowercase()))
        {
            let source_firefly_id = find_firefly_id(config, &rule.source_account)?;
            let destination_firefly_id = find_firefly_id(config, &rule.destination_account)?;
            return Some(RuleMatch {
                rule_id: format!("{}:{}", rule.source_account, rule.destination_account),
                leg: leg.clone(),
                source_firefly_id,
                destination_firefly_id,
            });
        }
    }

    None
}

fn find_firefly_id(config: &Config, account_id: &str) -> Option<String> {
    config
        .firefly
        .asset_accounts
        .iter()
        .find(|a| a.id == account_id)
        .map(|a| a.firefly_account_id.clone())
}

// ---------------------------------------------------------------------------
// Pending store
// ---------------------------------------------------------------------------

/// Row returned by `PendingStore::find_match`.
pub struct PendingLeg {
    pub id: i64,
}

/// Arguments for `PendingStore::insert`.
pub struct InsertLeg<'a> {
    pub rule_id: &'a str,
    pub leg: &'a Leg,
    pub amount_units: i64,
    pub narration: &'a str,
    pub category: Option<&'a str>,
    pub source_firefly_id: &'a str,
    pub destination_firefly_id: &'a str,
    pub occurred_at: &'a DateTime<Utc>,
    pub tags: &'a [String],
}

/// Row returned by `PendingStore::drain_expired` — enough data to post a
/// fallback withdrawal/deposit to Firefly if the counterpart never arrives.
pub struct ExpiredLeg {
    pub id: i64,
    pub leg: Leg,
    pub amount: f64,
    pub narration: String,
    pub category: Option<String>,
    pub source_firefly_id: String,
    pub destination_firefly_id: String,
    pub occurred_at: DateTime<Utc>,
    pub tags: Vec<String>,
}

pub struct PendingStore {
    conn: Connection,
    window_secs: i64,
}

impl PendingStore {
    pub fn open(path: &str, window_hours: u64) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening transfer state DB: {path}"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pending_transfers (
                id                     INTEGER PRIMARY KEY AUTOINCREMENT,
                rule_id                TEXT    NOT NULL,
                leg                    TEXT    NOT NULL,
                amount_units           INTEGER NOT NULL,
                narration              TEXT    NOT NULL,
                category               TEXT,
                source_firefly_id      TEXT    NOT NULL,
                destination_firefly_id TEXT    NOT NULL,
                occurred_at            TEXT    NOT NULL,
                inserted_at            INTEGER NOT NULL,
                tags                   TEXT    NOT NULL
            );",
        )
        .context("initialising pending_transfers table")?;
        Ok(Self {
            conn,
            window_secs: (window_hours * 3600) as i64,
        })
    }

    fn cutoff_unix(&self) -> i64 {
        Utc::now().timestamp() - self.window_secs
    }

    /// Return all legs whose `inserted_at` is older than the window.
    /// Call `delete` on each returned entry after successfully flushing it.
    pub fn drain_expired(&self) -> Result<Vec<ExpiredLeg>> {
        let cutoff = self.cutoff_unix();
        let mut stmt = self.conn.prepare(
            "SELECT id, leg, amount_units, narration, category,
                    source_firefly_id, destination_firefly_id, occurred_at, tags
             FROM pending_transfers
             WHERE inserted_at < ?1
             ORDER BY inserted_at ASC",
        )?;

        let rows = stmt
            .query_map(params![cutoff], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()
            .context("querying expired pending transfers")?;

        let mut out = Vec::new();
        for (id, leg_str, amount_units, narration, category, src, dst, occurred_at_str, tags_json)
            in rows
        {
            let leg = if leg_str == "withdrawal" {
                Leg::Withdrawal
            } else {
                Leg::Deposit
            };
            let occurred_at = DateTime::parse_from_rfc3339(&occurred_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            out.push(ExpiredLeg {
                id,
                leg,
                amount: amount_units as f64 / 100.0,
                narration,
                category,
                source_firefly_id: src,
                destination_firefly_id: dst,
                occurred_at,
                tags,
            });
        }
        Ok(out)
    }

    /// Delete a pending leg by ID (call after successfully posting it).
    pub fn delete(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_transfers WHERE id = ?1", params![id])
            .context("deleting pending transfer leg")?;
        Ok(())
    }

    /// Find the oldest pending counterpart leg for this rule and amount that
    /// is still within the match window.
    pub fn find_match(
        &self,
        rule_id: &str,
        opposite_leg: &str,
        amount_units: i64,
    ) -> Result<Option<PendingLeg>> {
        let cutoff = self.cutoff_unix();
        let result = self.conn.query_row(
            "SELECT id
             FROM pending_transfers
             WHERE rule_id = ?1 AND leg = ?2 AND amount_units = ?3 AND inserted_at >= ?4
             ORDER BY inserted_at ASC
             LIMIT 1",
            params![rule_id, opposite_leg, amount_units, cutoff],
            |row| Ok(PendingLeg { id: row.get(0)? }),
        );
        match result {
            Ok(leg) => Ok(Some(leg)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("querying pending transfer match"),
        }
    }

    /// Insert a new pending leg into the store.
    pub fn insert(&self, args: InsertLeg<'_>) -> Result<()> {
        let tags_json = serde_json::to_string(args.tags).context("serialising tags")?;
        self.conn
            .execute(
                "INSERT INTO pending_transfers
                     (rule_id, leg, amount_units, narration, category,
                      source_firefly_id, destination_firefly_id,
                      occurred_at, inserted_at, tags)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    args.rule_id,
                    args.leg.as_str(),
                    args.amount_units,
                    args.narration,
                    args.category,
                    args.source_firefly_id,
                    args.destination_firefly_id,
                    args.occurred_at.to_rfc3339(),
                    Utc::now().timestamp(),
                    tags_json,
                ],
            )
            .context("inserting pending transfer leg")?;
        Ok(())
    }
}
