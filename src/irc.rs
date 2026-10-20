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
    /// Bits cheered with this message.
    pub bits: Option<i64>,
    /// Unprocessed `emotes` tag: `emote_id:start-end,start-end/...`, with
    /// inclusive positions counted in characters (code points) of `text`.
    pub emotes: String,
    /// Unix milliseconds, from `tmi-sent-ts` (falls back to `received_at`).
    pub sent_at: i64,
    /// Unix milliseconds when this client parsed the message.
    pub received_at: i64,
}

impl ChatMessage {
    /// A cheer, as an event alongside subs and raids.
    pub fn cheer(&self) -> Option<UserNotice> {
        let bits = self.bits?;
        Some(UserNotice {
            id: self.id.clone(),
            channel: self.channel.clone(),
            room_id: self.room_id.clone(),
            user_id: self.user_id.clone(),
            login: self.login.clone(),
            display_name: self.display_name.clone(),
            color: self.color,
            kind: "cheer".to_string(),
            tier: None,
            months: None,
            count: Some(bits),
            recipient_login: None,
            recipient_display_name: None,
            bulk_gift: false,
            system_text: format!("{} cheered {bits} bit{}!", self.display_name, if bits == 1 { "" } else { "s" }),
            text: Some(self.text.clone()),
            sent_at: self.sent_at,
            received_at: self.received_at,
        })
    }
}

/// A moderator removing chat: `CLEARCHAT` (ban, timeout, /clear) or
/// `CLEARMSG` (a single deleted message).
#[derive(Debug, Clone)]
pub struct ModAction {
    pub channel: String,
    pub room_id: Option<String>,
    pub kind: ModKind,
    /// The banned or timed-out user, or the author of a deleted message.
    pub target_user_id: Option<String>,
    pub target_login: Option<String>,
    /// For a deleted message: its id and text.
    pub target_msg_id: Option<String>,
    pub text: Option<String>,
    pub sent_at: i64,
    pub received_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModKind {
    Ban,
    /// Seconds.
    Timeout(i64),
    /// `/clear`: the whole chat was cleared.
    Clear,
    Delete,
}

impl ModKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ModKind::Ban => "ban",
            ModKind::Timeout(_) => "timeout",
            ModKind::Clear => "clear",
            ModKind::Delete => "delete",
        }
    }
}

/// A `USERNOTICE`: a sub, resub, gifted sub, raid, announcement, etc.
#[derive(Debug, Clone)]
pub struct UserNotice {
    pub id: Option<String>,
    pub channel: String,
    pub room_id: Option<String>,
    /// The user who triggered the event (the gifter, for gifts; the raider, for raids).
    pub user_id: Option<String>,
    pub login: String,
    pub display_name: String,
    pub color: Option<(u8, u8, u8)>,
    /// The `msg-id` tag: `sub`, `resub`, `subgift`, `submysterygift`, `raid`, ...
    pub kind: String,
    /// `Prime`, `1000`, `2000` or `3000`, for sub events.
    pub tier: Option<String>,
    /// Cumulative months subscribed (the recipient's, for a gift).
    pub months: Option<i64>,
    /// Subs in a gift bomb (`submysterygift`), or viewers in a raid.
    pub count: Option<i64>,
    pub recipient_login: Option<String>,
    pub recipient_display_name: Option<String>,
    /// A single gift that is part of a gift bomb, which has its own
    /// `submysterygift` notice announcing the total.
    pub bulk_gift: bool,
    /// Twitch's ready-made description, e.g. "X subscribed at Tier 1.".
    pub system_text: String,
    /// The message the user attached (resub message, announcement text).
    pub text: Option<String>,
    pub sent_at: i64,
    pub received_at: i64,
}

impl UserNotice {
    /// A new, renewed or gifted subscription.
    pub fn is_sub(&self) -> bool {
        matches!(self.kind.as_str(), "sub" | "resub" | "subgift")
    }
}

#[derive(Debug, Clone)]
pub enum ChatEvent {
    Connecting,
    Connected,
    Disconnected(String),
    Message(ChatMessage),
    UserNotice(UserNotice),
    Moderation(ModAction),
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
        bits: tag("bits").and_then(|b| b.parse().ok()).filter(|&b: &i64| b > 0),
        emotes: tag("emotes").unwrap_or_default(),
        sent_at: tag("tmi-sent-ts")
            .and_then(|ts| ts.parse().ok())
            .unwrap_or(received_at),
        received_at,
    }
}

