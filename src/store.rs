use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::irc::{ChatMessage, ModAction, ModKind, UserNotice};

/// One viewer-count reading of a live channel.
#[derive(Debug, Clone)]
pub struct StreamSample {
    pub channel: String,
    pub stream_id: String,
    /// Unix milliseconds.
    pub sampled_at: i64,
    pub started_at: i64,
    pub viewer_count: i64,
    pub title: String,
    pub game_name: String,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    id           INTEGER PRIMARY KEY,
    msg_id       TEXT UNIQUE,
    channel      TEXT NOT NULL,
    room_id      TEXT,
    user_id      TEXT,
    login        TEXT NOT NULL,
    display_name TEXT NOT NULL,
    color        TEXT,
    badges       TEXT NOT NULL,
    text         TEXT NOT NULL,
    sent_at      INTEGER NOT NULL,
    received_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_channel_sent_at ON messages (channel, sent_at);
CREATE INDEX IF NOT EXISTS messages_user_id ON messages (user_id);
CREATE INDEX IF NOT EXISTS messages_login ON messages (login);
CREATE INDEX IF NOT EXISTS messages_sent_at ON messages (sent_at);

-- Subs, resubs, gifted subs, raids, announcements, ... (IRC USERNOTICEs).
CREATE TABLE IF NOT EXISTS events (
    id                     INTEGER PRIMARY KEY,
    msg_id                 TEXT UNIQUE,
    channel                TEXT NOT NULL,
    room_id                TEXT,
    user_id                TEXT,
    login                  TEXT NOT NULL,
    display_name           TEXT NOT NULL,
    color                  TEXT,
    kind                   TEXT NOT NULL,
    tier                   TEXT,
    months                 INTEGER,
    count                  INTEGER,
    recipient_login        TEXT,
    recipient_display_name TEXT,
    bulk_gift              INTEGER NOT NULL DEFAULT 0,
    system_text            TEXT NOT NULL,
    text                   TEXT,
    sent_at                INTEGER NOT NULL,
    received_at            INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS events_channel_sent_at ON events (channel, sent_at);
CREATE INDEX IF NOT EXISTS events_sent_at ON events (sent_at);
CREATE INDEX IF NOT EXISTS events_user_id ON events (user_id);

-- Bans, timeouts, /clear and deleted messages (IRC CLEARCHAT / CLEARMSG).
CREATE TABLE IF NOT EXISTS moderation (
    id             INTEGER PRIMARY KEY,
    channel        TEXT NOT NULL,
    room_id        TEXT,
    kind           TEXT NOT NULL, -- 'ban', 'timeout', 'clear', 'delete'
    duration       INTEGER,       -- seconds, for timeouts
    target_user_id TEXT,
    target_login   TEXT,
    target_msg_id  TEXT,
    text           TEXT,          -- the deleted message, for 'delete'
    sent_at        INTEGER NOT NULL,
    received_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS moderation_channel_sent_at ON moderation (channel, sent_at);
CREATE INDEX IF NOT EXISTS moderation_target_user_id ON moderation (target_user_id);

-- Viewer counts, sampled about once a minute while a channel is live.
CREATE TABLE IF NOT EXISTS stream_samples (
    id           INTEGER PRIMARY KEY,
    channel      TEXT NOT NULL,
    stream_id    TEXT NOT NULL,
    sampled_at   INTEGER NOT NULL,
    started_at   INTEGER NOT NULL,
    viewer_count INTEGER NOT NULL,
    title        TEXT NOT NULL,
    game_name    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS stream_samples_channel_sampled_at ON stream_samples (channel, sampled_at);

-- Channels the recorder should be in. `pinned` rows come from the recorder's
-- command line and are rewritten on every start; the rest are added and
-- removed from the web dashboard.
CREATE TABLE IF NOT EXISTS watched_channels (
    channel  TEXT PRIMARY KEY,
    pinned   INTEGER NOT NULL DEFAULT 0,
    added_at INTEGER NOT NULL
);
";

/// Columns added to `messages` after it was first released; added to older
/// databases on open.
const MESSAGE_COLUMNS_ADDED: [(&str, &str); 3] = [
    ("bits", "INTEGER"),
    // Raw `emotes` tag.
    ("emotes", "TEXT"),
    // When a moderator deleted it, or timed out or banned its author.
    ("deleted_at", "INTEGER"),
];

/// A ban or timeout hides the user's messages from this long before it, the
/// way Twitch's chat clears whatever of theirs is still on screen.
const BAN_REMOVES_MS: i64 = 10 * 60 * 1000;

/// Full-text index over `messages.text`, kept in sync by triggers.
const FTS_SCHEMA: &str = "
CREATE VIRTUAL TABLE messages_fts USING fts5(
    text,
    content = 'messages',
    content_rowid = 'id',
    tokenize = 'unicode61 remove_diacritics 2'
);
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
-- Index messages recorded before full-text search existed.
INSERT INTO messages_fts (messages_fts) VALUES ('rebuild');
";

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        // Set before anything that takes a lock, so a second process opening the
        // same database (recorder + web server starting together) waits instead
        // of failing with "database is locked".
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Inserts all messages in one transaction. Messages whose id is already
    /// stored (e.g. replayed around a reconnect) are skipped. Returns the
    /// number of rows actually inserted.
    pub fn insert_batch(&mut self, messages: &[ChatMessage]) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO messages
                    (msg_id, channel, room_id, user_id, login, display_name,
                     color, badges, text, bits, emotes, sent_at, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for msg in messages {
                inserted += stmt.execute(params![
                    msg.id,
                    msg.channel,
                    msg.room_id,
                    msg.user_id,
                    msg.login,
                    msg.display_name,
                    msg.color.map(hex_color),
                    msg.badges_raw,
                    msg.text,
                    msg.bits,
                    Some(&msg.emotes).filter(|e| !e.is_empty()),
                    msg.sent_at,
                    msg.received_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Records bans, timeouts and deletions, and marks the affected messages
    /// as deleted. Call after inserting the messages received before them.
    pub fn insert_moderation(&mut self, actions: &[ModAction]) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO moderation
                    (channel, room_id, kind, duration, target_user_id, target_login,
                     target_msg_id, text, sent_at, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            let mut delete_one = tx.prepare_cached(
                "UPDATE messages SET deleted_at = ?2 WHERE msg_id = ?1 AND deleted_at IS NULL",
            )?;
            let mut delete_recent = tx.prepare_cached(
                "UPDATE messages SET deleted_at = ?4
                 WHERE user_id = ?1 AND channel = ?2 AND sent_at >= ?3 AND sent_at <= ?4 AND deleted_at IS NULL",
            )?;
            for a in actions {
                let duration = match a.kind {
                    ModKind::Timeout(seconds) => Some(seconds),
                    _ => None,
                };
                insert.execute(params![
                    a.channel,
                    a.room_id,
                    a.kind.as_str(),
                    duration,
                    a.target_user_id,
                    a.target_login,
                    a.target_msg_id,
                    a.text,
                    a.sent_at,
                    a.received_at,
                ])?;
                match (a.kind, &a.target_msg_id, &a.target_user_id) {
                    (ModKind::Delete, Some(msg_id), _) => {
                        delete_one.execute(params![msg_id, a.sent_at])?;
                    }
                    (ModKind::Ban | ModKind::Timeout(_), _, Some(user_id)) => {
                        delete_recent.execute(params![user_id, a.channel, a.sent_at - BAN_REMOVES_MS, a.sent_at])?;
                    }
                    _ => {}
                }
            }
        }
        tx.commit()?;
        Ok(actions.len())
    }

    pub fn insert_samples(&mut self, samples: &[StreamSample]) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO stream_samples
                    (channel, stream_id, sampled_at, started_at, viewer_count, title, game_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for s in samples {
                stmt.execute(params![
                    s.channel,
                    s.stream_id,
                    s.sampled_at,
                    s.started_at,
                    s.viewer_count,
                    s.title,
                    s.game_name
                ])?;
            }
        }
        tx.commit()?;
        Ok(samples.len())
    }

    /// Like `insert_batch`, for subs, raids and other events.
    pub fn insert_events(&mut self, events: &[UserNotice]) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut inserted = 0;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO events
                    (msg_id, channel, room_id, user_id, login, display_name, color,
                     kind, tier, months, count, recipient_login, recipient_display_name,
                     bulk_gift, system_text, text, sent_at, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            )?;
            for e in events {
                inserted += stmt.execute(params![
                    e.id,
                    e.channel,
                    e.room_id,
                    e.user_id,
                    e.login,
                    e.display_name,
                    e.color.map(hex_color),
                    e.kind,
                    e.tier,
                    e.months,
                    e.count,
                    e.recipient_login,
                    e.recipient_display_name,
                    e.bulk_gift,
                    e.system_text,
                    e.text,
                    e.sent_at,
                    e.received_at,
                ])?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Makes `channels` the pinned (command-line) set: channels pinned by a
    /// previous run but no longer listed are removed, and listed channels are
    /// pinned even if they were added from the dashboard.
    pub fn pin_channels(&mut self, channels: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM watched_channels WHERE pinned = 1", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO watched_channels (channel, pinned, added_at) VALUES (?1, 1, ?2)
                 ON CONFLICT (channel) DO UPDATE SET pinned = 1",
            )?;
            let now = now_millis();
            for channel in channels {
                stmt.execute(params![channel, now])?;
            }
        }
        tx.commit()
    }

    /// Every channel the recorder should be in, sorted by name.
    pub fn watched_channels(&self) -> rusqlite::Result<Vec<String>> {
        self.conn
            .prepare_cached("SELECT channel FROM watched_channels ORDER BY channel")?
            .query_map([], |r| r.get(0))?
            .collect()
    }
}

/// Adds a dashboard channel. Returns false if it was already watched.
pub fn add_watched(conn: &Connection, channel: &str) -> rusqlite::Result<bool> {
    let added = conn.execute(
        "INSERT INTO watched_channels (channel, pinned, added_at) VALUES (?1, 0, ?2)
         ON CONFLICT (channel) DO NOTHING",
        params![channel, now_millis()],
    )?;
    Ok(added > 0)
}

pub enum RemoveWatched {
    Removed,
    /// Listed on the recorder's command line, so only removable there.
    Pinned,
    NotFound,
}

pub fn remove_watched(conn: &Connection, channel: &str) -> rusqlite::Result<RemoveWatched> {
    let pinned: Option<bool> = conn
        .query_row("SELECT pinned FROM watched_channels WHERE channel = ?1", [channel], |r| r.get(0))
        .optional()?;
    Ok(match pinned {
        None => RemoveWatched::NotFound,
        Some(true) => RemoveWatched::Pinned,
        Some(false) => {
            conn.execute("DELETE FROM watched_channels WHERE channel = ?1 AND pinned = 0", [channel])?;
            RemoveWatched::Removed
        }
    })
}

/// Rows removed by `delete_channel`.
pub struct Deleted {
    pub messages: usize,
    pub events: usize,
}

/// Rows deleted per transaction, so a recorder writing at the same time is
/// never locked out for longer than its busy timeout.
const DELETE_CHUNK: usize = 5000;

/// Deletes everything recorded in `channel`: messages, events, moderation
/// actions and viewer counts.
pub fn delete_channel(conn: &Connection, channel: &str) -> rusqlite::Result<Deleted> {
    let delete_all = |table: &str| -> rusqlite::Result<usize> {
        let sql = format!(
            "DELETE FROM {table} WHERE id IN (SELECT id FROM {table} WHERE channel = ?1 LIMIT {DELETE_CHUNK})"
        );
        let mut total = 0;
        loop {
            let n = conn.execute(&sql, [channel])?;
            total += n;
            if n < DELETE_CHUNK {
                return Ok(total);
            }
        }
    };
    let deleted = Deleted {
        messages: delete_all("messages")?,
        events: delete_all("events")?,
    };
    delete_all("moderation")?;
    delete_all("stream_samples")?;
    Ok(deleted)
}

/// Normalizes user input to a Twitch login: strips a leading '#', lowercases,
/// and checks it is 1–25 letters, digits or underscores.
pub fn normalize_channel(input: &str) -> Option<String> {
    let name = input.trim().trim_start_matches('#').to_lowercase();
    let valid = (1..=25).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    valid.then_some(name)
}

fn hex_color((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Creates any missing tables and indexes. Safe to run concurrently from the
/// recorder and the web server.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        conn.execute_batch(SCHEMA)?;
        let existing: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('messages')")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for (name, decl) in MESSAGE_COLUMNS_ADDED {
            if !existing.iter().any(|c| c == name) {
                conn.execute_batch(&format!("ALTER TABLE messages ADD COLUMN {name} {decl}"))?;
            }
        }
        let has_fts: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'messages_fts')",
            [],
            |r| r.get(0),
        )?;
        if !has_fts {
            conn.execute_batch(FTS_SCHEMA)?;
        }
        Ok(())
    })();
    conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: Option<&str>, text: &str) -> ChatMessage {
        ChatMessage {
            id: id.map(String::from),
            channel: "somechannel".into(),
            room_id: Some("1".into()),
            user_id: Some("2".into()),
            login: "someone".into(),
            display_name: "Someone".into(),
            color: Some((255, 0, 16)),
            badges: vec!["SUB".into()],
            badges_raw: "subscriber/12".into(),
            text: text.into(),
            bits: None,
            emotes: String::new(),
            sent_at: 1_700_000_000_000,
            received_at: 1_700_000_000_050,
        }
    }

    #[test]
    fn duplicate_ids_are_ignored() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let batch = [message(Some("a"), "hi"), message(Some("a"), "hi"), message(Some("b"), "yo")];
        assert_eq!(store.insert_batch(&batch).unwrap(), 2);
        assert_eq!(store.insert_batch(&batch).unwrap(), 0);
    }

    #[test]
    fn messages_without_id_are_kept() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let batch = [message(None, "one"), message(None, "two")];
        assert_eq!(store.insert_batch(&batch).unwrap(), 2);
    }

    #[test]
    fn stores_color_as_hex() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store.insert_batch(&[message(Some("a"), "hi")]).unwrap();
        let color: String = store
            .conn
            .query_row("SELECT color FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(color, "#FF0010");
    }

    #[test]
    fn full_text_index_follows_inserts() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        store
            .insert_batch(&[message(Some("a"), "Grüße aus Köln KEKW"), message(Some("b"), "nope")])
            .unwrap();
        let hits: Vec<String> = store
            .conn
            .prepare("SELECT m.msg_id FROM messages_fts f JOIN messages m ON m.id = f.rowid WHERE messages_fts MATCH 'kekw OR koln'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(hits, vec!["a".to_string()]);
    }

    #[test]
    fn duplicate_events_are_ignored() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let event = UserNotice {
            id: Some("e".into()),
            channel: "somechannel".into(),
            room_id: None,
            user_id: Some("2".into()),
            login: "someone".into(),
            display_name: "Someone".into(),
            color: None,
            kind: "resub".into(),
            tier: Some("1000".into()),
            months: Some(3),
            count: None,
            recipient_login: None,
            recipient_display_name: None,
            bulk_gift: false,
            system_text: "Someone subscribed at Tier 1.".into(),
            text: None,
            sent_at: 1_700_000_000_000,
            received_at: 1_700_000_000_050,
        };
        assert_eq!(store.insert_events(&[event.clone(), event.clone()]).unwrap(), 1);
        assert_eq!(store.insert_events(&[event]).unwrap(), 0);
    }

    fn deleted_ids(store: &Store) -> Vec<String> {
        store
            .conn
            .prepare("SELECT msg_id FROM messages WHERE deleted_at IS NOT NULL ORDER BY msg_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn action(kind: ModKind, msg_id: Option<&str>, sent_at: i64) -> ModAction {
        ModAction {
            channel: "somechannel".into(),
            room_id: Some("1".into()),
            kind,
            target_user_id: Some("2".into()),
            target_login: Some("someone".into()),
            target_msg_id: msg_id.map(String::from),
            text: None,
            sent_at,
            received_at: sent_at,
        }
    }

    #[test]
    fn moderation_marks_messages_deleted() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let t = 1_700_000_000_000;
        let mut old = message(Some("old"), "from long ago");
        old.sent_at = t - BAN_REMOVES_MS - 1;
        let mut recent = message(Some("recent"), "just now");
        recent.sent_at = t - 1000;
        let mut other_user = message(Some("other"), "bystander");
        other_user.user_id = Some("3".into());
        other_user.sent_at = t - 1000;
        let mut single = message(Some("single"), "rude");
        single.sent_at = t - BAN_REMOVES_MS * 2;
        store.insert_batch(&[old, recent, other_user, single]).unwrap();

        store.insert_moderation(&[action(ModKind::Delete, Some("single"), t)]).unwrap();
        assert_eq!(deleted_ids(&store), ["single"]);

        store.insert_moderation(&[action(ModKind::Timeout(600), None, t)]).unwrap();
        assert_eq!(deleted_ids(&store), ["recent", "single"], "only their recent messages");

        let kinds: Vec<(String, Option<i64>)> = store
            .conn
            .prepare("SELECT kind, duration FROM moderation ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(kinds, [("delete".to_string(), None), ("timeout".to_string(), Some(600))]);
    }

    #[test]
    fn delete_channel_removes_only_that_channel() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let mut other = message(Some("c"), "kekw elsewhere");
        other.channel = "other".into();
        store
            .insert_batch(&[message(Some("a"), "kekw"), message(Some("b"), "hi"), other])
            .unwrap();

        let deleted = delete_channel(&store.conn, "somechannel").unwrap();
        assert_eq!((deleted.messages, deleted.events), (2, 0));
        let left: Vec<String> = store
            .conn
            .prepare("SELECT m.msg_id FROM messages_fts f JOIN messages m ON m.id = f.rowid WHERE messages_fts MATCH 'kekw'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(left, vec!["c".to_string()], "the search index drops deleted messages");
    }

    #[test]
    fn normalizes_channel_names() {
        assert_eq!(normalize_channel(" #PapaPlatte ").as_deref(), Some("papaplatte"));
        assert_eq!(normalize_channel("some_user_42").as_deref(), Some("some_user_42"));
        assert_eq!(normalize_channel(""), None);
        assert_eq!(normalize_channel("#"), None);
        assert_eq!(normalize_channel("two words"), None);
        assert_eq!(normalize_channel("twitch.tv/x"), None);
        assert_eq!(normalize_channel(&"a".repeat(26)), None);
    }

    #[test]
    fn dashboard_channels_can_be_added_and_removed() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        assert!(add_watched(&store.conn, "foo").unwrap());
        assert!(!add_watched(&store.conn, "foo").unwrap(), "adding twice is a no-op");
        assert_eq!(store.watched_channels().unwrap(), vec!["foo".to_string()]);
        assert!(matches!(remove_watched(&store.conn, "foo").unwrap(), RemoveWatched::Removed));
        assert!(matches!(remove_watched(&store.conn, "foo").unwrap(), RemoveWatched::NotFound));
        assert!(store.watched_channels().unwrap().is_empty());
    }

    #[test]
    fn pinned_channels_follow_the_command_line() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        add_watched(&store.conn, "dash").unwrap();
        add_watched(&store.conn, "both").unwrap();
        store.pin_channels(&["cli".into(), "both".into()]).unwrap();
        assert_eq!(store.watched_channels().unwrap(), ["both", "cli", "dash"]);
        assert!(matches!(remove_watched(&store.conn, "cli").unwrap(), RemoveWatched::Pinned));
        assert!(matches!(remove_watched(&store.conn, "both").unwrap(), RemoveWatched::Pinned));

        // A restart without `cli` drops it; dashboard channels stay.
        store.pin_channels(&["both".into()]).unwrap();
        assert_eq!(store.watched_channels().unwrap(), ["both", "dash"]);
    }

    #[test]
    fn migrate_backfills_existing_messages() {
        let dir = std::env::temp_dir().join(format!("twitch-chat-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute(
                "INSERT INTO messages (msg_id, channel, login, display_name, badges, text, sent_at, received_at)
                 VALUES ('old', 'c', 'u', 'U', '', 'hello world', 0, 0)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let n: i64 = store
            .conn
            .query_row("SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'hello'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let bits: Option<i64> = store
            .conn
            .query_row("SELECT bits FROM messages WHERE msg_id = 'old'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bits, None, "new columns are added to old databases");
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
