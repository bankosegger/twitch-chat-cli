//! Twitch Helix API client, authenticated with an app access token
//! (`TWITCH_CLIENT_ID` / `TWITCH_CLIENT_SECRET`). Only public data is used.

use std::env;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{mpsc, Mutex};

const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const API_URL: &str = "https://api.twitch.tv/helix";
/// Helix accepts at most this many ids or logins per request.
const MAX_PER_REQUEST: usize = 100;
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_INTERVAL: Duration = Duration::from_secs(20);

pub struct Helix {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    token: Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub title: String,
    pub game_name: String,
    pub viewer_count: u64,
    pub started_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub enum StreamStatus {
    Live(StreamInfo),
    Offline,
    Error(String),
}

/// A live stream, from `GET /streams`.
#[derive(Debug, Clone, Deserialize)]
pub struct Stream {
    pub id: String,
    pub user_login: String,
    pub title: String,
    pub game_name: String,
    pub viewer_count: u64,
    /// RFC 3339.
    pub started_at: String,
}

impl Stream {
    pub fn started_at(&self) -> Option<OffsetDateTime> {
        OffsetDateTime::parse(&self.started_at, &Rfc3339).ok()
    }
}

/// A user or channel, from `GET /users`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub id: String,
    pub login: String,
    pub display_name: String,
    /// `partner`, `affiliate` or empty.
    pub broadcaster_type: String,
    pub description: String,
    pub profile_image_url: String,
    /// RFC 3339.
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BadgeSet {
    pub set_id: String,
    pub versions: Vec<BadgeVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BadgeVersion {
    pub id: String,
    pub image_url_2x: String,
    pub title: String,
}

#[derive(Deserialize)]
struct Data<T> {
    data: Vec<T>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

impl Helix {
    /// A client from `TWITCH_CLIENT_ID` / `TWITCH_CLIENT_SECRET`, or `None` if
    /// either is unset.
    pub fn from_env() -> Option<Self> {
        let id = env::var("TWITCH_CLIENT_ID").ok().filter(|v| !v.is_empty())?;
        let secret = env::var("TWITCH_CLIENT_SECRET").ok().filter(|v| !v.is_empty())?;
        Some(Self::new(id, secret))
    }

    pub fn new(client_id: String, client_secret: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("default TLS backend is available");
        Self {
            http,
            client_id,
            client_secret,
            token: Mutex::new(None),
        }
    }

    async fn token(&self, refresh: bool) -> Result<String, String> {
        let mut token = self.token.lock().await;
        if refresh {
            *token = None;
        }
        if let Some(t) = &*token {
            return Ok(t.clone());
        }
        let params = [
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("grant_type", "client_credentials"),
        ];
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&params)
            .send()
            .await
            .map_err(|e| format!("auth failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("auth failed: token request returned {}", resp.status()));
        }
        let t = resp
            .json::<TokenResponse>()
            .await
            .map_err(|e| format!("auth failed: bad token response: {e}"))?
            .access_token;
        *token = Some(t.clone());
        Ok(t)
    }

    /// `GET /helix/{path}`, returning its `data` array. An expired token is
    /// renewed and the request retried once.
    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<T>, String> {
        let mut refresh = false;
        loop {
            let token = self.token(refresh).await?;
            let resp = self
                .http
                .get(format!("{API_URL}/{path}"))
                .query(query)
                .header("Client-Id", &self.client_id)
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| format!("{path} request failed: {e}"))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && !refresh {
                refresh = true;
                continue;
            }
            if !resp.status().is_success() {
                return Err(format!("{path} request returned {}", resp.status()));
            }
            return resp
                .json::<Data<T>>()
                .await
                .map(|d| d.data)
                .map_err(|e| format!("bad {path} response: {e}"));
        }
    }

    /// `GET /helix/{path}` with `key=value` repeated for each value, split
    /// into as many requests as needed.
    async fn get_many<T: DeserializeOwned>(&self, path: &str, key: &str, values: &[String]) -> Result<Vec<T>, String> {
        let mut out = Vec::new();
        for chunk in values.chunks(MAX_PER_REQUEST) {
            let mut query: Vec<(&str, &str)> = chunk.iter().map(|v| (key, v.as_str())).collect();
            query.push(("first", "100"));
            out.extend(self.get::<T>(path, &query).await?);
        }
        Ok(out)
    }

    /// The streams currently live among `logins`; offline channels are absent.
    pub async fn streams(&self, logins: &[String]) -> Result<Vec<Stream>, String> {
        self.get_many("streams", "user_login", logins).await
    }

    pub async fn users_by_login(&self, logins: &[String]) -> Result<Vec<User>, String> {
        self.get_many("users", "login", logins).await
    }

    pub async fn users_by_id(&self, ids: &[String]) -> Result<Vec<User>, String> {
        self.get_many("users", "id", ids).await
    }

    pub async fn global_badges(&self) -> Result<Vec<BadgeSet>, String> {
        self.get("chat/badges/global", &[]).await
    }

    /// The channel's own sub and bits badges, which override the global ones.
    pub async fn channel_badges(&self, broadcaster_id: &str) -> Result<Vec<BadgeSet>, String> {
        self.get("chat/badges", &[("broadcaster_id", broadcaster_id)]).await
    }
}

/// Polls one channel's stream status for the terminal UI.
pub async fn run(channel: String, helix: Helix, tx: mpsc::UnboundedSender<StreamStatus>) {
    let channel = vec![channel];
    loop {
        let (status, wait) = match helix.streams(&channel).await {
            Ok(streams) => match streams.into_iter().next() {
                Some(s) => match s.started_at() {
                    Some(started_at) => (
                        StreamStatus::Live(StreamInfo {
                            title: s.title,
                            game_name: s.game_name,
                            viewer_count: s.viewer_count,
                            started_at,
                        }),
                        POLL_INTERVAL,
                    ),
                    None => (StreamStatus::Error("bad started_at".into()), RETRY_INTERVAL),
                },
                None => (StreamStatus::Offline, POLL_INTERVAL),
            },
            Err(e) => (StreamStatus::Error(e), RETRY_INTERVAL),
        };
        if tx.send(status).is_err() {
            return;
        }
        tokio::time::sleep(wait).await;
    }
}

pub fn format_uptime(started_at: OffsetDateTime) -> String {
    let elapsed = OffsetDateTime::now_utc() - started_at;
    let total_seconds = elapsed.whole_seconds().max(0);
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shapes as documented at https://dev.twitch.tv/docs/api/reference.
    #[test]
    fn parses_api_responses() {
        let streams: Data<Stream> = serde_json::from_str(
            r#"{"data":[{"id":"1","user_id":"2","user_login":"somechannel","user_name":"SomeChannel",
                "game_id":"509658","game_name":"Just Chatting","type":"live","title":"hi",
                "viewer_count":1234,"started_at":"2026-10-21T10:00:00Z","language":"en",
                "thumbnail_url":"","tags":[],"is_mature":false}],"pagination":{}}"#,
        )
        .unwrap();
        let s = &streams.data[0];
        assert_eq!((s.user_login.as_str(), s.viewer_count), ("somechannel", 1234));
        assert_eq!(s.started_at().unwrap().unix_timestamp(), 1_792_576_800);

        let users: Data<User> = serde_json::from_str(
            r#"{"data":[{"id":"2","login":"somechannel","display_name":"SomeChannel","type":"",
                "broadcaster_type":"partner","description":"bio","profile_image_url":"https://x/p.png",
                "offline_image_url":"","view_count":0,"created_at":"2016-12-14T20:32:28Z"}]}"#,
        )
        .unwrap();
        assert_eq!(users.data[0].broadcaster_type, "partner");

        let badges: Data<BadgeSet> = serde_json::from_str(
            r#"{"data":[{"set_id":"subscriber","versions":[{"id":"12","image_url_1x":"a","image_url_2x":"b",
                "image_url_4x":"c","title":"1-Year Subscriber","description":"","click_action":null,"click_url":null}]}]}"#,
        )
        .unwrap();
        assert_eq!(badges.data[0].versions[0].image_url_2x, "b");
    }
}
