use crate::{
    index::{SearchIndex, normalize_sender},
    models::SearchRequest,
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use tantivy::{
    Term,
    query::{BooleanQuery, Query, QueryParser, RangeQuery, TermQuery},
    schema::{Field, IndexRecordOption},
};

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("{0}")]
    Invalid(String),
}

pub fn instant(s: &str, _end: bool) -> Result<i64, QueryError> {
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        // Both bounds denote the represented midnight. The caller applies
        // Included/Excluded, so a date-only `before` is not advanced by a day.
        let midnight = Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap());
        return midnight.timestamp_nanos_opt().ok_or_else(|| {
            QueryError::Invalid("date bound is outside Tantivy's timestamp range".into())
        });
    }
    DateTime::parse_from_rfc3339(s)
        .map_err(|error| QueryError::Invalid(format!("invalid date bound: {error}")))?
        .with_timezone(&Utc)
        .timestamp_nanos_opt()
        .ok_or_else(|| {
            QueryError::Invalid("date bound is outside Tantivy's timestamp range".into())
        })
}

pub fn build(
    req: &SearchRequest,
    idx: &SearchIndex,
    max: usize,
) -> Result<(Box<dyn Query>, usize), QueryError> {
    if req.query.trim().is_empty() {
        return Err(QueryError::Invalid("query must not be blank".into()));
    }
    if req.limit == Some(0) {
        return Err(QueryError::Invalid("limit must be at least 1".into()));
    }
    let limit = req.limit.unwrap_or(10).min(max);
    let f = &idx.fields;
    let mut parser = QueryParser::for_index(
        &idx.index,
        vec![
            f.sender_text,
            f.recipients_text,
            f.subject,
            f.body,
            f.attachment_text,
        ],
    );
    parser.set_field_boost(f.sender_text, 2.0);
    parser.set_field_boost(f.recipients_text, 2.0);
    parser.set_field_boost(f.subject, 3.0);
    parser.set_field_boost(f.attachment_text, 0.8);
    let parsed = parser
        .parse_query(&req.query)
        .map_err(|error| QueryError::Invalid(format!("invalid query: {error}")))?;
    let mut clauses = vec![(tantivy::query::Occur::Must, parsed)];
    fn group(values: &[String], field: Field) -> Option<Box<dyn Query>> {
        if values.is_empty() {
            None
        } else {
            Some(Box::new(BooleanQuery::union(
                values
                    .iter()
                    .map(|v| {
                        Box::new(TermQuery::new(
                            Term::from_field_text(field, v),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>
                    })
                    .collect(),
            )))
        }
    }
    if let Some(q) = group(&req.account_ids, f.account_id) {
        clauses.push((tantivy::query::Occur::Must, q))
    }
    if let Some(q) = group(&req.mailboxes, f.mailbox_name) {
        clauses.push((tantivy::query::Occur::Must, q))
    }
    if !req.senders.is_empty() {
        let vals = req
            .senders
            .iter()
            .map(|x| normalize_sender(x))
            .collect::<Vec<_>>();
        if let Some(q) = group(&vals, f.sender_exact) {
            clauses.push((tantivy::query::Occur::Must, q))
        }
    }
    let lower = req
        .after
        .as_deref()
        .map(|s| instant(s, false))
        .transpose()?;
    let upper = req
        .before
        .as_deref()
        .map(|s| instant(s, true))
        .transpose()?;
    if let (Some(lower), Some(upper)) = (lower, upper)
        && lower >= upper
    {
        return Err(QueryError::Invalid(
            "after must be earlier than before".into(),
        ));
    }
    if lower.is_some() || upper.is_some() {
        let lo = lower
            .map(|x| std::ops::Bound::Included(Term::from_field_i64(f.sent_timestamp, x)))
            .unwrap_or(std::ops::Bound::Unbounded);
        let hi = upper
            .map(|x| std::ops::Bound::Excluded(Term::from_field_i64(f.sent_timestamp, x)))
            .unwrap_or(std::ops::Bound::Unbounded);
        clauses.push((
            tantivy::query::Occur::Must,
            Box::new(RangeQuery::new(lo, hi)),
        ))
    }
    Ok((Box::new(BooleanQuery::new(clauses)), limit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_bounds_use_utc_midnight_and_exclusive_before() {
        assert_eq!(
            instant("2024-01-01", false).unwrap(),
            instant("2024-01-01", true).unwrap()
        );
        assert!(instant("not-a-date", false).is_err());
    }

    #[test]
    fn blank_and_zero_limits_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (index, _writer) = SearchIndex::open(dir.path(), 16 * 1024 * 1024).unwrap();
        let blank = build(
            &SearchRequest {
                query: " ".into(),
                ..Default::default()
            },
            &index,
            50,
        );
        assert!(blank.is_err());
        let zero = build(
            &SearchRequest {
                query: "x".into(),
                limit: Some(0),
                ..Default::default()
            },
            &index,
            50,
        );
        assert!(zero.is_err());
    }
}
