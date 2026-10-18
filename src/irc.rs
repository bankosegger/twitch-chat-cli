use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const TWITCH_WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";

/// Twitch allows anonymous connections to JOIN 20 channels per 10 seconds.
const JOIN_BATCH: usize = 20;
const JOIN_INTERVAL: Duration = Duration::from_secs(11);

/// We PING the server this often; Twitch answers with a PONG, so any healthy
/// connection receives data at least once per interval, even in a quiet chat.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// A connection that has received nothing for this long is treated as dead.
/// Without this, a silently dropped TCP connection (NAT timeout, network
/// blip) would block the read forever and never reconnect.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Unique message id (`id` tag).
    pub id: Option<String>,
    /// Channel the message was sent in, lowercase, without '#'.
    pub channel: String,
    pub room_id: Option<String>,
    pub user_id: Option<String>,
    pub login: String,
    pub display_name: String,
    pub color: Option<(u8, u8, u8)>,
    pub badges: Vec<String>,
    /// Unprocessed `badges` tag, e.g. `moderator/1,subscriber/12`.
    pub badges_raw: String,
    pub text: String,
    /// Unix milliseconds, from `tmi-sent-ts` (falls back to `received_at`).
    pub sent_at: i64,
    /// Unix milliseconds when this client parsed the message.
    pub received_at: i64,
}

#[derive(Debug, Clone)]
pub enum ChatEvent {
    Connecting,
    Connected,
    Disconnected(String),
    Message(ChatMessage),
    SystemNotice(String),
}

struct IrcLine<'a> {
    tags: HashMap<&'a str, String>,
    prefix: Option<&'a str>,
    command: &'a str,
    params: Vec<&'a str>,
}

fn parse_line(line: &str) -> Option<IrcLine<'_>> {
    let mut rest = line;
    let mut tags = HashMap::new();

    if let Some(stripped) = rest.strip_prefix('@') {
        let (tag_str, remainder) = stripped.split_once(' ')?;
        rest = remainder.trim_start();
        for pair in tag_str.split(';') {
            let mut it = pair.splitn(2, '=');
            let key = it.next().unwrap_or("");
            let raw_val = it.next().unwrap_or("");
            tags.insert(key, unescape_tag_value(raw_val));
        }
    }

    let prefix = if let Some(stripped) = rest.strip_prefix(':') {
        let (p, remainder) = stripped.split_once(' ')?;
        rest = remainder.trim_start();
        Some(p)
    } else {
        None
    };

    let (main_part, trailing) = match rest.split_once(" :") {
        Some((m, t)) => (m, Some(t)),
        None => (rest, None),
    };

    let mut params: Vec<&str> = main_part.split(' ').filter(|s| !s.is_empty()).collect();
    let command = if params.is_empty() {
        return None;
    } else {
        params.remove(0)
    };
    if let Some(t) = trailing {
        params.push(t);
    }

    Some(IrcLine {
        tags,
        prefix,
        command,
        params,
    })
}

fn unescape_tag_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('s') => out.push(' '),
                Some(':') => out.push(';'),
                Some('\\') => out.push('\\'),
                Some('r') => out.push('\r'),
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_color(raw: &str) -> Option<(u8, u8, u8)> {
    let raw = raw.strip_prefix('#')?;
    if raw.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&raw[0..2], 16).ok()?;
    let g = u8::from_str_radix(&raw[2..4], 16).ok()?;
    let b = u8::from_str_radix(&raw[4..6], 16).ok()?;
    Some((r, g, b))
}

fn badge_labels(raw: &str) -> Vec<String> {
    raw.split(',')
        .filter_map(|entry| {
            let name = entry.split('/').next().unwrap_or("");
            match name {
                "broadcaster" => Some("BROADCASTER".to_string()),
                "moderator" => Some("MOD".to_string()),
                "vip" => Some("VIP".to_string()),
                "subscriber" | "founder" => Some("SUB".to_string()),
                "staff" | "admin" | "global_mod" => Some("STAFF".to_string()),
                "" => None,
                _ => None,
            }
        })
        .collect()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

fn build_message(parsed: &IrcLine<'_>) -> ChatMessage {
    let tag = |key: &str| parsed.tags.get(key).cloned().filter(|s| !s.is_empty());

    let received_at = now_millis();
    let channel = parsed
        .params
        .first()
        .map(|c| c.trim_start_matches('#').to_lowercase())
        .unwrap_or_default();
    let text = parsed.params.last().copied().unwrap_or("").to_string();
    let login = parsed
        .prefix
        .and_then(|p| p.split('!').next())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| "unknown".to_string());
    let display_name = tag("display-name").unwrap_or_else(|| login.clone());
    let badges_raw = tag("badges").unwrap_or_default();

    ChatMessage {
        id: tag("id"),
        channel,
        room_id: tag("room-id"),
        user_id: tag("user-id"),
        login,
        display_name,
        color: tag("color").and_then(|c| parse_color(&c)),
        badges: badge_labels(&badges_raw),
        badges_raw,
        text,
        sent_at: tag("tmi-sent-ts")
            .and_then(|ts| ts.parse().ok())
            .unwrap_or(received_at),
        received_at,
    }
}