fn build_mod_action(parsed: &IrcLine<'_>) -> ModAction {
    let tag = |key: &str| parsed.tags.get(key).cloned().filter(|s| !s.is_empty());
    let received_at = now_millis();
    // The second parameter is the target login (CLEARCHAT) or the deleted
    // message's text (CLEARMSG).
    let trailing = parsed.params.get(1).map(|t| t.to_string());
    let (kind, target_login, text) = if parsed.command == "CLEARMSG" {
        (ModKind::Delete, tag("login"), trailing)
    } else {
        let kind = match (&trailing, tag("ban-duration").and_then(|d| d.parse().ok())) {
            (None, _) => ModKind::Clear,
            (Some(_), Some(seconds)) => ModKind::Timeout(seconds),
            (Some(_), None) => ModKind::Ban,
        };
        (kind, trailing.map(|l| l.to_lowercase()), None)
    };
    ModAction {
        channel: parsed
            .params
            .first()
            .map(|c| c.trim_start_matches('#').to_lowercase())
            .unwrap_or_default(),
        room_id: tag("room-id"),
        kind,
        target_user_id: tag("target-user-id"),
        target_login,
        target_msg_id: tag("target-msg-id"),
        text,
        sent_at: tag("tmi-sent-ts")
            .and_then(|ts| ts.parse().ok())
            .unwrap_or(received_at),
        received_at,
    }
}

