use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::irc::ChatMessage;

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

-- Channels the recorder should be in. `pinned` rows come from the recorder's
-- command line and are rewritten on every start; the rest are added and
-- removed from the web dashboard.
CREATE TABLE IF NOT EXISTS watched_channels (
    channel  TEXT PRIMARY KEY,
    pinned   INTEGER NOT NULL DEFAULT 0,
    added_at INTEGER NOT NULL
);
";

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
                     color, badges, text, sent_at, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for msg in messages {
                inserted += stmt.execute(params![
                    msg.id,
                    msg.channel,
                    msg.room_id,
                    msg.user_id,
                    msg.login,
                    msg.display_name,
                    msg.color.map(|(r, g, b)| format!("#{r:02X}{g:02X}{b:02X}")),
                    msg.badges_raw,
                    msg.text,
                    msg.sent_at,
                    msg.received_at,
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

/// Normalizes user input to a Twitch login: strips a leading '#', lowercases,
/// and checks it is 1–25 letters, digits or underscores.
pub fn normalize_channel(input: &str) -> Option<String> {
    let name = input.trim().trim_start_matches('#').to_lowercase();
    let valid = (1..=25).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    valid.then_some(name)
}

fn now_millis() -> i64 {
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
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
