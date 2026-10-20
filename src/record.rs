use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::irc::{self, ChatEvent, ChatMessage, UserNotice};
use crate::store::{self, Store};

/// Flush to the database once this many messages are pending...
const BATCH_SIZE: usize = 500;
/// ...or at least this often.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const STATS_INTERVAL: Duration = Duration::from_secs(60);
/// How often to check the database for channels added or removed in the
/// web dashboard.
const CHANNELS_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Bounded so a stalled database applies backpressure instead of growing memory.
const QUEUE_CAPACITY: usize = 10_000;

/// Messages and events received since the last flush.
#[derive(Default)]
struct Pending {
    messages: Vec<ChatMessage>,
    events: Vec<UserNotice>,
}

impl Pending {
    fn len(&self) -> usize {
        self.messages.len() + self.events.len()
    }
}

/// Headless mode: joins `channels` and writes every chat message and event to SQLite
/// until interrupted (Ctrl+C / SIGTERM).
pub async fn run(pinned: Vec<String>, db_path: &Path) -> io::Result<()> {
    let mut store = Store::open(db_path).map_err(io::Error::other)?;

    let mut valid = Vec::with_capacity(pinned.len());
    for name in &pinned {
        match store::normalize_channel(name) {
            Some(channel) => valid.push(channel),
            None => eprintln!("ignoring invalid channel name {name:?}"),
        }
    }
    store.pin_channels(&valid).map_err(io::Error::other)?;

    let initial = store.watched_channels().map_err(io::Error::other)?;
    eprintln!("recording to {}", db_path.display());
    log_channels(&initial);

    let (channels_tx, channels_rx) = watch::channel(initial);
    let (tx, mut rx) = mpsc::channel::<ChatEvent>(QUEUE_CAPACITY);
    tokio::spawn(irc::run(channels_rx, tx));

    let mut pending = Pending::default();
    let mut stored_since_stats = Stored::default();
    let mut flush_tick = tokio::time::interval(FLUSH_INTERVAL);
    let mut stats_tick = tokio::time::interval(STATS_INTERVAL);
    stats_tick.tick().await;
    let mut channels_tick = tokio::time::interval(CHANNELS_POLL_INTERVAL);
    channels_tick.tick().await;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(ChatEvent::Message(msg)) => pending.messages.push(msg),
                    Some(ChatEvent::UserNotice(event)) => pending.events.push(event),
                    Some(ChatEvent::Connecting) => eprintln!("connecting..."),
                    Some(ChatEvent::Connected) => eprintln!("connected"),
                    Some(ChatEvent::Disconnected(reason)) => eprintln!("disconnected: {reason}"),
                    Some(ChatEvent::SystemNotice(notice)) => eprintln!("notice: {notice}"),
                    None => break,
                }
                if pending.len() >= BATCH_SIZE {
                    stored_since_stats.add(flush(&mut store, &mut pending));
                }
            }
            _ = flush_tick.tick() => {
                stored_since_stats.add(flush(&mut store, &mut pending));
            }
            _ = stats_tick.tick() => {
                let Stored { messages, events } = std::mem::take(&mut stored_since_stats);
                eprintln!(
                    "stored {messages} message(s) and {events} event(s) in the last {}s",
                    STATS_INTERVAL.as_secs()
                );
            }
            _ = channels_tick.tick() => {
                // Picks up channels added or removed in the web dashboard.
                match tokio::task::block_in_place(|| store.watched_channels()) {
                    Ok(wanted) if wanted != *channels_tx.borrow() => {
                        log_changes(&channels_tx.borrow(), &wanted);
                        channels_tx.send_replace(wanted);
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("failed to read watched channels: {e}"),
                }
            }
            _ = &mut shutdown => {
                eprintln!("shutting down");
                break;
            }
        }
    }

    flush(&mut store, &mut pending);
    Ok(())
}

fn log_channels(channels: &[String]) {
    if channels.is_empty() {
        eprintln!("no channels to record yet; add some in the web dashboard or on the command line");
    } else {
        eprintln!("recording {} channel(s): {}", channels.len(), channels.join(", "));
    }
}

fn log_changes(old: &[String], new: &[String]) {
    for channel in new.iter().filter(|c| !old.contains(c)) {
        eprintln!("joining {channel}");
    }
    for channel in old.iter().filter(|c| !new.contains(c)) {
        eprintln!("leaving {channel}");
    }
    log_channels(new);
}

/// Rows actually inserted.
#[derive(Default)]
struct Stored {
    messages: usize,
    events: usize,
}

impl Stored {
    fn add(&mut self, other: Stored) {
        self.messages += other.messages;
        self.events += other.events;
    }
}

/// Writes and clears `pending`, returning how many rows were inserted. On a
/// database error the batch is logged and dropped so recording can continue.
fn flush(store: &mut Store, pending: &mut Pending) -> Stored {
    Stored {
        messages: insert_all(&mut pending.messages, "message", |batch| store.insert_batch(batch)),
        events: insert_all(&mut pending.events, "event", |batch| store.insert_events(batch)),
    }
}

fn insert_all<T>(pending: &mut Vec<T>, noun: &str, insert: impl FnOnce(&[T]) -> rusqlite::Result<usize>) -> usize {
    if pending.is_empty() {
        return 0;
    }
    let result = tokio::task::block_in_place(|| insert(pending));
    let count = pending.len();
    pending.clear();
    match result {
        Ok(inserted) => inserted,
        Err(e) => {
            eprintln!("failed to store {count} {noun}(s): {e}");
            0
        }
    }
}

pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
