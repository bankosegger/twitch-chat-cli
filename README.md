# twitch-chat

A terminal-based Twitch chat client built in Rust with [ratatui](https://github.com/ratatui-org/ratatui).

Connects to a channel's chat over IRC and displays it in a scrollable terminal UI, with optional live stream status (title, category, viewer count) via the Twitch Helix API.

## Features

- Live chat via Twitch IRC (anonymous, read-only)
- Scrollable message history
- Message filtering (search-as-you-type)
- Highlight moderators / subscribers
- Headless `record` mode that stores chat from many channels in SQLite
- Web UI (`serve`) with dashboards, full-text search and user history
- Optional stream status (online/offline, title, category, viewers) when API credentials are provided

## Usage

```sh
cargo run --release -- <channel>
```

`<channel>` is the channel name as it appears in the twitch.tv URL (with or without a leading `#`).

## Recording chat to a database

`record` runs headless (no terminal UI) and stores every chat message from one or more channels in a SQLite database. It is meant to run unattended on a server:

```sh
twitch-chat record --db chat.db papaplatte xqc
```

- Messages are written in batches (every second, or every 500 messages).
- Each message's Twitch id is stored with a unique constraint, so duplicates are skipped.
- It reconnects automatically. Messages sent while disconnected are lost, because Twitch IRC has no replay.
- `Ctrl+C` / `SIGTERM` flush pending messages before exiting.
- A line like `stored 412 message(s) in the last 60s` is logged to stderr every minute.

Schema (timestamps are Unix milliseconds):

```sql
CREATE TABLE messages (
    id           INTEGER PRIMARY KEY,
    msg_id       TEXT UNIQUE,   -- Twitch message id
    channel      TEXT NOT NULL, -- lowercase, without '#'
    room_id      TEXT,          -- channel's user id
    user_id      TEXT,
    login        TEXT NOT NULL,
    display_name TEXT NOT NULL,
    color        TEXT,          -- e.g. '#1E90FF'
    badges       TEXT NOT NULL, -- raw tag, e.g. 'subscriber/12,vip/1'
    text         TEXT NOT NULL,
    sent_at      INTEGER NOT NULL,
    received_at  INTEGER NOT NULL
);
```

A full-text index (`messages_fts`, SQLite FTS5) is kept in sync automatically. Example query:

```sh
sqlite3 chat.db "SELECT datetime(sent_at / 1000, 'unixepoch'), display_name, text
                 FROM messages WHERE channel = 'papaplatte' ORDER BY sent_at DESC LIMIT 20"
```

## Web UI

`serve` starts a web app for browsing and analysing a database written by `record`. It can run while the recorder is writing.

```sh
twitch-chat serve --db chat.db                       # http://127.0.0.1:8080
twitch-chat serve --db chat.db --bind 0.0.0.0:8080   # reachable from other machines
```

- **Channels**: every recorded channel, its message count and when the last message arrived
- **Channel dashboard**: messages over time, unique chatters, busiest period, top chatters and most repeated messages, for the last hour, 24 hours, 7 days, 30 days or all time. Click a point on the chart to read the chat from that moment.
- **Search**: full-text search across all messages, filterable by channel and user. Every word must appear, and words match as prefixes (`kek` finds `KEKW`).
- **Context**: click a message's timestamp to see the conversation around it
- **Users**: a chatter's message history, the names they used, and the channels they chat in

It listens on localhost only by default. To expose it, set `TWITCH_CHAT_PASSWORD` (any username is accepted) and put it behind a TLS reverse proxy, or keep it on localhost and use an SSH tunnel: `ssh -L 8080:localhost:8080 your-server`.

### Running with Docker

`docker-compose.yml` runs the recorder and the web UI as two containers built from the same image, sharing the database on a named volume (`chat-data`).

1. Set the channels on the `record` service's `command:` line in `docker-compose.yml`.
2. Start everything:

   ```sh
   docker compose up -d --build
   ```

3. Open http://localhost:8080.

```sh
docker compose logs -f record     # follow the recorder's log
docker compose down               # stop (the data stays in the volume)
```

The web UI is published on `127.0.0.1` only. To reach it from other machines, change the port mapping to `"8080:8080"` and set a password in a `.env` file next to `docker-compose.yml`:

```
TWITCH_CHAT_PASSWORD=choose-something-long
```

To copy the database out of the volume, for example to analyse it with other tools, copy the whole folder. The newest messages may still be in `chat.db-wal` next to `chat.db`.

```sh
docker compose cp web:/data ./chat-data
```

### Running as a systemd service

```ini
# /etc/systemd/system/twitch-chat-record.service
[Unit]
Description=Record Twitch chat to SQLite
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/twitch-chat record --db /var/lib/twitch-chat/chat.db papaplatte
StateDirectory=twitch-chat
User=twitch-chat
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

The web UI runs as a second service with the same user and database:

```ini
# /etc/systemd/system/twitch-chat-web.service
[Unit]
Description=Twitch chat web UI
After=network-online.target

[Service]
ExecStart=/usr/local/bin/twitch-chat serve --db /var/lib/twitch-chat/chat.db --bind 127.0.0.1:8080
User=twitch-chat
Restart=always

[Install]
WantedBy=multi-user.target
```

```sh
sudo useradd --system --home /var/lib/twitch-chat twitch-chat
sudo systemctl enable --now twitch-chat-record twitch-chat-web
journalctl -u twitch-chat-record -f
```

## Stream status (optional)

To show live stream status, create a [Twitch application](https://dev.twitch.tv/console/apps) and set its credentials in a `.env` file (see `.env.example`):

```
TWITCH_CLIENT_ID=your_client_id
TWITCH_CLIENT_SECRET=your_client_secret
```

Without credentials, the app runs in chat-only mode.

## Keybindings

| Key | Action |
| --- | --- |
| `q` / `Esc` | Quit |
| `Ctrl+C` | Quit |
| `↑` / `k` | Scroll up |
| `↓` / `j` | Scroll down |
| `Page Up` / `Page Down` | Scroll by page |
| `Home` / `g` | Scroll to top |
| `End` / `G` | Scroll to bottom |
| `/` | Edit message filter |
| `m` / `M` | Toggle mod highlighting |
| `s` / `S` | Toggle sub highlighting |

While editing the filter: `Enter` confirms, `Esc` cancels, `Backspace` deletes, `Ctrl+U` clears.
