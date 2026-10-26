//! The web server: starts up, serves the embedded UI assets, and routes
//! `/api/*` to the handlers in the submodules below.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{FromRef, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use base64::Engine;

use crate::chat::Account;
use crate::helix::Helix;
use crate::store::Store;

mod account;
mod channels;
mod db;
mod filter;
mod messages;
mod seventv;
mod stats;
mod twitch;
mod users;

use db::Db;
use seventv::SevenTv;
use twitch::Twitch;

/// Handlers take the part they need: `State<Arc<Db>>`, `State<Arc<Twitch>>`,
/// `State<Arc<SevenTv>>` or `State<Arc<Account>>`.
#[derive(Clone)]
struct AppState {
    db: Arc<Db>,
    twitch: Arc<Twitch>,
    seventv: Arc<SevenTv>,
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

impl FromRef<AppState> for Arc<SevenTv> {
    fn from_ref(state: &AppState) -> Self {
        Arc::clone(&state.seventv)
    }
}

const INDEX_HTML: &str = include_str!("web/index.html");
const APP_JS: &str = include_str!("web/app.js");
const STYLE_CSS: &str = include_str!("web/style.css");
const FONT_WOFF2: &[u8] = include_bytes!("fonts/PlusJakartaSans-Variable.woff2");

/// Serves the web UI and its JSON API until interrupted. When `password` is
/// set, every request requires HTTP basic auth with that password (any
/// username).
pub async fn run(db_path: &Path, bind: SocketAddr, password: Option<String>, token_path: &Path) -> io::Result<()> {
    // Opening read-write once creates the schema (and the search index) if the
    // database is new or was written by an older recorder.
    Store::open(db_path).map_err(io::Error::other)?;

    let db = Arc::new(Db::new(db_path.to_path_buf()));
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
        seventv: Arc::new(SevenTv::new()),
        account,
    };

    let api = Router::new()
        .route("/channels", get(channels::channels))
        .route("/channels/{channel}", delete(channels::delete_channel))
        .route("/channels/{channel}/messages", post(account::send_message))
        .route("/chat-account", get(account::chat_account).delete(account::logout))
        .route("/chat-account/login", post(account::start_login))
        .route("/storage", get(channels::storage))
        .route("/download", get(channels::download))
        .route("/stats", get(stats::stats))
        .route("/activity", get(stats::activity))
        .route("/top-chatters", get(stats::top_chatters))
        .route("/top-messages", get(stats::top_messages))
        .route("/roles", get(stats::roles))
        .route("/messages", get(messages::messages))
        .route("/messages/{id}/context", get(messages::message_context))
        .route("/events", get(messages::events))
        .route("/moderation", get(messages::moderation))
        .route("/viewers", get(stats::viewers))
        .route("/users", get(users::search_users))
        .route("/users/{user_id}", get(users::user))
        .route("/watched", get(channels::watched).post(channels::add_watched))
        .route("/watched/{channel}", delete(channels::remove_watched))
        .route("/profile/{login}", get(twitch::profile))
        .route("/badges", get(twitch::badges))
        .route("/7tv-emotes", get(seventv::emotes))
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
