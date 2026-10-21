use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{FromRef, Path as UrlPath, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, OpenFlags, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::chat::{self, Account, SendError};
use crate::helix::Helix;
use crate::store::{self, RemoveWatched, Store};

mod twitch;
use twitch::Twitch;

/// Handlers take the part they need: `State<Arc<Db>>`, `State<Arc<Twitch>>`
/// or `State<Arc<Account>>`.
#[derive(Clone)]
struct AppState {
    db: Arc<Db>,
    twitch: Arc<Twitch>,
    account: Arc<Account>,
}

impl FromRef<AppState> for Arc<Account> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.account)
    }
}

impl FromRef<AppState> for Arc<Db> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.db)
    }
}

impl FromRef<AppState> for Arc<Twitch> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.twitch)
    }
}

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const STYLE_CSS: &str = include_str!("web/style.css");
const FONT_WOFF2: &[u8] = include_bytes!("fonts/PlusJakartaSans-Variable.woff2");

const MAX_IDLE_CONNECTIONS: usize = 8;
const MAX_ACTIVITY_BUCKETS: i64 = 2000;

/// Serves the web UI and its JSON API until interrupted. When `password` is
/// set, every request requires HTTP basic auth with that password (any
/// username).
pub async fn run(db_path: &Path, bind: SocketAddr, password: Option<String>, token_path: &Path) -> io::Result<()> {
    // Opening read-write once creates the schema (and the search index) if the
    // database is new or was written by an older recorder.
    Store::open(db_path).map_err(io::Error::other)?;

    let db = Arc::new(Db {
        path: db_path.to_path_buf(),
        idle: Mutex::new(Vec::new()),
    });
    let helix = Helix::from_env();
    if helix.is_none() {
        eprintln!("set TWITCH_CLIENT_ID and TWITCH_CLIENT_SECRET to show profile pictures and badges");
    }
    let account = Account::load(token_path).map_err(io::Error::other)?;
    if !account.enabled() {
        eprintln!("set TWITCH_CLIENT_ID to send chat messages");
    } else {
        match account.sender() {
            Some(sender) => eprintln!("sending chat messages as {}", sender.login()),
            None => eprintln!("log in to Twitch in the web UI to send chat messages"),
        }
        if password.is_none() && !bind.ip().is_loopback() {
            eprintln!("warning: anyone who can reach this server can log in, and send chat messages as the logged-in account; set TWITCH_CHAT_PASSWORD");
        }
    }
    let state = AppState {
        db,
        twitch: Arc::new(Twitch::new(helix)),
        account,
    };

    let api = Router::new()
        .route("/channels", get(channels))
        .route("/channels/{channel}", delete(delete_channel))
        .route("/channels/{channel}/messages", post(send_message))
        .route("/chat-account", get(chat_account).delete(logout))
        .route("/chat-account/login", post(start_login))
        .route("/storage", get(storage))
        .route("/download", get(download))
        .route("/stats", get(stats))
        .route("/activity", get(activity))
        .route("/top-chatters", get(top_chatters))
        .route("/top-messages", get(top_messages))
        .route("/messages", get(messages))
        .route("/messages/{id}/context", get(message_context))
        .route("/events", get(events))
        .route("/moderation", get(moderation))
        .route("/viewers", get(viewers))
        .route("/users", get(search_users))
        .route("/users/{user_id}", get(user))
        .route("/watched", get(watched).post(add_watched))
        .route("/watched/{channel}", delete(remove_watched))
        .route("/profile/{login}", get(twitch::profile))
        .route("/badges", get(twitch::badges))
        .with_state(state);

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

    /// Runs `f` on a fresh read-write connection. Only the watch-list
    /// endpoints write, and rarely, so these are not pooled.
    async fn write<T, F>(self: &Arc<Self>, f: F) -> Result<T, ApiError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
    {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            f(&conn)
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    }
}

