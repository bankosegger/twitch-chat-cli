use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

const TWITCH_WS_URL: &str = "wss://irc-ws.chat.twitch.tv:443";

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub display_name: String,
    pub color: Option<(u8, u8, u8)>,
    pub badges: Vec<String>,
    pub text: String,
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

pub async fn run(channel: String, tx: mpsc::UnboundedSender<ChatEvent>) {
    let mut backoff = Duration::from_secs(1);
    let channel_lower = channel.to_lowercase();

    loop {
        if let Err(e) = connect_and_read(&channel_lower, &tx).await {
            let _ = tx.send(ChatEvent::Disconnected(e));
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }
}

async fn connect_and_read(
    channel: &str,
    tx: &mpsc::UnboundedSender<ChatEvent>,
) -> Result<(), String> {
    let _ = tx.send(ChatEvent::Connecting);

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
    write
        .send(WsMessage::Text(format!("JOIN #{channel}")))
        .await
        .map_err(|e| format!("join failed: {e}"))?;

    let _ = tx.send(ChatEvent::Connected);

    loop {
        let msg = match read.next().await {
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
                    let text = parsed.params.last().copied().unwrap_or("").to_string();
                    let display_name = parsed
                        .tags
                        .get("display-name")
                        .cloned()
                        .filter(|s| !s.is_empty())
                        .or_else(|| parsed.prefix.and_then(|p| p.split('!').next().map(String::from)))
                        .unwrap_or_else(|| "unknown".to_string());
                    let color = parsed.tags.get("color").and_then(|c| parse_color(c));
                    let badges = parsed
                        .tags
                        .get("badges")
                        .map(|b| badge_labels(b))
                        .unwrap_or_default();

                    let _ = tx.send(ChatEvent::Message(ChatMessage {
                        display_name,
                        color,
                        badges,
                        text,
                    }));
                }
                "NOTICE" => {
                    let text = parsed.params.last().copied().unwrap_or("").to_string();
                    let _ = tx.send(ChatEvent::SystemNotice(text));
                }
                "RECONNECT" => {
                    return Err("server requested reconnect".to_string());
                }
                _ => {}
            }
        }
    }
}
