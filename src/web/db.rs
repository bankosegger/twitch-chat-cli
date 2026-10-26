//! Database access for the web server, and the error type every handler
//! returns.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

const MAX_IDLE_CONNECTIONS: usize = 8;

/// Read-only SQLite connections, reused across requests. Queries run on the
/// blocking thread pool.
pub(super) struct Db {
    pub(super) path: PathBuf,
    idle: Mutex<Vec<Connection>>,
}

impl Db {
    pub(super) fn new(path: PathBuf) -> Self {
        Self {
            path,
            idle: Mutex::new(Vec::new()),
        }
    }

    fn open(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    pub(super) async fn query<T, F>(self: &Arc<Self>, f: F) -> Result<T, ApiError>
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
    pub(super) async fn write<T, F>(self: &Arc<Self>, f: F) -> Result<T, ApiError>
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

    /// The channel's Twitch user id, from its latest message: chat messages
    /// carry it, so no API call is needed.
    pub(super) async fn room_id(self: &Arc<Self>, channel: String) -> Result<Option<String>, ApiError> {
        self.query(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT room_id FROM messages WHERE channel = ? AND room_id IS NOT NULL ORDER BY sent_at DESC LIMIT 1",
                    [channel],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
        })
        .await
    }
}

pub(super) enum ApiError {
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

pub(super) type ApiResult<T> = Result<Json<T>, ApiError>;