enum ApiError {
    BadRequest(String),
    NotFound,
    /// A feature that isn't set up, e.g. sending without a login.
    Unavailable(String),
    /// Twitch failed or couldn't be reached.
    Upstream(String),
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
            ApiError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            ApiError::Upstream(m) => {
                eprintln!("twitch request failed: {m}");
                (StatusCode::BAD_GATEWAY, m)
            }
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
        Self::range_on(range, "sent_at")
    }

    /// Like `range`, for a table whose timestamp column is `time_column`.
    fn range_on(range: &RangeParams, time_column: &str) -> Self {
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
struct WatchedChannel {
    channel: String,
    /// Set on the recorder's command line; can't be removed from the dashboard.
    pinned: bool,
    added_at: i64,
    last_message_at: Option<i64>,
}

fn list_watched(conn: &Connection) -> Result<Vec<WatchedChannel>, ApiError> {
    let mut stmt = conn.prepare_cached(
        "SELECT channel, pinned, added_at,
                (SELECT max(sent_at) FROM messages m WHERE m.channel = w.channel)
         FROM watched_channels w ORDER BY channel",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(WatchedChannel {
            channel: r.get(0)?,
            pinned: r.get(1)?,
            added_at: r.get(2)?,
            last_message_at: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

async fn watched(State(db): State<Arc<Db>>) -> ApiResult<Vec<WatchedChannel>> {
    db.query(list_watched).await.map(Json)
}

#[derive(Deserialize)]
struct AddWatched {
    channel: String,
}

/// Takes a JSON body, so browsers won't send it cross-site without a CORS
/// preflight (which this server never approves).
async fn add_watched(State(db): State<Arc<Db>>, Json(body): Json<AddWatched>) -> ApiResult<Vec<WatchedChannel>> {
    let channel = store::normalize_channel(&body.channel).ok_or_else(|| {
        ApiError::BadRequest("Channel names are 1–25 letters, digits or underscores.".to_string())
    })?;
    db.write(move |conn| {
        store::add_watched(conn, &channel)?;
        list_watched(conn)
    })
    .await
    .map(Json)
}

async fn remove_watched(State(db): State<Arc<Db>>, UrlPath(channel): UrlPath<String>) -> ApiResult<Vec<WatchedChannel>> {
    let channel = store::normalize_channel(&channel).ok_or(ApiError::NotFound)?;
    db.write(move |conn| match store::remove_watched(conn, &channel)? {
        RemoveWatched::Removed => list_watched(conn),
        RemoveWatched::NotFound => Err(ApiError::NotFound),
        RemoveWatched::Pinned => Err(ApiError::BadRequest(format!(
            "{channel} is set on the recorder's command line; remove it there."
        ))),
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct DeleteChannelParams {
    /// Also remove the channel from the recorder's list.
    #[serde(default)]
    stop_recording: bool,
}

#[derive(Serialize)]
struct DeletedChannel {
    messages: usize,
    events: usize,
    /// False when recording wasn't asked to stop, or the channel is pinned on
    /// the recorder's command line.
    stopped_recording: bool,
}

/// Deletes everything recorded in a channel. DELETE isn't a "simple" method,
/// so browsers won't send it cross-site without a CORS preflight.
async fn delete_channel(
    State(db): State<Arc<Db>>,
    UrlPath(channel): UrlPath<String>,
    Query(p): Query<DeleteChannelParams>,
) -> ApiResult<DeletedChannel> {
    let channel = store::normalize_channel(&channel).ok_or(ApiError::NotFound)?;
    db.write(move |conn| {
        // Stop recording first, so no new messages arrive after the delete.
        let stopped_recording =
            p.stop_recording && matches!(store::remove_watched(conn, &channel)?, RemoveWatched::Removed);
        let deleted = store::delete_channel(conn, &channel)?;
        Ok(DeletedChannel {
            messages: deleted.messages,
            events: deleted.events,
            stopped_recording,
        })
    })
    .await
    .map(Json)
}

/// The account messages are sent as, and any login in progress.
async fn chat_account(State(account): State<Arc<Account>>) -> Json<chat::Status> {
    Json(account.status())
}

/// No fields: the JSON body only makes browsers preflight cross-site requests,
/// which this server never approves.
#[derive(Deserialize)]
struct StartLogin {}

/// Starts a login: the response's `pending` holds the code to enter on Twitch.
async fn start_login(State(account): State<Arc<Account>>, Json(_): Json<StartLogin>) -> ApiResult<chat::Status> {
    if !account.enabled() {
        return Err(ApiError::Unavailable("Set TWITCH_CLIENT_ID to log in.".to_string()));
    }
    account.start_login().await.map(Json).map_err(ApiError::Upstream)
}

/// Cancels a pending login and logs out. DELETE isn't a "simple" method, so
/// browsers won't send it cross-site without a CORS preflight.
async fn logout(State(account): State<Arc<Account>>) -> ApiResult<chat::Status> {
    account.logout().await.map(Json).map_err(ApiError::Internal)
}

#[derive(Deserialize)]
struct SendMessage {
    message: String,
    /// Twitch id (`msg_id`) of the message to reply to.
    reply_to: Option<String>,
}

#[derive(Serialize)]
struct SentMessage {
    msg_id: String,
}

/// Posts a chat message as the logged-in account. Takes a JSON body, so
/// browsers won't send it cross-site without a CORS preflight.
async fn send_message(
    State(account): State<Arc<Account>>,
    UrlPath(channel): UrlPath<String>,
    Json(body): Json<SendMessage>,
) -> ApiResult<SentMessage> {
    let sender = account
        .sender()
        .ok_or_else(|| ApiError::Unavailable("Not logged in to Twitch.".to_string()))?;
    let channel = store::normalize_channel(&channel).ok_or(ApiError::NotFound)?;
    let message = body.message.trim();
    if message.is_empty() {
        return Err(ApiError::BadRequest("The message is empty.".to_string()));
    }
    if message.chars().count() > chat::MAX_MESSAGE_LEN {
        return Err(ApiError::BadRequest(format!(
            "Messages are at most {} characters.",
            chat::MAX_MESSAGE_LEN
        )));
    }
    match sender.send(&channel, message, body.reply_to.as_deref()).await {
        Ok(msg_id) => Ok(Json(SentMessage { msg_id })),
        Err(SendError::NoSuchChannel) => Err(ApiError::NotFound),
        Err(SendError::Rejected(m)) => Err(ApiError::BadRequest(m)),
        Err(SendError::Failed(m)) => Err(ApiError::Upstream(m)),
    }
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

/// Streams a consistent copy of the whole database. The recorder keeps
/// writing (partly into the `-wal` file), so the file on disk can't be sent
/// as is: `VACUUM INTO` writes a snapshot next to it, deleted once sent.
async fn download(State(db): State<Arc<Db>>) -> Result<Response, ApiError> {
    let dir = match db.path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let snapshot = db
        .query(move |conn| {
            // VACUUM INTO accepts an existing file as long as it is empty.
            let snapshot = tempfile::Builder::new()
                .prefix(".download-")
                .suffix(".db")
                .tempfile_in(&dir)
                .map_err(|e| ApiError::Internal(e.to_string()))?
                .into_temp_path();
            let target = snapshot.to_str().ok_or_else(|| ApiError::Internal("non-UTF-8 temp path".into()))?;
            conn.execute("VACUUM INTO ?", [target])?;
            Ok(snapshot)
        })
        .await?;

    let file = tokio::fs::File::open(&snapshot)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let len = file.metadata().await.map_err(|e| ApiError::Internal(e.to_string()))?.len();
    // The state tuple drops the file before the temp path, so the snapshot is
    // closed before it is deleted (required on Windows), whether the download
    // finishes or the client disconnects.
    let body = futures_util::stream::try_unfold((file, snapshot), |(mut file, snapshot)| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0; 64 * 1024];
        let n = file.read(&mut buf).await?;
        if n == 0 {
            return Ok::<_, io::Error>(None);
        }
        buf.truncate(n);
        Ok(Some((axum::body::Bytes::from(buf), (file, snapshot))))
    });

    let name = db.path.file_name().and_then(|n| n.to_str()).unwrap_or("chat.db").replace('"', "");
    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.sqlite3".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        axum::body::Body::from_stream(body),
    )
        .into_response())
}

#[derive(Serialize)]
struct Stats {
    messages: i64,
    chatters: i64,
    first_at: Option<i64>,
    last_at: Option<i64>,
    /// New subs and resubs, including Prime.
    subs: i64,
    /// Gifted subs, counted one per recipient.
    gift_subs: i64,
    bits: i64,
    cheers: i64,
    bans: i64,
    timeouts: i64,
    /// Single messages deleted by moderators.
    deletions: i64,
    /// Highest sampled viewer count; `None` without samples.
    peak_viewers: Option<i64>,
    /// The latest sample, if the channel is live now.
    live: Option<LiveStream>,
}

#[derive(Serialize)]
struct LiveStream {
    title: String,
    game_name: String,
    viewer_count: i64,
    started_at: i64,
    sampled_at: i64,
}

/// Samples are taken every minute; older than this means the stream ended
/// (or the recorder stopped).
const LIVE_SAMPLE_MAX_AGE_MS: i64 = 3 * 60 * 1000;

async fn stats(State(db): State<Arc<Db>>, Query(range): Query<RangeParams>) -> ApiResult<Stats> {
    let filter = Filter::range(&range);
    let samples_filter = Filter::range_on(&range, "sampled_at");
    let channel = non_empty(&range.channel).map(str::to_lowercase);
    db.query(move |conn| {
        let sql = format!(
            "SELECT count(*), count(DISTINCT coalesce(user_id, login)), min(sent_at), max(sent_at)
             FROM messages {}",
            filter.sql()
        );
        let (messages, chatters, first_at, last_at) =
            conn.query_row(&sql, params_from_iter(&filter.args), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
        let sql = format!(
            "SELECT count(*) FILTER (WHERE kind IN ('sub', 'resub')),
                    count(*) FILTER (WHERE kind = 'subgift'),
                    coalesce(sum(count) FILTER (WHERE kind = 'cheer'), 0),
                    count(*) FILTER (WHERE kind = 'cheer')
             FROM events {}",
            filter.sql()
        );
        let (subs, gift_subs, bits, cheers) = conn.query_row(&sql, params_from_iter(&filter.args), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        let sql = format!(
            "SELECT count(*) FILTER (WHERE kind = 'ban'),
                    count(*) FILTER (WHERE kind = 'timeout'),
                    count(*) FILTER (WHERE kind = 'delete')
             FROM moderation {}",
            filter.sql()
        );
        let (bans, timeouts, deletions) = conn.query_row(&sql, params_from_iter(&filter.args), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        let peak_viewers = conn.query_row(
            &format!("SELECT max(viewer_count) FROM stream_samples {}", samples_filter.sql()),
            params_from_iter(&samples_filter.args),
            |r| r.get(0),
        )?;
        let live = match channel {
            Some(channel) => conn
                .query_row(
                    "SELECT title, game_name, viewer_count, started_at, sampled_at FROM stream_samples
                     WHERE channel = ?1 AND sampled_at >= ?2 ORDER BY sampled_at DESC LIMIT 1",
                    rusqlite::params![channel, store::now_millis() - LIVE_SAMPLE_MAX_AGE_MS],
                    |r| {
                        Ok(LiveStream {
                            title: r.get(0)?,
                            game_name: r.get(1)?,
                            viewer_count: r.get(2)?,
                            started_at: r.get(3)?,
                            sampled_at: r.get(4)?,
                        })
                    },
                )
                .optional()?,
            None => None,
        };
        Ok(Stats {
            messages,
            chatters,
            first_at,
            last_at,
            subs,
            gift_subs,
            bits,
            cheers,
            bans,
            timeouts,
            deletions,
            peak_viewers,
            live,
        })
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

/// Messages per bucket.
async fn activity(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<ActivityParams>,
) -> ApiResult<Activity> {
    let filter = Filter::range(&range);
    bucketed(&db, &range, &p, "count(*)", "messages", "sent_at", filter).await
}

/// Peak viewer count per bucket, while live.
async fn viewers(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<ActivityParams>,
) -> ApiResult<Activity> {
    let filter = Filter::range_on(&range, "sampled_at");
    bucketed(&db, &range, &p, "max(viewer_count)", "stream_samples", "sampled_at", filter).await
}

/// `SELECT bucket, {value} FROM {table}`, grouped into buckets of `time_column`.
async fn bucketed(
    db: &Arc<Db>,
    range: &RangeParams,
    p: &ActivityParams,
    value: &'static str,
    table: &'static str,
    time_column: &'static str,
    filter: Filter,
) -> ApiResult<Activity> {
    let mut bucket = p.bucket.max(1000);
    if let (Some(from), Some(to)) = (range.from, range.to) {
        bucket = bucket.max((to - from) / MAX_ACTIVITY_BUCKETS);
    }
    let offset = p.tz_offset;
    db.query(move |conn| {
        let sql = format!(
            "SELECT (({time_column} + ?1) / ?2) * ?2 - ?1 AS t, {value}
             FROM {table} {} GROUP BY t ORDER BY t",
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
struct MessagesParams {
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

/// Pagination shared by `/messages` and `/events`: newest first with a
/// `sent_at:id` cursor, or, when polling with `after`, rows above that id.
struct PageParams {
    polling: bool,
    limit: usize,
}

impl PageParams {
    /// Adds the cursor conditions to `filter`.
    fn new(filter: &mut Filter, before: &Option<String>, after: Option<i64>, limit: usize) -> Result<Self, ApiError> {
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
    fn fetch<T>(
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

#[derive(Serialize)]
struct Event {
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
struct EventsParams {
    user_id: Option<String>,
    /// Comma-separated `kind`s, e.g. `sub,resub,subgift`.
    kinds: Option<String>,
    before: Option<String>,
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct EventPage {
    events: Vec<Event>,
    next: Option<String>,
}

/// Subs, raids and other events, paginated like `/messages`. Single gifts
/// from a gift bomb are left out; its `submysterygift` event stands for them.
async fn events(
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
struct ModRow {
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
struct ModParams {
    /// Actions against this user.
    user_id: Option<String>,
    before: Option<String>,
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct ModPage {
    moderation: Vec<ModRow>,
    next: Option<String>,
}

/// Bans, timeouts, /clear and deleted messages, paginated like `/messages`.
async fn moderation(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<ModParams>,
) -> ApiResult<ModPage> {
    let limit = p.limit.unwrap_or(100).clamp(1, 500);
    let mut filter = Filter::range(&range);
    if let Some(user_id) = non_empty(&p.user_id) {
        filter.add("target_user_id = ?", [Value::Text(user_id.to_string())]);
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
    /// Subs, gifts, raids, ... they triggered (gift bombs count once).
    events: i64,
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

/// Everything a user did: their messages (`is_message` = 1) and the events
/// they triggered, so someone who subbed but never chatted still has a page.
const USER_ACTIVITY: &str = "(
    SELECT channel, login, display_name, sent_at, 1 AS is_message FROM messages WHERE user_id = ?1
    UNION ALL
    SELECT channel, login, display_name, sent_at, 0 FROM events WHERE user_id = ?1 AND bulk_gift = 0
)";

async fn user(State(db): State<Arc<Db>>, UrlPath(user_id): UrlPath<String>) -> ApiResult<User> {
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
