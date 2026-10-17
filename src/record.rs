use std::io;
use std::path::Path;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::irc::{self, ChatEvent, ChatMessage};
use crate::store::Store;

/// Flush to the database once this many messages are pending...
const BATCH_SIZE: usize = 500;
/// ...or at least this often.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const STATS_INTERVAL: Duration = Duration::from_secs(60);
/// Bounded so a stalled database applies backpressure instead of growing memory.
const QUEUE_CAPACITY: usize = 10_000;

/// Headless mode: joins `channels` and writes every chat message to SQLite
/// until interrupted (Ctrl+C / SIGTERM).
pub async fn run(channels: Vec<String>, db_path: &Path) -> io::Result<()> {
    let mut store = Store::open(db_path).map_err(io::Error::other)?;
    eprintln!(
        "recording {} channel(s) to {}: {}",
        channels.len(),
        db_path.display(),
        channels.join(", ")
    );

    let (tx, mut rx) = mpsc::channel::<ChatEvent>(QUEUE_CAPACITY);
    tokio::spawn(irc::run(channels, tx));

    let mut pending: Vec<ChatMessage> = Vec::with_capacity(BATCH_SIZE);
    let mut stored_since_stats = 0usize;
    let mut flush_tick = tokio::time::interval(FLUSH_INTERVAL);
    let mut stats_tick = tokio::time::interval(STATS_INTERVAL);
    stats_tick.tick().await;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(ChatEvent::Message(msg)) => {
                        pending.push(msg);
                        if pending.len() >= BATCH_SIZE {
                            stored_since_stats += flush(&mut store, &mut pending);
                        }
                    }
                    Some(ChatEvent::Connecting) => eprintln!("connecting..."),
                    Some(ChatEvent::Connected) => eprintln!("connected"),
                    Some(ChatEvent::Disconnected(reason)) => eprintln!("disconnected: {reason}"),
                    Some(ChatEvent::SystemNotice(notice)) => eprintln!("notice: {notice}"),
                    None => break,
                }
            }
            _ = flush_tick.tick() => {
                stored_since_stats += flush(&mut store, &mut pending);
            }
            _ = stats_tick.tick() => {
                eprintln!("stored {stored_since_stats} message(s) in the last {}s", STATS_INTERVAL.as_secs());
                stored_since_stats = 0;
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

/// Writes and clears `pending`, returning how many rows were inserted. On a
/// database error the batch is logged and dropped so recording can continue.
fn flush(store: &mut Store, pending: &mut Vec<ChatMessage>) -> usize {
    if pending.is_empty() {
        return 0;
    }
    let result = tokio::task::block_in_place(|| store.insert_batch(pending));
    let count = pending.len();
    pending.clear();
    match result {
        Ok(inserted) => inserted,
        Err(e) => {
            eprintln!("failed to store {count} message(s): {e}");
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
