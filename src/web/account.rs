//! The Twitch account `serve` sends messages as: its status, logging in and
//! out, and posting a message.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::db::{ApiError, ApiResult};
use crate::chat::{self, Account, SendError};
use crate::store;

/// The account messages are sent as, and any login in progress.
pub(super) async fn chat_account(State(account): State<Arc<Account>>) -> Json<chat::Status> {
    Json(account.status())
}

/// No fields: the JSON body only makes browsers preflight cross-site requests,
/// which this server never approves.
#[derive(Deserialize)]
pub(super) struct StartLogin {}

/// Starts a login: the response's `pending` holds the code to enter on Twitch.
pub(super) async fn start_login(
    State(account): State<Arc<Account>>,
    Json(_): Json<StartLogin>,
) -> ApiResult<chat::Status> {
    if !account.enabled() {
        return Err(ApiError::Unavailable("Set TWITCH_CLIENT_ID to log in.".to_string()));
    }
    account.start_login().await.map(Json).map_err(ApiError::Upstream)
}

/// Cancels a pending login and logs out. DELETE isn't a "simple" method, so
/// browsers won't send it cross-site without a CORS preflight.
pub(super) async fn logout(State(account): State<Arc<Account>>) -> ApiResult<chat::Status> {
    account.logout().await.map(Json).map_err(ApiError::Internal)
}

#[derive(Deserialize)]
pub(super) struct SendMessage {
    message: String,
    /// Twitch id (`msg_id`) of the message to reply to.
    reply_to: Option<String>,
}

#[derive(Serialize)]
pub(super) struct SentMessage {
    msg_id: String,
}

/// Posts a chat message as the logged-in account. Takes a JSON body, so
/// browsers won't send it cross-site without a CORS preflight.
pub(super) async fn send_message(
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
