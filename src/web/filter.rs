//! Shared query building: the `WHERE` clause every endpoint narrows its table
//! with, and the cursor pagination the list endpoints share.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, Row};
use serde::Deserialize;

use super::db::ApiError;

/// Accumulates `WHERE` conditions and their bound values.
#[derive(Default)]
pub(super) struct Filter {
    conds: Vec<String>,
    pub(super) args: Vec<Value>,
}

impl Filter {
    pub(super) fn add(&mut self, cond: &str, args: impl IntoIterator<Item = Value>) {
        self.conds.push(cond.to_string());
        self.args.extend(args);
    }

    pub(super) fn range(range: &RangeParams) -> Self {
        Self::range_on(range, "sent_at")
    }

    /// Like `range`, for a table whose timestamp column is `time_column`.
    pub(super) fn range_on(range: &RangeParams, time_column: &str) -> Self {
        let mut f = Filter::default();
        if let Some(channel) = non_empty(&range.channel) {
            f.add("channel = ?", [Value::Text(channel.to_lowercase())]);
        }
        if let Some(from) = range.from {
            f.add(&format!("{time_column} >= ?"), [Value::Integer(from)]);
        }
        if let Some(to) = range.to {
            f.add(&format!("{time_column} < ?"), [Value::Integer(to)]);
        }
        f
    }

    pub(super) fn sql(&self) -> String {
        if self.conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.conds.join(" AND "))
        }
    }
}

pub(super) fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

#[derive(Deserialize)]
pub(super) struct RangeParams {
    pub(super) channel: Option<String>,
    /// Inclusive lower bound, unix ms.
    pub(super) from: Option<i64>,
    /// Exclusive upper bound, unix ms.
    pub(super) to: Option<i64>,
}

/// Pagination shared by `/messages` and `/events`: newest first with a
/// `sent_at:id` cursor, or, when polling with `after`, rows above that id.
pub(super) struct PageParams {
    polling: bool,
    limit: usize,
}

impl PageParams {
    /// Adds the cursor conditions to `filter`.
    pub(super) fn new(
        filter: &mut Filter,
        before: &Option<String>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Self, ApiError> {
        if let Some(before) = non_empty(before) {
            let (sent_at, id) = before
                .split_once(':')
                .and_then(|(t, id)| Some((t.parse::<i64>().ok()?, id.parse::<i64>().ok()?)))
                .ok_or_else(|| ApiError::BadRequest("invalid cursor".to_string()))?;
            filter.add(
                "(sent_at < ? OR (sent_at = ? AND id < ?))",
                [Value::Integer(sent_at), Value::Integer(sent_at), Value::Integer(id)],
            );
        }
        if let Some(after) = after {
            filter.add("id > ?", [Value::Integer(after)]);
        }
        Ok(Self { polling: after.is_some(), limit })
    }

    /// Returns the rows, newest first, and the cursor for the next page.
    pub(super) fn fetch<T>(
        &self,
        conn: &Connection,
        columns: &str,
        table: &str,
        filter: &Filter,
        from_row: fn(&Row<'_>) -> rusqlite::Result<T>,
        key: fn(&T) -> (i64, i64),
    ) -> rusqlite::Result<(Vec<T>, Option<String>)> {
        let limit = self.limit;
        if self.polling {
            // The oldest new rows first when there are more than `limit`, so
            // repeated polls never skip any.
            let sql = format!("SELECT {columns} FROM {table} {} ORDER BY id LIMIT {limit}", filter.sql());
            let mut rows: Vec<T> = conn
                .prepare(&sql)?
                .query_map(params_from_iter(&filter.args), from_row)?
                .collect::<Result<_, _>>()?;
            rows.reverse();
            return Ok((rows, None));
        }

        let sql = format!(
            "SELECT {columns} FROM {table} {} ORDER BY sent_at DESC, id DESC LIMIT {}",
            filter.sql(),
            limit + 1
        );
        let mut rows: Vec<T> = conn
            .prepare(&sql)?
            .query_map(params_from_iter(&filter.args), from_row)?
            .collect::<Result<_, _>>()?;
        let next = if rows.len() > limit {
            rows.truncate(limit);
            rows.last().map(|r| {
                let (sent_at, id) = key(r);
                format!("{sent_at}:{id}")
            })
        } else {
            None
        };
        Ok((rows, next))
    }
}
