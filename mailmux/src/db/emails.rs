use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

/// Metadata for a stored email.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailRecord {
    pub id: i64,
    pub account_id: String,
    pub mailbox_name: String,
    pub uid: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub recipients: Option<serde_json::Value>,
    pub date: Option<DateTime<Utc>>,
    pub flags: Vec<String>,
    pub raw_message_path: String,
    pub size_bytes: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Data needed to insert a new email.
#[derive(Debug)]
pub struct NewEmail {
    pub account_id: String,
    pub mailbox_name: String,
    pub uid: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub recipients: Option<serde_json::Value>,
    pub date: Option<DateTime<Utc>>,
    pub flags: Vec<String>,
    pub raw_message_path: String,
    pub size_bytes: Option<i64>,
}

/// Sync state for a mailbox.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MailboxState {
    pub id: i64,
    pub account_id: String,
    pub mailbox_name: String,
    pub uid_validity: i64,
    pub last_seen_uid: i64,
    pub last_sync_at: Option<DateTime<Utc>>,
}

/// Get the current mailbox state.
pub async fn get_mailbox_state(
    pool: &PgPool,
    account_id: &str,
    mailbox: &str,
) -> Result<Option<MailboxState>> {
    let row = sqlx::query(
        r#"
        SELECT id, account_id, mailbox_name, uid_validity, last_seen_uid, last_sync_at
        FROM mailbox_states
        WHERE account_id = $1 AND mailbox_name = $2
        "#,
    )
    .bind(account_id)
    .bind(mailbox)
    .fetch_optional(pool)
    .await
    .context("fetching mailbox state")?;

    Ok(row.map(|r| MailboxState {
        id: r.get("id"),
        account_id: r.get("account_id"),
        mailbox_name: r.get("mailbox_name"),
        uid_validity: r.get("uid_validity"),
        last_seen_uid: r.get("last_seen_uid"),
        last_sync_at: r.get("last_sync_at"),
    }))
}

/// Upsert mailbox state (insert or update).
pub async fn upsert_mailbox_state(
    pool: &PgPool,
    account_id: &str,
    mailbox_name: &str,
    uid_validity: i64,
    last_seen_uid: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO mailbox_states (account_id, mailbox_name, uid_validity, last_seen_uid, last_sync_at)
        VALUES ($1, $2, $3, $4, now())
        ON CONFLICT (account_id, mailbox_name)
        DO UPDATE SET uid_validity = $3, last_seen_uid = $4, last_sync_at = now(), updated_at = now()
        "#,
    )
    .bind(account_id)
    .bind(mailbox_name)
    .bind(uid_validity)
    .bind(last_seen_uid)
    .execute(pool)
    .await
    .context("upserting mailbox state")?;

    Ok(())
}

/// Columns selected for every `emails` read. Kept in one place so new query
/// paths cannot drift out of sync with the full `EmailRecord` field list.
const EMAIL_COLUMNS: &str = "id, account_id, mailbox_name, uid, message_id, subject, sender, recipients,\
       date, flags, raw_message_path, size_bytes, created_at, updated_at";

