//! Finding a chatter by name, and the per-user page: the names they have used
//! and the channels they appear in.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::db::{ApiError, ApiResult, Db};

#[derive(Deserialize)]
pub(super) struct UserSearchParams {
    q: String,
}

#[derive(Serialize)]
pub(super) struct UserMatch {
    user_id: Option<String>,
    login: String,
    display_name: String,
    messages: i64,
}

/// Logins starting with `q`, alphabetically.
pub(super) async fn search_users(
    State(db): State<Arc<Db>>,
    Query(p): Query<UserSearchParams>,
) -> ApiResult<Vec<UserMatch>> {
    let prefix = p.q.trim().trim_start_matches('@').to_lowercase();
    if prefix.is_empty() {
        return Ok(Json(Vec::new()));
    }
    db.query(move |conn| {
        // A range scan on the login index; LIKE would not use it.
        let upper = format!("{prefix}\u{10FFFF}");
        let mut stmt = conn.prepare(
            "SELECT user_id, login, display_name, count(*)
             FROM messages WHERE login >= ?1 AND login < ?2
             GROUP BY login ORDER BY login LIMIT 20",
        )?;
        let rows = stmt.query_map(rusqlite::params![prefix, upper], |r| {
            Ok(UserMatch {
                user_id: r.get(0)?,
                login: r.get(1)?,
                display_name: r.get(2)?,
                messages: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
pub(super) struct UserChannel {
    channel: String,
    messages: i64,
    /// Subs, gifts, raids, ... they triggered (gift bombs count once).
    events: i64,
    first_at: i64,
    last_at: i64,
}

#[derive(Serialize)]
pub(super) struct UserName {
    login: String,
    display_name: String,
    first_at: i64,
    last_at: i64,
}

#[derive(Serialize)]
pub(super) struct User {
    user_id: String,
    /// Most recent first.
    names: Vec<UserName>,
    channels: Vec<UserChannel>,
}

/// Everything a user did: their messages (`is_message` = 1) and the events
/// they triggered, so someone who subbed but never chatted still has a page.
const USER_ACTIVITY: &str = "(
    SELECT channel, login, display_name, sent_at, 1 AS is_message FROM messages WHERE user_id = ?1
    UNION ALL
    SELECT channel, login, display_name, sent_at, 0 FROM events WHERE user_id = ?1 AND bulk_gift = 0
)";

pub(super) async fn user(State(db): State<Arc<Db>>, UrlPath(user_id): UrlPath<String>) -> ApiResult<User> {
    db.query(move |conn| {
        let names: Vec<UserName> = conn
            .prepare(&format!(
                "SELECT login, display_name, min(sent_at), max(sent_at)
                 FROM {USER_ACTIVITY} GROUP BY login, display_name ORDER BY max(sent_at) DESC"
            ))?
            .query_map([&user_id], |r| {
                Ok(UserName {
                    login: r.get(0)?,
                    display_name: r.get(1)?,
                    first_at: r.get(2)?,
                    last_at: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        if names.is_empty() {
            return Err(ApiError::NotFound);
        }
        let channels = conn
            .prepare(&format!(
                "SELECT channel, sum(is_message), sum(1 - is_message), min(sent_at), max(sent_at)
                 FROM {USER_ACTIVITY} GROUP BY channel ORDER BY sum(is_message) DESC, count(*) DESC"
            ))?
            .query_map([&user_id], |r| {
                Ok(UserChannel {
                    channel: r.get(0)?,
                    messages: r.get(1)?,
                    events: r.get(2)?,
                    first_at: r.get(3)?,
                    last_at: r.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(User { user_id, names, channels })
    })
    .await
    .map(Json)
}
