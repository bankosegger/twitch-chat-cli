//! Sends chat messages as a Twitch user. The account is authorized with the
//! device code flow, either by `twitch-chat login` or from the web UI, and its
//! token is saved to a file that `serve` loads to post through
//! `POST /helix/chat/messages`.

use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const REVOKE_URL: &str = "https://id.twitch.tv/oauth2/revoke";
const API_URL: &str = "https://api.twitch.tv/helix";
const SCOPES: &str = "user:write:chat";
/// Twitch rejects longer messages.
pub const MAX_MESSAGE_LEN: usize = 500;

/// What the token file holds.
#[derive(Clone, Serialize, Deserialize)]
struct Token {
    access_token: String,
    refresh_token: String,
    user_id: String,
    login: String,
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct Validation {
    user_id: String,
    login: String,
}

/// Twitch's error body, e.g. `{"status":400,"message":"authorization_pending"}`.
#[derive(Deserialize)]
struct TwitchError {
    message: String,
}

#[derive(Deserialize)]
struct Data<T> {
    data: Vec<T>,
}

#[derive(Deserialize)]
struct UserId {
    id: String,
}

#[derive(Deserialize)]
struct SendResult {
    message_id: String,
    is_sent: bool,
    drop_reason: Option<DropReason>,
}

#[derive(Deserialize)]
struct DropReason {
    message: String,
}

#[derive(Debug)]
pub enum SendError {
    /// The channel doesn't exist.
    NoSuchChannel,
    /// Twitch refused the message (banned, followers-only, AutoMod, ...).
    Rejected(String),
    /// Twitch couldn't be reached or answered unexpectedly.
    Failed(String),
}

fn client_id() -> Option<String> {
    env::var("TWITCH_CLIENT_ID").ok().filter(|v| !v.is_empty())
}

/// Confidential apps must send their secret when refreshing; public apps
/// (as registered for the device code flow) have none.
fn client_secret() -> Option<String> {
    env::var("TWITCH_CLIENT_SECRET").ok().filter(|v| !v.is_empty())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("default TLS backend is available")
}

async fn twitch_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    match resp.json::<TwitchError>().await {
        Ok(e) => e.message,
        Err(_) => format!("Twitch returned {status}"),
    }
}

async fn validate(http: &reqwest::Client, access_token: &str) -> Result<Validation, String> {
    let resp = http
        .get(VALIDATE_URL)
        .header("Authorization", format!("OAuth {access_token}"))
        .send()
        .await
        .map_err(|e| format!("token validation failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("token validation failed: {}", twitch_error(resp).await));
    }
    resp.json().await.map_err(|e| format!("bad validation response: {e}"))
}

fn save(path: &Path, token: &Token) -> Result<(), String> {
    use std::io::Write;
    let json = serde_json::to_vec_pretty(token).map_err(|e| e.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // The token posts as the user: keep it private.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .and_then(|mut f| f.write_all(&json))
        .map_err(|e| format!("can't write {}: {e}", path.display()))
}

/// Asks Twitch for a code for the user to enter at `verification_uri`.
async fn request_device_code(http: &reqwest::Client, client_id: &str) -> Result<DeviceCode, String> {
    let resp = http
        .post(DEVICE_URL)
        .form(&[("client_id", client_id), ("scopes", SCOPES)])
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("device code request failed: {}", twitch_error(resp).await));
    }
    resp.json().await.map_err(|e| format!("bad device code response: {e}"))
}

/// Waits until the user has entered `code` and approved the app, and returns
/// the account's token.
async fn await_authorization(http: &reqwest::Client, client_id: &str, code: &DeviceCode) -> Result<Token, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = Duration::from_secs(code.interval.max(1));
    let tokens = loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() >= deadline {
            return Err("The code expired before it was entered.".to_string());
        }
        let resp = http
            .post(TOKEN_URL)
            .form(&[
                ("client_id", client_id),
                ("scopes", SCOPES),
                ("device_code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?;
        if resp.status().is_success() {
            break resp
                .json::<TokenResponse>()
                .await
                .map_err(|e| format!("bad token response: {e}"))?;
        }
        match twitch_error(resp).await.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += Duration::from_secs(5),
            "access_denied" => return Err("Authorization was denied on Twitch.".to_string()),
            other => return Err(format!("token request failed: {other}")),
        }
    };
    let user = validate(http, &tokens.access_token).await?;
    Ok(Token {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        user_id: user.user_id,
        login: user.login,
    })
}

