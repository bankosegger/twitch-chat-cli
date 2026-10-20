//! Endpoints backed by the Twitch API rather than the database: profile
//! pictures and badge images. Answers are cached in memory, and everything
//! degrades to empty answers when no API credentials are configured.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{ApiResult, Db};
use crate::helix::{BadgeSet, Helix, User};

/// Profiles and badges change rarely.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

pub struct Twitch {
    helix: Option<Helix>,
    profiles: Mutex<HashMap<String, (Instant, Option<User>)>>,
    /// Keyed by broadcaster id; `""` holds the global badges.
    badges: Mutex<HashMap<String, (Instant, Arc<BadgeMap>)>>,
}

/// `"set/version"` (as in the `badges` tag, e.g. `subscriber/12`) → image.
type BadgeMap = HashMap<String, Badge>;

#[derive(Clone, Serialize)]
pub struct Badge {
    url: String,
    title: String,
}

fn cached<V: Clone>(cache: &Mutex<HashMap<String, (Instant, V)>>, key: &str) -> Option<V> {
    cache
        .lock()
        .unwrap()
        .get(key)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, v)| v.clone())
}

fn store<V>(cache: &Mutex<HashMap<String, (Instant, V)>>, key: String, value: V) {
    cache.lock().unwrap().insert(key, (Instant::now(), value));
}

impl Twitch {
    pub fn new(helix: Option<Helix>) -> Self {
        Self {
            helix,
            profiles: Mutex::new(HashMap::new()),
            badges: Mutex::new(HashMap::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.helix.is_some()
    }

    /// `None` when the API is disabled, the user doesn't exist, or Twitch
    /// couldn't be reached (logged, not cached).
    async fn profile(&self, login: &str) -> Option<User> {
        let helix = self.helix.as_ref()?;
        if let Some(hit) = cached(&self.profiles, login) {
            return hit;
        }
        match helix.users_by_login(&[login.to_string()]).await {
            Ok(users) => {
                let user = users.into_iter().next();
                store(&self.profiles, login.to_string(), user.clone());
                user
            }
            Err(e) => {
                eprintln!("profile {login}: {e}");
                None
            }
        }
    }

    /// Global badges overlaid with the channel's own (sub and bits badges).
    async fn badges(&self, room_id: Option<&str>) -> BadgeMap {
        let mut merged = self.badge_set("").await.as_deref().cloned().unwrap_or_default();
        if let Some(room_id) = room_id
            && let Some(channel) = self.badge_set(room_id).await
        {
            merged.extend(channel.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        merged
    }

    async fn badge_set(&self, room_id: &str) -> Option<Arc<BadgeMap>> {
        let helix = self.helix.as_ref()?;
        if let Some(hit) = cached(&self.badges, room_id) {
            return Some(hit);
        }
        let result = if room_id.is_empty() {
            helix.global_badges().await
        } else {
            helix.channel_badges(room_id).await
        };
        match result {
            Ok(sets) => {
                let map = Arc::new(badge_map(sets));
                store(&self.badges, room_id.to_string(), Arc::clone(&map));
                Some(map)
            }
            Err(e) => {
                eprintln!("badges {}: {e}", if room_id.is_empty() { "global" } else { room_id });
                None
            }
        }
    }
}

fn badge_map(sets: Vec<BadgeSet>) -> BadgeMap {
    sets.into_iter()
        .flat_map(|set| {
            set.versions.into_iter().map(move |v| {
                (
                    format!("{}/{}", set.set_id, v.id),
                    Badge {
                        url: v.image_url_2x,
                        title: v.title,
                    },
                )
            })
        })
        .collect()
}

/// The channel's Twitch profile (picture, description, ...), or `null`.
pub async fn profile(State(twitch): State<Arc<Twitch>>, UrlPath(login): UrlPath<String>) -> Json<Option<User>> {
    match crate::store::normalize_channel(&login) {
        Some(login) => Json(twitch.profile(&login).await),
        None => Json(None),
    }
}

#[derive(Deserialize)]
pub struct BadgeParams {
    channel: Option<String>,
}

/// Badge images for a channel's messages; `{}` without API credentials.
pub async fn badges(
    State(twitch): State<Arc<Twitch>>,
    State(db): State<Arc<Db>>,
    Query(p): Query<BadgeParams>,
) -> ApiResult<BadgeMap> {
    if !twitch.enabled() {
        return Ok(Json(BadgeMap::new()));
    }
    // Chat messages carry the channel's id, so no extra API call is needed.
    let room_id = match p.channel.and_then(|c| crate::store::normalize_channel(&c)) {
        Some(channel) => {
            db.query(move |conn| {
                use rusqlite::OptionalExtension;
                Ok(conn
                    .query_row(
                        "SELECT room_id FROM messages WHERE channel = ? AND room_id IS NOT NULL ORDER BY sent_at DESC LIMIT 1",
                        [channel],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .await?
        }
        None => None,
    };
    Ok(Json(twitch.badges(room_id.as_deref()).await))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helix::BadgeVersion;

    #[test]
    fn badge_keys_match_the_badges_tag() {
        let map = badge_map(vec![BadgeSet {
            set_id: "subscriber".into(),
            versions: vec![BadgeVersion {
                id: "12".into(),
                image_url_2x: "https://x/12.png".into(),
                title: "1-Year Subscriber".into(),
            }],
        }]);
        assert_eq!(map["subscriber/12"].url, "https://x/12.png");
    }
}
