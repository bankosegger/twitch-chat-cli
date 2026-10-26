//! The three paged timelines — messages, events and moderation actions —
//! plus full-text search and the context around a single message.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::Json;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::db::{ApiError, ApiResult, Db};
use super::filter::{non_empty, Filter, PageParams, RangeParams};

#[derive(Serialize)]
pub(super) struct Message {
    id: i64,
    msg_id: Option<String>,
    channel: String,
    user_id: Option<String>,
    login: String,
    display_name: String,
    color: Option<String>,
    badges: String,
    text: String,
    sent_at: i64,
    bits: Option<i64>,
    /// Raw `emotes` tag; positions count characters (code points) of `text`.
    emotes: Option<String>,
    /// Set when a moderator removed it.
    deleted_at: Option<i64>,
}

const MESSAGE_COLUMNS: &str =
    "id, msg_id, channel, user_id, login, display_name, color, badges, text, sent_at, bits, emotes, deleted_at";

fn message_from_row(r: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        msg_id: r.get(1)?,
        channel: r.get(2)?,
        user_id: r.get(3)?,
        login: r.get(4)?,
        display_name: r.get(5)?,
        color: r.get(6)?,
        badges: r.get(7)?,
        text: r.get(8)?,
        sent_at: r.get(9)?,
        bits: r.get(10)?,
        emotes: r.get(11)?,
        deleted_at: r.get(12)?,
    })
}

