use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, OpenFlags, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::store::Store;

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const STYLE_CSS: &str = include_str!("web/style.css");
const FONT_WOFF2: &[u8] = include_bytes!("fonts/PlusJakartaSans-Variable.woff2");

const MAX_IDLE_CONNECTIONS: usize = 8;
const MAX_ACTIVITY_BUCKETS: i64 = 2000;

/// Serves the web UI and its JSON API until interrupted. When `password` is
/// set, every request requires HTTP basic auth with that password (any
/// username).
pub async fn run(db_path: &Path, bind: SocketAddr, password: Option<String>) -> io::Result<()> {
    // Opening read-write once creates the schema (and the search index) if the
    // database is new or was written by an older recorder.
    Store::open(db_path).map_err(io::Error::other)?;

    let db = Arc::new(Db {
        path: db_path.to_path_buf(),
        idle: Mutex::new(Vec::new()),
    });

    let api = Router::new()
        .route("/channels", get(channels))
        .route("/storage", get(storage))
        .route("/stats", get(stats))
        .route("/activity", get(activity))
        .route("/top-chatters", get(top_chatters))
        .route("/top-messages", get(top_messages))
        .route("/messages", get(messages))
        .route("/messages/{id}/context", get(message_context))
        .route("/users", get(search_users))
        .route("/users/{user_id}", get(user))
        .with_state(db);

    let mut app = Router::new()
        .route("/", get(|| async { asset("text/html; charset=utf-8", INDEX_HTML) }))
        .route("/app.js", get(|| async { asset("text/javascript", APP_JS) }))
        .route("/style.css", get(|| async { asset("text/css", STYLE_CSS) }))
        .route(
            "/fonts/PlusJakartaSans-Variable.woff2",
            get(|| async { asset("font/woff2", FONT_WOFF2) }),
        )
        .nest("/api", api);

    let auth_enabled = password.is_some();
    if let Some(password) = password {
        app = app.layer(middleware::from_fn_with_state(Arc::new(password), require_password));
    }

    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!(
        "serving {} on http://{}{}",
        db_path.display(),
        listener.local_addr()?,
        if auth_enabled { " (password protected)" } else { "" }
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            crate::record::shutdown_signal().await;
            eprintln!("shutting down");
        })
        .await
}

/// Assets are embedded in the binary; `no-cache` makes browsers revalidate so
/// an upgraded binary's UI shows up without a hard refresh.
fn asset(content_type: &'static str, body: impl IntoResponse) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-cache")],
        body,
    )
}

