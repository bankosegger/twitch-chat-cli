//! Aggregates for the channel dashboard: headline counts, the activity and
//! viewer charts, and the leaderboards.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use rusqlite::types::Value;
use rusqlite::{params_from_iter, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::db::{ApiResult, Db};
use super::filter::{non_empty, Filter, RangeParams};
use crate::store;

/// Caps how many points a chart request can return.
const MAX_ACTIVITY_BUCKETS: i64 = 2000;

#[derive(Serialize)]
pub(super) struct Stats {
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
pub(super) struct LiveStream {
    title: String,
    game_name: String,
    viewer_count: i64,
    started_at: i64,
    sampled_at: i64,
}

/// Samples are taken every minute; older than this means the stream ended
/// (or the recorder stopped).
const LIVE_SAMPLE_MAX_AGE_MS: i64 = 3 * 60 * 1000;

pub(super) async fn stats(State(db): State<Arc<Db>>, Query(range): Query<RangeParams>) -> ApiResult<Stats> {
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
pub(super) struct ActivityParams {
    /// Bucket width, ms.
    bucket: i64,
    /// Added to timestamps before bucketing so day/hour buckets align with
    /// the viewer's local time, ms.
    #[serde(default)]
    tz_offset: i64,
}

#[derive(Serialize)]
pub(super) struct Activity {
    bucket: i64,
    /// `[bucket_start, messages]`, only non-empty buckets.
    points: Vec<(i64, i64)>,
}

/// Messages per bucket.
pub(super) async fn activity(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
    Query(p): Query<ActivityParams>,
) -> ApiResult<Activity> {
    let filter = Filter::range(&range);
    bucketed(&db, &range, &p, "count(*)", "messages", "sent_at", filter).await
}

/// Peak viewer count per bucket, while live.
pub(super) async fn viewers(
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
pub(super) struct TopParams {
    limit: Option<i64>,
}

#[derive(Serialize)]
pub(super) struct TopChatter {
    user_id: Option<String>,
    login: String,
    display_name: String,
    messages: i64,
}

pub(super) async fn top_chatters(
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
pub(super) struct RoleHolder {
    user_id: Option<String>,
    login: String,
    display_name: String,
    /// `moderator` or `vip`.
    role: &'static str,
    messages: i64,
    last_at: i64,
}

/// Moderators and VIPs, as seen in the badges of each chatter's latest
/// message in the range: Twitch offers no list of them without the
/// broadcaster's own token.
pub(super) async fn roles(
    State(db): State<Arc<Db>>,
    Query(range): Query<RangeParams>,
) -> ApiResult<Vec<RoleHolder>> {
    let filter = Filter::range(&range);
    db.query(move |conn| {
        // SQLite takes bare columns (`badges`) from the row that has the max().
        let sql = format!(
            "SELECT * FROM (
                 SELECT user_id, login, display_name, ',' || badges AS b, max(sent_at), count(*)
                 FROM messages {} GROUP BY coalesce(user_id, login)
             ) WHERE instr(b, ',moderator/') OR instr(b, ',lead_moderator/') OR instr(b, ',vip/')",
            filter.sql()
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(&filter.args), |r| {
            let badges: String = r.get(3)?;
            Ok(RoleHolder {
                user_id: r.get(0)?,
                login: r.get(1)?,
                display_name: r.get(2)?,
                role: if badges.contains(",vip/") { "vip" } else { "moderator" },
                last_at: r.get(4)?,
                messages: r.get(5)?,
            })
        })?;
        let mut holders: Vec<RoleHolder> = rows.collect::<Result<_, _>>()?;
        holders.sort_by(|a, b| a.role.cmp(b.role).then(b.messages.cmp(&a.messages)));
        Ok(holders)
    })
    .await
    .map(Json)
}

#[derive(Serialize)]
pub(super) struct TopMessage {
    text: String,
    messages: i64,
    chatters: i64,
}

pub(super) async fn top_messages(
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
