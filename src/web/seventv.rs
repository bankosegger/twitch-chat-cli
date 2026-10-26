//! 7TV emotes (https://7tv.app): the global set and each channel's own set,
//! from 7TV's public API. No credentials are needed. Answers are cached in
//! memory; a channel without a 7TV account just has no channel emotes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::db::{ApiResult, Db};

const API_URL: &str = "https://7tv.io/v3";
/// Streamers add and remove 7TV emotes now and then, so don't keep them long.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
/// Set on an emote's `data.flags` when it's meant to be laid over the
/// previous emote (e.g. a hat or a rain effect).
const ZERO_WIDTH: u32 = 1 << 8;

pub struct SevenTv {
    http: reqwest::Client,
    /// Keyed by Twitch room id; `""` holds the global set.
    sets: Mutex<HashMap<String, (Instant, Arc<EmoteMap>)>>,
}

/// Emote name, as typed in chat → emote.
pub type EmoteMap = HashMap<String, Emote>;

#[derive(Clone, Debug, Serialize)]
pub struct Emote {
    /// Image directory: append `/1x.webp`, `/2x.webp`, ...
    url: String,
    zero_width: bool,
}

#[derive(Deserialize)]
struct EmoteSet {
    #[serde(default)]
    emotes: Option<Vec<ActiveEmote>>,
}

#[derive(Deserialize)]
struct ActiveEmote {
    /// The name in this set, which may differ from the emote's own name.
    name: String,
    data: EmoteData,
}

#[derive(Deserialize)]
struct EmoteData {
    #[serde(default)]
    flags: u32,
    host: EmoteHost,
}

#[derive(Deserialize)]
struct EmoteHost {
    /// Protocol-relative, e.g. `//cdn.7tv.app/emote/<id>`.
    url: String,
}

#[derive(Deserialize)]
struct TwitchConnection {
    emote_set: Option<EmoteSet>,
}

fn emote_map(set: EmoteSet) -> EmoteMap {
    set.emotes
        .unwrap_or_default()
        .into_iter()
        .map(|e| {
            let url = match e.data.host.url.strip_prefix("//") {
                Some(rest) => format!("https://{rest}"),
                None => e.data.host.url,
            };
            (
                e.name,
                Emote {
                    url,
                    zero_width: e.data.flags & ZERO_WIDTH != 0,
                },
            )
        })
        .collect()
}

impl SevenTv {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("default TLS backend is available");
        Self {
            http,
            sets: Mutex::new(HashMap::new()),
        }
    }

    /// Global emotes overlaid with the channel's own.
    async fn emotes(&self, room_id: Option<&str>) -> EmoteMap {
        let mut merged = self.set("").await.as_deref().cloned().unwrap_or_default();
        if let Some(room_id) = room_id
            && let Some(channel) = self.set(room_id).await
        {
            merged.extend(channel.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        merged
    }

    /// `None` when 7TV couldn't be reached (logged, not cached).
    async fn set(&self, room_id: &str) -> Option<Arc<EmoteMap>> {
        if let Some(hit) = cached(&self.sets, room_id) {
            return Some(hit);
        }
        let result = if room_id.is_empty() {
            self.get::<EmoteSet>("emote-sets/global").await.map(|s| s.map(emote_map))
        } else {
            self.get::<TwitchConnection>(&format!("users/twitch/{room_id}"))
                .await
                .map(|c| c.and_then(|c| c.emote_set).map(emote_map))
        };
        match result {
            Ok(map) => {
                let map = Arc::new(map.unwrap_or_default());
                self.sets
                    .lock()
                    .unwrap()
                    .insert(room_id.to_string(), (Instant::now(), Arc::clone(&map)));
                Some(map)
            }
            Err(e) => {
                eprintln!("7tv emotes {}: {e}", if room_id.is_empty() { "global" } else { room_id });
                None
            }
        }
    }

    /// `GET {API_URL}/{path}`; `None` for 404 (no 7TV account).
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Option<T>, String> {
        let resp = self
            .http
            .get(format!("{API_URL}/{path}"))
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("request returned {}", resp.status()));
        }
        resp.json::<T>().await.map(Some).map_err(|e| format!("bad response: {e}"))
    }
}

fn cached<V: Clone>(cache: &Mutex<HashMap<String, (Instant, V)>>, key: &str) -> Option<V> {
    cache
        .lock()
        .unwrap()
        .get(key)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, v)| v.clone())
}

#[derive(Deserialize)]
pub struct EmoteParams {
    channel: Option<String>,
}

/// 7TV emotes usable in a channel's chat (global ones without `channel`).
pub async fn emotes(
    State(seventv): State<Arc<SevenTv>>,
    State(db): State<Arc<Db>>,
    Query(p): Query<EmoteParams>,
) -> ApiResult<EmoteMap> {
    let room_id = match p.channel.and_then(|c| crate::store::normalize_channel(&c)) {
        Some(channel) => db.room_id(channel).await?,
        None => None,
    };
    Ok(Json(seventv.emotes(room_id.as_deref()).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape as returned by https://7tv.io/v3/users/twitch/{id}, trimmed.
    #[test]
    fn parses_channel_emote_set() {
        let c: TwitchConnection = serde_json::from_str(
            r#"{"id":"71092938","platform":"TWITCH","emote_set":{"id":"s","name":"xQc's Emotes","emotes":[
                {"id":"a","name":"xqcL","flags":0,"data":{"id":"a","name":"xqcL","flags":0,"animated":true,
                 "host":{"url":"//cdn.7tv.app/emote/a","files":[{"name":"1x.webp","width":32,"height":32}]}}},
                {"id":"b","name":"RainTime","flags":0,"data":{"id":"b","name":"Rain","flags":256,
                 "host":{"url":"//cdn.7tv.app/emote/b","files":[]}}}]}}"#,
        )
        .unwrap();
        let map = emote_map(c.emote_set.unwrap());
        assert_eq!(map["xqcL"].url, "https://cdn.7tv.app/emote/a");
        assert!(!map["xqcL"].zero_width);
        // Keyed by the name in the set, not the emote's original name.
        assert!(map["RainTime"].zero_width);
    }

    #[test]
    fn empty_set_has_no_emotes() {
        let set: EmoteSet = serde_json::from_str(r#"{"id":"s","name":"empty","emotes":null}"#).unwrap();
        assert!(emote_map(set).is_empty());
    }
}