/// Authorizes an account interactively and saves its token to `path`.
pub async fn login(path: &Path) -> Result<(), String> {
    let client_id = client_id().ok_or("set TWITCH_CLIENT_ID to log in")?;
    let http = http();
    let code = request_device_code(&http, &client_id).await?;
    eprintln!("Open {} and enter the code {}", code.verification_uri, code.user_code);
    eprintln!("Log in as the account that should send messages. Waiting...");
    let token = await_authorization(&http, &client_id, &code).await?;
    save(path, &token)?;
    eprintln!("Logged in as {}. Token saved to {}", token.login, path.display());
    Ok(())
}

/// Posts messages with a saved user token, renewing it when it expires.
pub struct Sender {
    http: reqwest::Client,
    client_id: String,
    client_secret: Option<String>,
    path: PathBuf,
    user_id: String,
    login: String,
    /// `None` once logged out, so an in-flight refresh can't save it again.
    token: Mutex<Option<Token>>,
    /// Channel login → broadcaster id.
    broadcaster_ids: Mutex<HashMap<String, String>>,
}

impl Sender {
    fn new(client_id: String, path: PathBuf, token: Token) -> Self {
        Self {
            http: http(),
            client_id,
            client_secret: client_secret(),
            path,
            user_id: token.user_id.clone(),
            login: token.login.clone(),
            token: Mutex::new(Some(token)),
            broadcaster_ids: Mutex::new(HashMap::new()),
        }
    }

    /// The account messages are sent as.
    pub fn login(&self) -> &str {
        &self.login
    }

    fn logged_out() -> SendError {
        SendError::Failed("Logged out.".to_string())
    }

    /// Exchanges the refresh token for a new access token, unless another
    /// request already did since `stale` was used.
    async fn refresh(&self, stale: &str) -> Result<String, SendError> {
        let mut guard = self.token.lock().await;
        let token = guard.as_mut().ok_or_else(Self::logged_out)?;
        if token.access_token != stale {
            return Ok(token.access_token.clone());
        }
        let mut form = vec![
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", token.refresh_token.as_str()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret));
        }
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .map_err(|e| SendError::Failed(format!("token refresh failed: {e}")))?;
        if !resp.status().is_success() {
            let reason = twitch_error(resp).await;
            eprintln!("token refresh failed: {reason}");
            return Err(SendError::Failed(format!(
                "The login for {} expired or was revoked ({reason}). Log in again.",
                self.login
            )));
        }
        let fresh: TokenResponse = resp
            .json()
            .await
            .map_err(|e| SendError::Failed(format!("bad token response: {e}")))?;
        token.access_token = fresh.access_token;
        token.refresh_token = fresh.refresh_token;
        // Refresh tokens can be single use, so the new one must survive a restart.
        if let Err(e) = save(&self.path, token) {
            eprintln!("{e}");
        }
        Ok(token.access_token.clone())
    }

    /// Sends a request built by `build` with the access token, renewing the
    /// token and retrying once if it has expired.
    async fn request(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response, SendError> {
        let mut access_token = match &*self.token.lock().await {
            Some(token) => token.access_token.clone(),
            None => return Err(Self::logged_out()),
        };
        let mut refreshed = false;
        loop {
            let resp = build()
                .header("Client-Id", &self.client_id)
                .bearer_auth(&access_token)
                .send()
                .await
                .map_err(|e| SendError::Failed(format!("request failed: {e}")))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                access_token = self.refresh(&access_token).await?;
                refreshed = true;
                continue;
            }
            return Ok(resp);
        }
    }

