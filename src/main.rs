mod app;
mod irc;
mod ui;

use std::io;
use std::time::Duration;

use clap::Parser;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use app::App;
use irc::ChatEvent;

/// Watch a Twitch channel's chat in your terminal.
#[derive(Parser, Debug)]
#[command(name = "twitch-chat", version, about)]
struct Args {
    /// Channel name (as it appears in the twitch.tv URL), with or without '#'
    channel: String,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = Args::parse();
    let channel = args.channel.trim_start_matches('#').to_string();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, channel).await;

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

async fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, channel: String) -> io::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ChatEvent>();
    tokio::spawn(irc::run(channel.clone(), tx));

    let mut app = App::new(channel);
    let mut events = EventStream::new();

    terminal.draw(|f| ui::draw(f, &mut app))?;

    loop {
        let mut dirty = false;

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                            KeyCode::Char('c') if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
                                app.should_quit = true;
                            }
                            KeyCode::Up | KeyCode::Char('k') => app.scroll_up(1),
                            KeyCode::Down | KeyCode::Char('j') => app.scroll_down(1),
                            KeyCode::PageUp => app.scroll_up(app.last_viewport.max(1)),
                            KeyCode::PageDown => app.scroll_down(app.last_viewport.max(1)),
                            KeyCode::Home | KeyCode::Char('g') => app.scroll_to_top(),
                            KeyCode::End | KeyCode::Char('G') => app.scroll_to_bottom(),
                            _ => {}
                        }
                        dirty = true;
                    }
                    Some(Ok(Event::Resize(_, _))) => dirty = true,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => app.should_quit = true,
                }
            }
            maybe_chat = rx.recv() => {
                match maybe_chat {
                    Some(ChatEvent::Message(msg)) => {
                        app.push_message(msg);
                        dirty = true;
                    }
                    Some(ChatEvent::Connecting) => {
                        app.status = app::ConnStatus::Connecting;
                        dirty = true;
                    }
                    Some(ChatEvent::Connected) => {
                        app.status = app::ConnStatus::Connected;
                        dirty = true;
                    }
                    Some(ChatEvent::Disconnected(reason)) => {
                        app.status = app::ConnStatus::Disconnected(reason);
                        dirty = true;
                    }
                    Some(ChatEvent::SystemNotice(notice)) => {
                        app.last_notice = Some(notice);
                        dirty = true;
                    }
                    None => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }

        if app.should_quit {
            return Ok(());
        }

        if dirty {
            terminal.draw(|f| ui::draw(f, &mut app))?;
        }
    }
}