/// Stays connected to Twitch chat, in whichever channels `channels` currently
/// holds: changes are applied to the live connection with JOIN/PART, and the
/// latest list is re-joined after every reconnect.
pub async fn run(mut channels: watch::Receiver<Vec<String>>, tx: mpsc::Sender<ChatEvent>) {
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if let Err(e) = connect_and_read(&mut channels, &tx, &mut backoff).await {
            let _ = tx.send(ChatEvent::Disconnected(e)).await;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }
}

fn normalize(channels: &[String]) -> Vec<String> {
    channels.iter().map(|c| c.trim_start_matches('#').to_lowercase()).collect()
}

/// Sends `COMMAND #a,#b,...` in batches of `JOIN_BATCH`.
async fn send_channel_command<S>(write: &mut S, command: &str, channels: &[String]) -> Result<(), String>
where
    S: SinkExt<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    for batch in channels.chunks(JOIN_BATCH) {
        let list = batch.iter().map(|c| format!("#{c}")).collect::<Vec<_>>().join(",");
        write
            .send(WsMessage::Text(format!("{command} {list}")))
            .await
            .map_err(|e| format!("{} failed: {e}", command.to_lowercase()))?;
    }
    Ok(())
}

async fn connect_and_read(
    channels_rx: &mut watch::Receiver<Vec<String>>,
    tx: &mpsc::Sender<ChatEvent>,
    backoff: &mut Duration,
) -> Result<(), String> {
    let _ = tx.send(ChatEvent::Connecting).await;

    let (ws_stream, _) = tokio_tungstenite::connect_async(TWITCH_WS_URL)
        .await
        .map_err(|e| format!("connection failed: {e}"))?;

    let (mut write, mut read) = ws_stream.split();

    let nick_suffix: u32 = rand::thread_rng().gen_range(10000..99999);
    let nick = format!("justinfan{nick_suffix}");

    write
        .send(WsMessage::Text(
            "CAP REQ :twitch.tv/tags twitch.tv/commands".into(),
        ))
        .await
        .map_err(|e| format!("handshake failed: {e}"))?;
    write
        .send(WsMessage::Text(format!("NICK {nick}")))
        .await
        .map_err(|e| format!("handshake failed: {e}"))?;
    let mut joined = normalize(&channels_rx.borrow_and_update());
    for (i, batch) in joined.chunks(JOIN_BATCH).enumerate() {
        if i > 0 {
            tokio::time::sleep(JOIN_INTERVAL).await;
        }
        send_channel_command(&mut write, "JOIN", batch).await?;
    }

    *backoff = INITIAL_BACKOFF;
    let _ = tx.send(ChatEvent::Connected).await;

    // Once the sender is gone the list can no longer change; stop polling it.
    let mut list_closed = false;

    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await;
    let mut last_received = Instant::now();

    loop {
        let next = tokio::select! {
            next = read.next() => next,
            _ = keepalive.tick() => {
                if last_received.elapsed() > KEEPALIVE_TIMEOUT {
                    return Err(format!(
                        "no data from server in {}s",
                        last_received.elapsed().as_secs()
                    ));
                }
                write
                    .send(WsMessage::Text("PING :tmi.twitch.tv".into()))
                    .await
                    .map_err(|e| format!("keepalive failed: {e}"))?;
                continue;
            }
            changed = channels_rx.changed(), if !list_closed => {
                if changed.is_err() {
                    list_closed = true;
                    continue;
                }
                let wanted = normalize(&channels_rx.borrow_and_update());
                let added: Vec<String> = wanted.iter().filter(|c| !joined.contains(c)).cloned().collect();
                let removed: Vec<String> = joined.iter().filter(|c| !wanted.contains(c)).cloned().collect();
                send_channel_command(&mut write, "PART", &removed).await?;
                send_channel_command(&mut write, "JOIN", &added).await?;
                joined = wanted;
                continue;
            }
        };
        last_received = Instant::now();

        let msg = match next {
            Some(Ok(m)) => m,
            Some(Err(e)) => return Err(format!("connection error: {e}")),
            None => return Err("connection closed by server".to_string()),
        };

        let text = match msg {
            WsMessage::Text(t) => t,
            WsMessage::Ping(payload) => {
                let _ = write.send(WsMessage::Pong(payload)).await;
                continue;
            }
            WsMessage::Close(_) => return Err("connection closed".to_string()),
            _ => continue,
        };

        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Some(parsed) = parse_line(line) else { continue };

            match parsed.command {
                "PING" => {
                    let payload = parsed.params.first().copied().unwrap_or("tmi.twitch.tv");
                    let _ = write.send(WsMessage::Text(format!("PONG :{payload}"))).await;
                }
                "PRIVMSG" => {
                    let _ = tx.send(ChatEvent::Message(build_message(&parsed))).await;
                }
                "NOTICE" => {
                    let text = parsed.params.last().copied().unwrap_or("").to_string();
                    let _ = tx.send(ChatEvent::SystemNotice(text)).await;
                }
                "RECONNECT" => {
                    return Err("server requested reconnect".to_string());
                }
                _ => {}
            }
        }
    }
}