    async fn broadcaster_id(&self, channel: &str) -> Result<String, SendError> {
        if let Some(id) = self.broadcaster_ids.lock().await.get(channel) {
            return Ok(id.clone());
        }
        let resp = self
            .request(|| self.http.get(format!("{API_URL}/users")).query(&[("login", channel)]))
            .await?;
        if !resp.status().is_success() {
            return Err(SendError::Failed(format!("user lookup failed: {}", twitch_error(resp).await)));
        }
        let users: Data<UserId> = resp
            .json()
            .await
            .map_err(|e| SendError::Failed(format!("bad users response: {e}")))?;
        let id = users.data.into_iter().next().ok_or(SendError::NoSuchChannel)?.id;
        self.broadcaster_ids.lock().await.insert(channel.to_string(), id.clone());
        Ok(id)
    }

    /// Posts `message` in `channel` (a normalized login), optionally as a
    /// reply to the message with Twitch id `reply_to`. Returns the new
    /// message's id.
    pub async fn send(&self, channel: &str, message: &str, reply_to: Option<&str>) -> Result<String, SendError> {
        let broadcaster_id = self.broadcaster_id(channel).await?;
        let mut body = serde_json::json!({
            "broadcaster_id": broadcaster_id,
            "sender_id": self.user_id,
            "message": message,
        });
        if let Some(reply_to) = reply_to {
            body["reply_parent_message_id"] = reply_to.into();
        }
        let resp = self
            .request(|| self.http.post(format!("{API_URL}/chat/messages")).json(&body))
            .await?;
        let status = resp.status();
        if status.is_client_error() {
            return Err(SendError::Rejected(twitch_error(resp).await));
        }
        if !status.is_success() {
            return Err(SendError::Failed(format!("chat/messages returned {status}")));
        }
        let result = resp
            .json::<Data<SendResult>>()
            .await
            .map_err(|e| SendError::Failed(format!("bad chat/messages response: {e}")))?
            .data
            .into_iter()
            .next()
            .ok_or_else(|| SendError::Failed("empty chat/messages response".to_string()))?;
        if !result.is_sent {
            let reason = result.drop_reason.map(|r| r.message);
            return Err(SendError::Rejected(reason.unwrap_or_else(|| "Twitch dropped the message.".to_string())));
        }
        Ok(result.message_id)
    }

    /// Forgets the token and revokes it at Twitch (best effort).
    async fn logout(&self) {
        let Some(token) = self.token.lock().await.take() else {
            return;
        };
        let result = self
            .http
            .post(REVOKE_URL)
            .form(&[("client_id", self.client_id.as_str()), ("token", token.access_token.as_str())])
            .send()
            .await;
        if let Err(e) = result {
            eprintln!("token revocation failed: {e}");
        }
    }
}

/// The account `serve` sends as. It can be logged in and out while the
/// server runs.
pub struct Account {
    /// `None` when `TWITCH_CLIENT_ID` isn't set, which disables sending.
    client_id: Option<String>,
    http: reqwest::Client,
    path: PathBuf,
    state: std::sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    sender: Option<Arc<Sender>>,
    pending: Option<PendingLogin>,
    /// Why the last login attempt failed.
    error: Option<String>,
    /// Identifies the latest login attempt, so a superseded one can't
    /// overwrite its result.
    attempt: u64,
}

struct PendingLogin {
    user_code: String,
    verification_uri: String,
    expires_at: Instant,
    task: JoinHandle<()>,
}

#[derive(Serialize)]
pub struct Status {
    /// False when `TWITCH_CLIENT_ID` isn't set.
    enabled: bool,
    /// The account messages are sent as.
    login: Option<String>,
    /// A login waiting for the user to enter the code on Twitch.
    pending: Option<PendingStatus>,
    error: Option<String>,
}

#[derive(Serialize)]
pub struct PendingStatus {
    user_code: String,
    verification_uri: String,
    /// Seconds until the code expires.
    expires_in: u64,
}

impl Account {
    /// The account whose token is saved at `path`, if any.
    pub fn load(path: &Path) -> Result<Arc<Self>, String> {
        let client_id = client_id();
        let token = match std::fs::read(path) {
            Ok(json) => Some(
                serde_json::from_slice::<Token>(&json).map_err(|e| format!("bad token file {}: {e}", path.display()))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("can't read {}: {e}", path.display())),
        };
        let sender = match (&client_id, token) {
            (Some(id), Some(token)) => Some(Arc::new(Sender::new(id.clone(), path.to_path_buf(), token))),
            _ => None,
        };
        Ok(Arc::new(Self {
            client_id,
            http: http(),
            path: path.to_path_buf(),
            state: std::sync::Mutex::new(State {
                sender,
                ..State::default()
            }),
        }))
    }