async fn require_password(
    State(password): State<Arc<String>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok())
        .and_then(|v| v.split_once(':').map(|(_, p)| p.to_string()));

    match supplied {
        Some(p) if constant_time_eq(p.as_bytes(), password.as_bytes()) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"twitch-chat\"")],
            "unauthorized",
        )
            .into_response(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Read-only SQLite connections, reused across requests. Queries run on the
/// blocking thread pool.
struct Db {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
}

impl Db {
    fn open(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    async fn query<T, F>(self: &Arc<Self>, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
    {
        let db = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let pooled = db.idle.lock().unwrap().pop();
            let conn = match pooled {
                Some(conn) => conn,
                None => db.open()?,
            };
            let result = f(&conn);
            let mut idle = db.idle.lock().unwrap();
            if idle.len() < MAX_IDLE_CONNECTIONS {
                idle.push(conn);
            }
            result
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    }
}

enum ApiError {
    BadRequest(String),
    NotFound,
    Internal(String),
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::Internal(m) => {
                eprintln!("request failed: {m}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// Accumulates `WHERE` conditions and their bound values.
#[derive(Default)]
struct Filter {
    conds: Vec<String>,
    args: Vec<Value>,
}

impl Filter {
    fn add(&mut self, cond: &str, args: impl IntoIterator<Item = Value>) {
        self.conds.push(cond.to_string());
        self.args.extend(args);
    }

    fn range(range: &RangeParams) -> Self {
        let mut f = Filter::default();
        if let Some(channel) = non_empty(&range.channel) {
            f.add("channel = ?", [Value::Text(channel.to_lowercase())]);
        }
        if let Some(from) = range.from {
            f.add("sent_at >= ?", [Value::Integer(from)]);
        }
        if let Some(to) = range.to {
            f.add("sent_at < ?", [Value::Integer(to)]);
        }
        f
    }

    fn sql(&self) -> String {
        if self.conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.conds.join(" AND "))
        }
    }
}

fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

#[derive(Deserialize)]
struct RangeParams {
    channel: Option<String>,
    /// Inclusive lower bound, unix ms.
    from: Option<i64>,
    /// Exclusive upper bound, unix ms.
    to: Option<i64>,
}

#[derive(Serialize)]
struct ChannelSummary {
    channel: String,
    messages: i64,
    first_at: i64,
    last_at: i64,
}

async fn channels(State(db): State<Arc<Db>>) -> ApiResult<Vec<ChannelSummary>> {
    db.query(|conn| {
        let mut stmt = conn.prepare(
            "SELECT channel, count(*), min(sent_at), max(sent_at)
             FROM messages GROUP BY channel ORDER BY max(sent_at) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ChannelSummary {
                channel: r.get(0)?,
                messages: r.get(1)?,
                first_at: r.get(2)?,
                last_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct Storage {
    /// On-disk size of the database, including its `-wal` and `-shm` files.
    bytes: u64,
    messages: i64,
    /// Messages received in the last 24 hours, to estimate growth.
    messages_24h: i64,
}

async fn storage(State(db): State<Arc<Db>>) -> ApiResult<Storage> {
    let db_path = db.path.clone();
    db.query(move |conn| {
        let bytes = ["", "-wal", "-shm"]
            .iter()
            .filter_map(|suffix| {
                let mut path = db_path.clone().into_os_string();
                path.push(suffix);
                std::fs::metadata(path).ok()
            })
            .map(|m| m.len())
            .sum();
        let day_ago = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
            - 24 * 60 * 60 * 1000;
        let (messages, messages_24h) = conn.query_row(
            "SELECT count(*), (SELECT count(*) FROM messages WHERE sent_at >= ?) FROM messages",
            [day_ago],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Storage { bytes, messages, messages_24h })
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct Stats {
    messages: i64,
    chatters: i64,
    first_at: Option<i64>,
    last_at: Option<i64>,
}

async fn stats(State(db): State<Arc<Db>>, Query(range): Query<RangeParams>) -> ApiResult<Stats> {
    let filter = Filter::range(&range);
    db.query(move |conn| {
        let sql = format!(
            "SELECT count(*), count(DISTINCT coalesce(user_id, login)), min(sent_at), max(sent_at)
             FROM messages {}",
            filter.sql()
        );
        Ok(conn.query_row(&sql, params_from_iter(&filter.args), |r| {
            Ok(Stats {
                messages: r.get(0)?,
                chatters: r.get(1)?,
                first_at: r.get(2)?,
                last_at: r.get(3)?,
            })
        })?)
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct ActivityParams {
    /// Bucket width, ms.
    bucket: i64,
    /// Added to timestamps before bucketing so day/hour buckets align with
    /// the viewer's local time, ms.
    #[serde(default)]
    tz_offset: i64,
}

#[derive(Serialize)]
struct Activity {
    bucket: i64,
    /// `[bucket_start, messages]`, only non-empty buckets.
    points: Vec<(i64, i64)>,
}

async fn activity(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<ActivityParams>,
) -> ApiResult<Activity> {
    let mut bucket = p.bucket.max(1000);
    if let (Some(from), Some(to)) = (range.from, range.to) {
        bucket = bucket.max((to - from) / MAX_ACTIVITY_BUCKETS);
    }
    let offset = p.tz_offset;
    let filter = Filter::range(&range);
    db.query(move |conn| {
        let sql = format!(
            "SELECT ((sent_at + ?1) / ?2) * ?2 - ?1 AS t, count(*)
             FROM messages {} GROUP BY t ORDER BY t",
            filter.sql()
        );
        // Numbered and positional parameters share one sequence: ?1 and ?2
        // come first, so the filter's `?` placeholders bind from 3 onwards.
        let args = [Value::Integer(offset), Value::Integer(bucket)]
            .into_iter()
            .chain(filter.args);
        let mut stmt = conn.prepare(&sql)?;
        let points = stmt
            .query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(Activity { bucket, points })
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct TopParams {
    limit: Option<i64>,
}

#[derive(Serialize)]
struct TopChatter {
    user_id: Option<String>,
    login: String,
    display_name: String,
    messages: i64,
}

async fn top_chatters(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<TopParams>,
) -> ApiResult<Vec<TopChatter>> {
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let filter = Filter::range(&range);
    db.query(move |conn| {
        let sql = format!(
            "SELECT user_id, login, display_name, count(*) AS n
             FROM messages {} GROUP BY coalesce(user_id, login) ORDER BY n DESC LIMIT {limit}",
            filter.sql()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(&filter.args), |r| {
            Ok(TopChatter {
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
struct TopMessage {
    text: String,
    messages: i64,
    chatters: i64,
}

async fn top_messages(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<TopParams>,
) -> ApiResult<Vec<TopMessage>> {
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let filter = Filter::range(&range);
    db.query(move |conn| {
        let sql = format!(
            "SELECT text, count(*) AS n, count(DISTINCT coalesce(user_id, login))
             FROM messages {} GROUP BY text HAVING n > 1 ORDER BY n DESC LIMIT {limit}",
            filter.sql()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(&filter.args), |r| {
            Ok(TopMessage {
                text: r.get(0)?,
                messages: r.get(1)?,
                chatters: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
struct Message {
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
}

const MESSAGE_COLUMNS: &str =
    "id, msg_id, channel, user_id, login, display_name, color, badges, text, sent_at";

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
    })
}

#[derive(Deserialize)]
struct MessagesParams {
    user_id: Option<String>,
    login: Option<String>,
    /// Full-text query; every word must appear (as a word prefix).
    q: Option<String>,
    /// Cursor from a previous page's `next`.
    before: Option<String>,
    /// Only messages with an id above this, for polling. The oldest new
    /// messages come first when there are more than `limit`, so repeated polls
    /// never skip any.
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct MessagePage {
    messages: Vec<Message>,
    /// Pass as `before` to fetch the next (older) page.
    next: Option<String>,
}

/// Newest first, paginated with a `(sent_at, id)` cursor.
async fn messages(
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
    if let Some(before) = non_empty(&p.before) {
        let (sent_at, id) = before
            .split_once(':')
            .and_then(|(t, id)| Some((t.parse::<i64>().ok()?, id.parse::<i64>().ok()?)))
            .ok_or_else(|| ApiError::BadRequest("invalid cursor".to_string()))?;
        filter.add(
            "(sent_at < ? OR (sent_at = ? AND id < ?))",
            [Value::Integer(sent_at), Value::Integer(sent_at), Value::Integer(id)],
        );
    }

    let after = p.after;
    if let Some(after) = after {
        filter.add("id > ?", [Value::Integer(after)]);
    }

    db.query(move |conn| {
        if after.is_some() {
            let sql = format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages {} ORDER BY id LIMIT {limit}",
                filter.sql()
            );
            let mut messages: Vec<Message> = conn
                .prepare(&sql)?
                .query_map(params_from_iter(&filter.args), message_from_row)?
                .collect::<Result<_, _>>()?;
            messages.reverse();
            return Ok(MessagePage { messages, next: None });
        }

        let sql = format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages {} ORDER BY sent_at DESC, id DESC LIMIT {}",
            filter.sql(),
            limit + 1
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut messages: Vec<Message> = stmt
            .query_map(params_from_iter(&filter.args), message_from_row)?
            .collect::<Result<_, _>>()?;
        let next = if messages.len() > limit {
            messages.truncate(limit);
            messages.last().map(|m| format!("{}:{}", m.sent_at, m.id))
        } else {
            None
        };
        Ok(MessagePage { messages, next })
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
struct ContextParams {
    n: Option<i64>,
}

#[derive(Serialize)]
struct MessageContext {
    focus: i64,
    messages: Vec<Message>,
}

/// The message plus up to `n` messages before and after it in the same channel.
async fn message_context(
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

#[derive(Deserialize)]
struct UserSearchParams {
    q: String,
}

#[derive(Serialize)]
struct UserMatch {
    user_id: Option<String>,
    login: String,
    display_name: String,
    messages: i64,
}

/// Logins starting with `q`, alphabetically.
async fn search_users(State(db): State<Arc<Db>>, Query(p): Query<UserSearchParams>) -> ApiResult<Vec<UserMatch>> {
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
struct UserChannel {
    channel: String,
    messages: i64,
    first_at: i64,
    last_at: i64,
}

#[derive(Serialize)]
struct UserName {
    login: String,
    display_name: String,
    first_at: i64,
    last_at: i64,
}

#[derive(Serialize)]
struct User {
    user_id: String,
    /// Most recent first.
    names: Vec<UserName>,
    channels: Vec<UserChannel>,
}

async fn user(State(db): State<Arc<Db>>, UrlPath(user_id): UrlPath<String>) -> ApiResult<User> {
    db.query(move |conn| {
        let names: Vec<UserName> = conn
            .prepare(
                "SELECT login, display_name, min(sent_at), max(sent_at)
                 FROM messages WHERE user_id = ? GROUP BY login, display_name ORDER BY max(sent_at) DESC",
            )?
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
            .prepare(
                "SELECT channel, count(*), min(sent_at), max(sent_at)
                 FROM messages WHERE user_id = ? GROUP BY channel ORDER BY count(*) DESC",
            )?
            .query_map([&user_id], |r| {
                Ok(UserChannel {
                    channel: r.get(0)?,
                    messages: r.get(1)?,
                    first_at: r.get(2)?,
                    last_at: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(User { user_id, names, channels })
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