fn build_user_notice(parsed: &IrcLine<'_>) -> Option<UserNotice> {
    let tag = |key: &str| parsed.tags.get(key).cloned().filter(|s| !s.is_empty());
    let number = |key: &str| tag(key).and_then(|v| v.parse::<i64>().ok()).filter(|&n| n > 0);

    let kind = tag("msg-id")?;
    let received_at = now_millis();
    // The prefix is the server here, so the user comes from the `login` tag.
    let login = tag("login").unwrap_or_else(|| "unknown".to_string());
    let display_name = tag("display-name").unwrap_or_else(|| login.clone());
    let (months, count) = match kind.as_str() {
        "sub" | "resub" => (number("msg-param-cumulative-months"), None),
        "subgift" => (number("msg-param-months"), None),
        "submysterygift" => (None, number("msg-param-mass-gift-count")),
        "raid" => (None, number("msg-param-viewerCount")),
        _ => (None, None),
    };

    Some(UserNotice {
        id: tag("id"),
        channel: parsed
            .params
            .first()
            .map(|c| c.trim_start_matches('#').to_lowercase())
            .unwrap_or_default(),
        room_id: tag("room-id"),
        user_id: tag("user-id"),
        login,
        display_name,
        color: tag("color").and_then(|c| parse_color(&c)),
        tier: tag("msg-param-sub-plan"),
        months,
        count,
        recipient_login: tag("msg-param-recipient-user-name"),
        recipient_display_name: tag("msg-param-recipient-display-name"),
        bulk_gift: kind == "subgift" && tag("msg-param-community-gift-id").is_some(),
        system_text: tag("system-msg").unwrap_or_default(),
        // Only the channel when there is no attached message.
        text: parsed.params.get(1).map(|t| t.to_string()).filter(|t| !t.trim().is_empty()),
        kind,
        sent_at: tag("tmi-sent-ts")
            .and_then(|ts| ts.parse().ok())
            .unwrap_or(received_at),
        received_at,
    })
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
                "CLEARCHAT" | "CLEARMSG" => {
                    let _ = tx.send(ChatEvent::Moderation(build_mod_action(&parsed))).await;
                }
                "USERNOTICE" => {
                    if let Some(notice) = build_user_notice(&parsed) {
                        let _ = tx.send(ChatEvent::UserNotice(notice)).await;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(line: &str) -> UserNotice {
        build_user_notice(&parse_line(line).unwrap()).unwrap()
    }

    #[test]
    fn parses_resub() {
        let n = notice(
            concat!(
                r"@badges=subscriber/12;color=#FF0010;display-name=Someone;id=abc;login=someone;msg-id=resub;",
                r"msg-param-cumulative-months=12;msg-param-sub-plan=1000;room-id=1;",
                r"system-msg=Someone\ssubscribed\sat\sTier\s1.;tmi-sent-ts=1700000000000;user-id=2",
                r" :tmi.twitch.tv USERNOTICE #SomeChannel :a whole year!",
            ),
        );
        assert_eq!(n.kind, "resub");
        assert_eq!(n.channel, "somechannel");
        assert_eq!(n.login, "someone");
        assert_eq!(n.months, Some(12));
        assert_eq!(n.tier.as_deref(), Some("1000"));
        assert_eq!(n.system_text, "Someone subscribed at Tier 1.");
        assert_eq!(n.text.as_deref(), Some("a whole year!"));
        assert_eq!(n.sent_at, 1_700_000_000_000);
        assert!(n.is_sub());
    }

    #[test]
    fn parses_gift_bomb() {
        let bomb = notice(
            concat!(
                r"@display-name=Gifter;login=gifter;msg-id=submysterygift;msg-param-mass-gift-count=5;",
                r"msg-param-community-gift-id=99;msg-param-sub-plan=1000;system-msg=Gifter\sis\sgifting\s5\ssubs",
                r" :tmi.twitch.tv USERNOTICE #chan",
            ),
        );
        assert_eq!(bomb.count, Some(5));
        assert_eq!(bomb.text, None);
        assert!(!bomb.is_sub(), "the gifts are counted individually");

        let gift = notice(
            concat!(
                "@display-name=Gifter;login=gifter;msg-id=subgift;msg-param-community-gift-id=99;",
                "msg-param-recipient-user-name=lucky;msg-param-recipient-display-name=Lucky;msg-param-months=3",
                " :tmi.twitch.tv USERNOTICE #chan",
            ),
        );
        assert!(gift.bulk_gift);
        assert_eq!(gift.recipient_display_name.as_deref(), Some("Lucky"));
        assert_eq!(gift.months, Some(3));
        assert!(gift.is_sub());
    }

    fn mod_action(line: &str) -> ModAction {
        build_mod_action(&parse_line(line).unwrap())
    }

    #[test]
    fn parses_moderation() {
        let timeout = mod_action(
            "@ban-duration=600;room-id=1;target-user-id=2;tmi-sent-ts=1700000000000 :tmi.twitch.tv CLEARCHAT #chan :Rowdy",
        );
        assert_eq!(timeout.kind, ModKind::Timeout(600));
        assert_eq!(timeout.target_login.as_deref(), Some("rowdy"));
        assert_eq!(timeout.target_user_id.as_deref(), Some("2"));
        assert_eq!(timeout.sent_at, 1_700_000_000_000);

        let ban = mod_action("@room-id=1;target-user-id=2 :tmi.twitch.tv CLEARCHAT #chan :rowdy");
        assert_eq!(ban.kind, ModKind::Ban);

        let clear = mod_action("@room-id=1 :tmi.twitch.tv CLEARCHAT #chan");
        assert_eq!(clear.kind, ModKind::Clear);
        assert_eq!(clear.target_login, None);

        let delete = mod_action(
            "@login=rowdy;room-id=;target-msg-id=abc :tmi.twitch.tv CLEARMSG #chan :something rude",
        );
        assert_eq!(delete.kind, ModKind::Delete);
        assert_eq!(delete.target_msg_id.as_deref(), Some("abc"));
        assert_eq!(delete.target_login.as_deref(), Some("rowdy"));
        assert_eq!(delete.text.as_deref(), Some("something rude"));
    }

    #[test]
    fn parses_bits_and_emotes() {
        let msg = build_message(
            &parse_line(
                "@bits=100;display-name=Fan;emotes=25:0-4;id=x :fan!fan@fan.tmi.twitch.tv PRIVMSG #chan :Kappa Cheer100",
            )
            .unwrap(),
        );
        assert_eq!(msg.bits, Some(100));
        assert_eq!(msg.emotes, "25:0-4");
        let cheer = msg.cheer().unwrap();
        assert_eq!(cheer.kind, "cheer");
        assert_eq!(cheer.count, Some(100));
        assert_eq!(cheer.system_text, "Fan cheered 100 bits!");

        let plain = build_message(&parse_line(":a!a@a.tmi.twitch.tv PRIVMSG #chan :hi").unwrap());
        assert_eq!(plain.bits, None);
        assert!(plain.cheer().is_none());
    }

    #[test]
    fn parses_raid() {
        let n = notice(
            "@display-name=Raider;login=raider;msg-id=raid;msg-param-viewerCount=1234 :tmi.twitch.tv USERNOTICE #chan",
        );
        assert_eq!(n.kind, "raid");
        assert_eq!(n.count, Some(1234));
    }
}