    pub fn enabled(&self) -> bool {
        self.client_id.is_some()
    }

    pub fn sender(&self) -> Option<Arc<Sender>> {
        self.state.lock().unwrap().sender.clone()
    }

    pub fn status(&self) -> Status {
        let state = self.state.lock().unwrap();
        Status {
            enabled: self.enabled(),
            login: state.sender.as_ref().map(|s| s.login.clone()),
            pending: state.pending.as_ref().map(|p| PendingStatus {
                user_code: p.user_code.clone(),
                verification_uri: p.verification_uri.clone(),
                expires_in: p.expires_at.saturating_duration_since(Instant::now()).as_secs(),
            }),
            error: state.error.clone(),
        }
    }

    /// Starts a device code login, replacing any pending one. The account
    /// switches over once the user has entered the code on Twitch.
    pub async fn start_login(self: &Arc<Self>) -> Result<Status, String> {
        let client_id = self.client_id.clone().ok_or("Set TWITCH_CLIENT_ID to log in.")?;
        let code = request_device_code(&self.http, &client_id).await?;

        let mut state = self.state.lock().unwrap();
        if let Some(old) = state.pending.take() {
            old.task.abort();
        }
        state.attempt += 1;
        state.error = None;
        let attempt = state.attempt;
        let account = Arc::clone(self);
        let user_code = code.user_code.clone();
        let verification_uri = code.verification_uri.clone();
        let expires_at = Instant::now() + Duration::from_secs(code.expires_in);
        let task = tokio::spawn(async move {
            let result = await_authorization(&account.http, &client_id, &code)
                .await
                .and_then(|token| save(&account.path, &token).map(|()| token));
            account.finish_login(attempt, client_id, result).await;
        });
        state.pending = Some(PendingLogin {
            user_code,
            verification_uri,
            expires_at,
            task,
        });
        drop(state);
        Ok(self.status())
    }

    async fn finish_login(&self, attempt: u64, client_id: String, result: Result<Token, String>) {
        let replaced = {
            let mut state = self.state.lock().unwrap();
            if state.attempt != attempt {
                return;
            }
            state.pending = None;
            match result {
                Ok(token) => {
                    eprintln!("logged in as {}; sending chat messages", token.login);
                    let sender = Sender::new(client_id, self.path.clone(), token);
                    state.sender.replace(Arc::new(sender))
                }
                Err(e) => {
                    eprintln!("login failed: {e}");
                    state.error = Some(e);
                    None
                }
            }
        };
        if let Some(old) = replaced {
            old.logout().await;
        }
    }

    /// Cancels a pending login and logs the account out.
    pub async fn logout(&self) -> Result<Status, String> {
        let sender = {
            let mut state = self.state.lock().unwrap();
            state.attempt += 1;
            state.error = None;
            if let Some(pending) = state.pending.take() {
                pending.task.abort();
            }
            state.sender.take()
        };
        if let Some(sender) = sender {
            // Forget the token first, so a refresh in flight can't write it back.
            sender.logout().await;
            match std::fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("can't delete {}: {e}", self.path.display())),
            }
            eprintln!("logged out {}", sender.login);
        }
        Ok(self.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shapes as documented at https://dev.twitch.tv/docs/api/reference/#send-chat-message.
    #[test]
    fn parses_send_responses() {
        let sent: Data<SendResult> =
            serde_json::from_str(r#"{"data":[{"message_id":"abc-123","is_sent":true,"drop_reason":null}]}"#).unwrap();
        assert!(sent.data[0].is_sent);
        assert_eq!(sent.data[0].message_id, "abc-123");

        let dropped: Data<SendResult> = serde_json::from_str(
            r#"{"data":[{"message_id":"","is_sent":false,
                "drop_reason":{"code":"msg_duplicate","message":"Your message is identical to the one you sent within the last 30 seconds."}}]}"#,
        )
        .unwrap();
        assert!(!dropped.data[0].is_sent);
        assert!(dropped.data[0].drop_reason.as_ref().unwrap().message.starts_with("Your message"));
    }
}
