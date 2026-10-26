//! The Channels page: what has been recorded, which channels the recorder is
//! in, and the database as a whole (size, download, deletion).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::db::{ApiError, ApiResult, Db};
use crate::store::{self, RemoveWatched};

#[derive(Serialize)]
pub(super) struct ChannelSummary {
    channel: String,
    messages: i64,
    first_at: i64,
    last_at: i64,
}

pub(super) async fn channels(State(db): State<Arc<Db>>) -> ApiResult<Vec<ChannelSummary>> {
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
pub(super) struct WatchedChannel {
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

pub(super) async fn watched(State(db): State<Arc<Db>>) -> ApiResult<Vec<WatchedChannel>> {
    db.query(list_watched).await.map(Json)
}

#[derive(Deserialize)]
pub(super) struct AddWatched {
    channel: String,
}

/// Takes a JSON body, so browsers won't send it cross-site without a CORS
/// preflight (which this server never approves).
pub(super) async fn add_watched(
    State(db): State<Arc<Db>>,
    Json(body): Json<AddWatched>,
) -> ApiResult<Vec<WatchedChannel>> {
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

pub(super) async fn remove_watched(
    State(db): State<Arc<Db>>,
    UrlPath(channel): UrlPath<String>,
) -> ApiResult<Vec<WatchedChannel>> {
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
pub(super) struct DeleteChannelParams {
    /// Also remove the channel from the recorder's list.
    #[serde(default)]
    stop_recording: bool,
}

#[derive(Serialize)]
pub(super) struct DeletedChannel {
    messages: usize,
    events: usize,
    /// False when recording wasn't asked to stop, or the channel is pinned on
    /// the recorder's command line.
    stopped_recording: bool,
}

/// Deletes everything recorded in a channel. DELETE isn't a "simple" method,
/// so browsers won't send it cross-site without a CORS preflight.
pub(super) async fn delete_channel(
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

#[derive(Serialize)]
pub(super) struct Storage {
    /// On-disk size of the database, including its `-wal` and `-shm` files.
    bytes: u64,
    messages: i64,
    /// Messages received in the last 24 hours, to estimate growth.
    messages_24h: i64,
}

pub(super) async fn storage(State(db): State<Arc<Db>>) -> ApiResult<Storage> {
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
pub(super) async fn download(State(db): State<Arc<Db>>) -> Result<Response, ApiError> {
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