#[derive(Deserialize)]
pub(super) struct MessagesParams {
    user_id: Option<String>,
    login: Option<String>,
    /// Full-text query; every word must appear (as a word prefix).
    q: Option<String>,
    /// Cursor from a previous page's `next`.
    before: Option<String>,
    /// Only messages with an id above this, for polling.
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct MessagePage {
    messages: Vec<Message>,
    /// Pass as `before` to fetch the next (older) page.
    next: Option<String>,
}

/// Newest first, paginated with a `(sent_at, id)` cursor.
pub(super) async fn messages(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<MessagesParams>,
) -> ApiResult<MessagePage> {
    let limit = p.limit.unwrap_or(100).clamp(1, 500);
    let mut filter = Filter::range(&range);
    if let Some(user_id) = non_empty(&p.user_id) {
        filter.add("user_id = ?", [Value::Text(user_id.to_string())]);
    }
    if let Some(login) = non_empty(&p.login) {
        filter.add("login = ?", [Value::Text(login.to_lowercase())]);
    }
    if let Some(q) = non_empty(&p.q).and_then(fts_query) {
        filter.add("id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)", [Value::Text(q)]);
    }
    let page = PageParams::new(&mut filter, &p.before, p.after, limit)?;
    db.query(move |conn| {
        let (messages, next) = page.fetch(conn, MESSAGE_COLUMNS, "messages", &filter, message_from_row, |m| {
            (m.sent_at, m.id)
        })?;
        Ok(MessagePage { messages, next })
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
pub(super) struct Event {
    id: i64,
    channel: String,
    user_id: Option<String>,
    login: String,
    display_name: String,
    color: Option<String>,
    kind: String,
    tier: Option<String>,
    months: Option<i64>,
    count: Option<i64>,
    recipient_login: Option<String>,
    recipient_display_name: Option<String>,
    system_text: String,
    text: Option<String>,
    sent_at: i64,
}

const EVENT_COLUMNS: &str = "id, channel, user_id, login, display_name, color, kind, tier, months, count,
    recipient_login, recipient_display_name, system_text, text, sent_at";

fn event_from_row(r: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        id: r.get(0)?,
        channel: r.get(1)?,
        user_id: r.get(2)?,
        login: r.get(3)?,
        display_name: r.get(4)?,
        color: r.get(5)?,
        kind: r.get(6)?,
        tier: r.get(7)?,
        months: r.get(8)?,
        count: r.get(9)?,
        recipient_login: r.get(10)?,
        recipient_display_name: r.get(11)?,
        system_text: r.get(12)?,
        text: r.get(13)?,
        sent_at: r.get(14)?,
    })
}

#[derive(Deserialize)]
pub(super) struct EventsParams {
    user_id: Option<String>,
    /// Comma-separated `kind`s, e.g. `sub,resub,subgift`.
    kinds: Option<String>,
    before: Option<String>,
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct EventPage {
    events: Vec<Event>,
    next: Option<String>,
}

/// Subs, raids and other events, paginated like `/messages`. Single gifts
/// from a gift bomb are left out; its `submysterygift` event stands for them.
pub(super) async fn events(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<EventsParams>,
) -> ApiResult<EventPage> {
    let limit = p.limit.unwrap_or(100).clamp(1, 500);
    let mut filter = Filter::range(&range);
    filter.add("bulk_gift = 0", []);
    if let Some(user_id) = non_empty(&p.user_id) {
        filter.add("user_id = ?", [Value::Text(user_id.to_string())]);
    }
    if let Some(kinds) = non_empty(&p.kinds) {
        let kinds: Vec<Value> = kinds.split(',').map(|k| Value::Text(k.trim().to_string())).collect();
        let placeholders = vec!["?"; kinds.len()].join(", ");
        filter.add(&format!("kind IN ({placeholders})"), kinds);
    }
    let page = PageParams::new(&mut filter, &p.before, p.after, limit)?;
    db.query(move |conn| {
        let (events, next) = page.fetch(conn, EVENT_COLUMNS, "events", &filter, event_from_row, |e| {
            (e.sent_at, e.id)
        })?;
        Ok(EventPage { events, next })
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
pub(super) struct ModRow {
    id: i64,
    channel: String,
    /// `ban`, `timeout`, `clear` or `delete`.
    kind: String,
    /// Seconds, for timeouts.
    duration: Option<i64>,
    target_user_id: Option<String>,
    target_login: Option<String>,
    /// The deleted message, for `delete`.
    text: Option<String>,
    sent_at: i64,
}

const MOD_COLUMNS: &str = "id, channel, kind, duration, target_user_id, target_login, text, sent_at";

fn mod_from_row(r: &Row<'_>) -> rusqlite::Result<ModRow> {
    Ok(ModRow {
        id: r.get(0)?,
        channel: r.get(1)?,
        kind: r.get(2)?,
        duration: r.get(3)?,
        target_user_id: r.get(4)?,
        target_login: r.get(5)?,
        text: r.get(6)?,
        sent_at: r.get(7)?,
    })
}

#[derive(Deserialize)]
pub(super) struct ModParams {
    /// Actions against this user.
    user_id: Option<String>,
    login: Option<String>,
    before: Option<String>,
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct ModPage {
    moderation: Vec<ModRow>,
    next: Option<String>,
}

/// Bans, timeouts, /clear and deleted messages, paginated like `/messages`.
pub(super) async fn moderation(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<ModParams>,
) -> ApiResult<ModPage> {
    let limit = p.limit.unwrap_or(100).clamp(1, 500);
    let mut filter = Filter::range(&range);
    if let Some(user_id) = non_empty(&p.user_id) {
        filter.add("target_user_id = ?", [Value::Text(user_id.to_string())]);
    }
    if let Some(login) = non_empty(&p.login) {
        filter.add("target_login = ?", [Value::Text(login.to_lowercase())]);
    }
    let page = PageParams::new(&mut filter, &p.before, p.after, limit)?;
    db.query(move |conn| {
        let (moderation, next) =
            page.fetch(conn, MOD_COLUMNS, "moderation", &filter, mod_from_row, |m| (m.sent_at, m.id))?;
        Ok(ModPage { moderation, next })
    })
    .await
    .map(Json)
}

/// Turns free text into an FTS5 query that can't contain syntax errors: each
/// word is quoted and matched as a prefix, and all words must match.
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|w| w.replace('"', ""))
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(|w| format!("\"{w}\"*"))
        .collect();
    if terms.is_empty() { None } else { Some(terms.join(" ")) }
}

#[derive(Deserialize)]
pub(super) struct ContextParams {
    n: Option<i64>,
}

#[derive(Serialize)]
pub(super) struct MessageContext {
    focus: i64,
    messages: Vec<Message>,
}

/// The message plus up to `n` messages before and after it in the same channel.
pub(super) async fn message_context(
    State(db): State<Arc<Db>>,
    UrlPath(id): UrlPath<i64>,
    Query(p): Query<ContextParams>,
) -> ApiResult<MessageContext> {
    let n = p.n.unwrap_or(30).clamp(1, 200);
    db.query(move |conn| {
        let target = conn
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ?"),
                [id],
                message_from_row,
            )
            .optional()?
            .ok_or(ApiError::NotFound)?;

        let around = |cmp: &str, order: &str| -> rusqlite::Result<Vec<Message>> {
            let sql = format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages
                 WHERE channel = ?1 AND (sent_at {cmp} ?2 OR (sent_at = ?2 AND id {cmp} ?3))
                 ORDER BY sent_at {order}, id {order} LIMIT ?4"
            );
            conn.prepare(&sql)?
                .query_map(rusqlite::params![target.channel, target.sent_at, target.id, n], message_from_row)?
                .collect()
        };
        let mut messages = around("<", "DESC")?;
        messages.reverse();
        let after = around(">", "ASC")?;
        messages.push(target);
        messages.extend(after);
        Ok(MessageContext { focus: id, messages })
    })
    .await
    .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_terms() {
        assert_eq!(fts_query("hello  World").as_deref(), Some("\"hello\"* \"World\"*"));
        assert_eq!(fts_query("say \"hi\" OR NOT").as_deref(), Some("\"say\"* \"hi\"* \"OR\"* \"NOT\"*"));
        assert_eq!(fts_query(" !! \"\" "), None);
    }
}
