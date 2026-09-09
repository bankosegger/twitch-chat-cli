use std::time::Duration;

use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::mpsc;

const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const STREAMS_URL: &str = "https://api.twitch.tv/helix/streams";
const POLL_INTERVAL: Duration = Duration::from_secs(60);
const RETRY_INTERVAL: Duration = Duration::from_secs(20);

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

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct StreamsResponse {
    data: Vec<StreamData>,
}

#[derive(Deserialize)]
struct StreamData {
    title: String,
    game_name: String,
    viewer_count: u64,
    started_at: String,
}

enum FetchError {
    Unauthorized,
    Other(String),
}

pub async fn run(channel: String, client_id: String, client_secret: String, tx: mpsc::UnboundedSender<StreamStatus>) {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(10)).build() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(StreamStatus::Error(format!("http client init failed: {e}")));
            return;
        }
    };

    let mut token: Option<String> = None;

    loop {
        if token.is_none() {
            match fetch_token(&client, &client_id, &client_secret).await {
                Ok(t) => token = Some(t),
                Err(e) => {
                    let _ = tx.send(StreamStatus::Error(format!("auth failed: {e}")));
                    tokio::time::sleep(RETRY_INTERVAL).await;
                    continue;
                }
            }
        }

        let tok = token.clone().expect("token set above");
        match fetch_stream(&client, &client_id, &tok, &channel).await {
            Ok(Some(info)) => {
                let _ = tx.send(StreamStatus::Live(info));
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Ok(None) => {
                let _ = tx.send(StreamStatus::Offline);
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(FetchError::Unauthorized) => {
                token = None;
            }
            Err(FetchError::Other(e)) => {
                let _ = tx.send(StreamStatus::Error(e));
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
        }
    }
}

async fn fetch_token(client: &reqwest::Client, client_id: &str, client_secret: &str) -> Result<String, String> {
    let params = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("grant_type", "client_credentials"),
    ];

    let resp = client
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.status().is_success() {
        return Err(format!("token request returned {}", resp.status()));
    }

    resp.json::<TokenResponse>()
        .await
        .map(|t| t.access_token)
        .map_err(|e| format!("bad token response: {e}"))
}

async fn fetch_stream(
    client: &reqwest::Client,
    client_id: &str,
    token: &str,
    channel: &str,
) -> Result<Option<StreamInfo>, FetchError> {
    let resp = client
        .get(STREAMS_URL)
        .query(&[("user_login", channel)])
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| FetchError::Other(e.to_string()))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(FetchError::Unauthorized);
    }
    if !resp.status().is_success() {
        return Err(FetchError::Other(format!("streams request returned {}", resp.status())));
    }

    let parsed: StreamsResponse = resp
        .json()
        .await
        .map_err(|e| FetchError::Other(format!("bad streams response: {e}")))?;

    let Some(data) = parsed.data.into_iter().next() else {
        return Ok(None);
    };

    let started_at = OffsetDateTime::parse(&data.started_at, &Rfc3339)
        .map_err(|e| FetchError::Other(format!("bad started_at: {e}")))?;

    Ok(Some(StreamInfo {
        title: data.title,
        game_name: data.game_name,
        viewer_count: data.viewer_count,
        started_at,
    }))
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