/// Map a database row to a complete `EmailRecord`.
fn email_record_from_row(row: &sqlx::postgres::PgRow) -> EmailRecord {
    EmailRecord {
        id: row.get("id"),
        account_id: row.get("account_id"),
        mailbox_name: row.get("mailbox_name"),
        uid: row.get("uid"),
        message_id: row.get("message_id"),
        subject: row.get("subject"),
        sender: row.get("sender"),
        recipients: row.get("recipients"),
        date: row.get("date"),
        flags: row.get("flags"),
        raw_message_path: row.get("raw_message_path"),
        size_bytes: row.get("size_bytes"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

/// Get an email record by ID.
pub async fn get_email_by_id(pool: &PgPool, id: i64) -> Result<Option<EmailRecord>> {
    let mut builder = QueryBuilder::new(format!("SELECT {EMAIL_COLUMNS} FROM emails WHERE id = "));
    builder.push_bind(id);
    let row = builder
        .build()
        .fetch_optional(pool)
        .await
        .context("fetching email by id")?;

    Ok(row.map(|r| email_record_from_row(&r)))
}

/// Normalized, typed selection criteria for historical backfill queries.
///
/// Only this struct (never raw CLI strings) reaches SQL construction, and all
/// values are sent as bind parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmailBackfillFilter {
    /// Include emails with `date >= after` (emails with NULL date never match
    /// a date filter).
    pub after: Option<DateTime<Utc>>,
    /// Include emails with `date < before` (exclusive upper bound).
    pub before: Option<DateTime<Utc>>,
    /// Include these account ids; repeated values are ORed.
    pub accounts: Vec<String>,
    /// Include these mailbox names; repeated values are ORed.
    pub mailboxes: Vec<String>,
    /// Include these specific `emails.id` values; repeated values are ORed.
    pub email_ids: Vec<i64>,
}

/// Append the backfill filter predicates to a query builder.
///
/// Values within one category (account, mailbox, email id) are ORed together;
/// different categories are combined with AND. All values are bound as
/// parameters, never interpolated into the SQL text.
fn append_backfill_filters(builder: &mut QueryBuilder<Postgres>, filter: &EmailBackfillFilter) {
    if let Some(after) = &filter.after {
        builder.push(" AND date >= ").push_bind(after);
    }
    if let Some(before) = &filter.before {
        builder.push(" AND date < ").push_bind(before);
    }
    if !filter.accounts.is_empty() {
        builder.push(" AND (");
        for (i, account) in filter.accounts.iter().enumerate() {
            if i > 0 {
                builder.push(" OR ");
            }
            builder.push("account_id = ").push_bind(account);
        }
        builder.push(")");
    }
    if !filter.mailboxes.is_empty() {
        builder.push(" AND (");
        for (i, mailbox) in filter.mailboxes.iter().enumerate() {
            if i > 0 {
                builder.push(" OR ");
            }
            builder.push("mailbox_name = ").push_bind(mailbox);
        }
        builder.push(")");
    }
    if !filter.email_ids.is_empty() {
        builder.push(" AND (");
        for (i, email_id) in filter.email_ids.iter().enumerate() {
            if i > 0 {
                builder.push(" OR ");
            }
            builder.push("id = ").push_bind(email_id);
        }
        builder.push(")");
    }
}

/// Build the parameterized COUNT query for backfill selection.
pub(crate) fn build_backfill_count_query(filter: &EmailBackfillFilter) -> QueryBuilder<Postgres> {
    let mut builder = QueryBuilder::new("SELECT COUNT(*) FROM emails WHERE 1=1");
    append_backfill_filters(&mut builder, filter);
    builder
}

/// Build the parameterized keyset page query for backfill selection.
///
/// Pages are bounded by `id > $last_id` plus the filter predicates, ordered by
/// ascending `emails.id`, and capped by a bound LIMIT.
pub(crate) fn build_backfill_page_query(
    filter: &EmailBackfillFilter,
    last_id: i64,
    page_size: i64,
) -> QueryBuilder<Postgres> {
    let mut builder = QueryBuilder::new(format!("SELECT {EMAIL_COLUMNS} FROM emails WHERE id > "));
    builder.push_bind(last_id);
    append_backfill_filters(&mut builder, filter);
    builder.push(" ORDER BY id ASC LIMIT ");
    builder.push_bind(page_size);
    builder
}

/// Count emails matching a backfill filter without loading any rows.
pub async fn count_emails_for_backfill(pool: &PgPool, filter: &EmailBackfillFilter) -> Result<i64> {
    let count: i64 = build_backfill_count_query(filter)
        .build_query_scalar()
        .fetch_one(pool)
        .await
        .context("counting emails for backfill")?;
    Ok(count)
}

/// Fetch one bounded keyset page of complete emails matching a backfill
/// filter: `id > last_id`, all filter predicates, `ORDER BY id ASC`,
/// `LIMIT page_size`.
pub async fn get_email_backfill_page(
    pool: &PgPool,
    filter: &EmailBackfillFilter,
    last_id: i64,
    page_size: i64,
) -> Result<Vec<EmailRecord>> {
    let rows = build_backfill_page_query(filter, last_id, page_size)
        .build()
        .fetch_all(pool)
        .await
        .context("fetching backfill email page")?;

    Ok(rows
        .into_iter()
        .map(|r| email_record_from_row(&r))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};
    use sqlx::{Arguments, Execute};

    fn utc_dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_time(NaiveTime::from_hms_opt(h, mi, s).unwrap())
            .and_utc()
    }

    /// Seed one test email and return its id.
    async fn insert_backfill_test_email(
        pool: &PgPool,
        account: &str,
        mailbox: &str,
        uid: i64,
        date: Option<DateTime<Utc>>,
    ) -> i64 {
        sqlx::query_scalar::<_, i64>(
            r#"
            INSERT INTO emails
                (account_id, mailbox_name, uid, subject, sender, date, raw_message_path)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING id
            "#,
        )
        .bind(account)
        .bind(mailbox)
        .bind(uid)
        .bind(format!("subject {uid}"))
        .bind(format!("sender{uid}@example.com"))
        .bind(date)
        .bind(format!("/tmp/backfill-test/{uid}.eml"))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn full_filter() -> EmailBackfillFilter {
        EmailBackfillFilter {
            after: Some(utc_dt(2021, 1, 1, 0, 0, 0)),
            before: Some(utc_dt(2022, 1, 1, 0, 0, 0)),
            accounts: vec!["personal".into(), "work".into()],
            mailboxes: vec!["INBOX".into(), "Sent".into()],
            email_ids: vec![10, 20],
        }
    }

    /// Extract the final SQL text and bind-parameter count from a built query.
    fn built(mut builder: QueryBuilder<Postgres>) -> (String, usize) {
        let mut query = builder.build();
        let args = query
            .take_arguments()
            .expect("take_arguments")
            .expect("arguments present");
        let params = args.len();
        // `sql()` consumes the query, so take the arguments first.
        let sql = query.sql().as_str().to_string();
        (sql, params)
    }

    #[test]
    fn test_count_query_uses_bind_placeholders_only() {
        let (sql, params) = built(build_backfill_count_query(&full_filter()));

        // Every user value arrives as a bind parameter, in a stable order.
        assert!(sql.starts_with("SELECT COUNT(*) FROM emails WHERE 1=1"));
        assert!(sql.contains("date >= $1"), "sql: {sql}");
        assert!(sql.contains("date < $2"), "sql: {sql}");
        assert!(
            sql.contains("account_id = $3 OR account_id = $4"),
            "sql: {sql}"
        );
        assert!(
            sql.contains("mailbox_name = $5 OR mailbox_name = $6"),
            "sql: {sql}"
        );
        assert!(sql.contains("id = $7 OR id = $8"), "sql: {sql}");
        assert_eq!(params, 8);

        // No user value may appear literally in the SQL text.
        for literal in ["personal", "work", "INBOX", "Sent", "10", "20"] {
            assert!(
                !sql.contains(literal),
                "literal {literal} leaked into sql: {sql}"
            );
        }
    }

    #[test]
    fn test_count_query_without_filters_has_no_conditions() {
        let (sql, params) = built(build_backfill_count_query(&EmailBackfillFilter::default()));
        assert_eq!(sql, "SELECT COUNT(*) FROM emails WHERE 1=1");
        assert_eq!(params, 0);
    }

    #[test]
    fn test_page_query_is_keyset_ascending_bounded() {
        // The full filter binds 8 values, plus last_id ($1) and LIMIT ($10).
        let (sql, params) = built(build_backfill_page_query(&full_filter(), 100, 500));

        assert!(sql.contains("WHERE id > $1"), "sql: {sql}");
        assert!(sql.contains("date >= $2"), "sql: {sql}");
        assert!(sql.contains("id = $8 OR id = $9"), "sql: {sql}");
        assert!(sql.ends_with("ORDER BY id ASC LIMIT $10"), "sql: {sql}");
        assert_eq!(params, 10);
        assert!(!sql.contains(" 100"), "last_id leaked into sql: {sql}");
        assert!(!sql.contains(" 500"), "page size leaked into sql: {sql}");
    }

    #[test]
    fn test_page_query_selects_complete_email_columns() {
        let (sql, _) = built(build_backfill_page_query(
            &EmailBackfillFilter::default(),
            0,
            10,
        ));
        for column in [
            "id",
            "account_id",
            "mailbox_name",
            "uid",
            "message_id",
            "subject",
            "sender",
            "recipients",
            "date",
            "flags",
            "raw_message_path",
            "size_bytes",
            "created_at",
            "updated_at",
        ] {
            assert!(sql.contains(column), "column {column} missing from: {sql}");
        }
    }

    /// End-to-end filter semantics and keyset pagination against PostgreSQL.
    ///
    /// Opt-in: requires `DATABASE_URL` (each `sqlx::test` creates a
    /// throwaway database and runs the migrations).
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn test_backfill_filter_semantics_and_pagination(pool: PgPool) {
        // Seven emails: varied dates (incl. NULL), accounts, mailboxes.
        let id1 =
            insert_backfill_test_email(&pool, "a", "INBOX", 1, Some(utc_dt(2020, 6, 15, 10, 0, 0)))
                .await;
        let id2 =
            insert_backfill_test_email(&pool, "a", "INBOX", 2, Some(utc_dt(2021, 1, 1, 0, 0, 0)))
                .await;
        let id3 =
            insert_backfill_test_email(&pool, "a", "Sent", 3, Some(utc_dt(2021, 6, 15, 0, 0, 0)))
                .await;
        let id4 = insert_backfill_test_email(
            &pool,
            "b",
            "INBOX",
            4,
            Some(utc_dt(2021, 12, 31, 23, 59, 59)),
        )
        .await;
        let id5 =
            insert_backfill_test_email(&pool, "b", "INBOX", 5, Some(utc_dt(2022, 1, 1, 0, 0, 0)))
                .await;
        let id6 = insert_backfill_test_email(&pool, "c", "INBOX", 6, None).await;
        let id7 =
            insert_backfill_test_email(&pool, "a", "INBOX", 7, Some(utc_dt(2023, 5, 5, 0, 0, 0)))
                .await;
        let all_ids = vec![id1, id2, id3, id4, id5, id6, id7];

        // Inclusive --after, exclusive --before; NULL dates excluded.
        let range = EmailBackfillFilter {
            after: Some(utc_dt(2021, 1, 1, 0, 0, 0)),
            before: Some(utc_dt(2022, 1, 1, 0, 0, 0)),
            ..Default::default()
        };
        assert_eq!(count_emails_for_backfill(&pool, &range).await.unwrap(), 3);
        let page = get_email_backfill_page(&pool, &range, 0, 500)
            .await
            .unwrap();
        assert_eq!(
            page.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![id2, id3, id4],
            "ascending id order with inclusive lower / exclusive upper bound"
        );
        assert_eq!(page[0].raw_message_path, "/tmp/backfill-test/2.eml");
        assert_eq!(page[0].subject.as_deref(), Some("subject 2"));
        assert_eq!(page[0].sender.as_deref(), Some("sender2@example.com"));

        // No filters: everything matches (incl. NULL-date email).
        let none = EmailBackfillFilter::default();
        assert_eq!(count_emails_for_backfill(&pool, &none).await.unwrap(), 7);

        // OR within a category, AND across categories.
        let two_accounts = EmailBackfillFilter {
            accounts: vec!["a".into(), "b".into()],
            ..Default::default()
        };
        assert_eq!(
            count_emails_for_backfill(&pool, &two_accounts)
                .await
                .unwrap(),
            6,
            "accounts a (4 emails) OR b (2 emails)"
        );

        let account_and_mailbox = EmailBackfillFilter {
            accounts: vec!["a".into()],
            mailboxes: vec!["Sent".into()],
            ..Default::default()
        };
        assert_eq!(
            count_emails_for_backfill(&pool, &account_and_mailbox)
                .await
                .unwrap(),
            1
        );

        // Explicit email-id selection.
        let by_ids = EmailBackfillFilter {
            email_ids: vec![id1, id4],
            ..Default::default()
        };
        assert_eq!(count_emails_for_backfill(&pool, &by_ids).await.unwrap(), 2);

        // NULL-date email is reachable without a date filter.
        let account_c = EmailBackfillFilter {
            accounts: vec!["c".into()],
            ..Default::default()
        };
        let page = get_email_backfill_page(&pool, &account_c, 0, 500)
            .await
            .unwrap();
        assert_eq!(page.iter().map(|e| e.id).collect::<Vec<_>>(), vec![id6]);
        assert!(page[0].date.is_none());

        // Keyset pagination: pages of 3, ascending, non-overlapping, then empty.
        let mut seen = Vec::new();
        let mut last_id = 0i64;
        for _ in 0..4 {
            let page = get_email_backfill_page(&pool, &none, last_id, 3)
                .await
                .unwrap();
            let page_ids: Vec<i64> = page.iter().map(|e| e.id).collect();
            if page_ids.is_empty() {
                break;
            }
            if let Some(prev) = seen.last() {
                assert!(
                    page_ids.iter().all(|id| *id > *prev),
                    "overlap: {seen:?} {page_ids:?}"
                );
            }
            let mut sorted = page_ids.clone();
            sorted.sort_unstable();
            assert_eq!(page_ids, sorted, "page must be ascending");
            last_id = *page_ids.last().unwrap();
            seen.extend(page_ids);
        }
        assert_eq!(seen, all_ids);
    }
}
