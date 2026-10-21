# twitch-chat

A terminal-based Twitch chat client built in Rust with [ratatui](https://github.com/ratatui-org/ratatui).

Connects to a channel's chat over IRC and displays it in a scrollable terminal UI, with optional live stream status (title, category, viewer count) via the Twitch Helix API.

## Features

- Live chat via Twitch IRC (anonymous, read-only)
- Scrollable message history
- Message filtering (search-as-you-type)
- Highlight moderators / subscribers
- Subs, resubs, gifted subs, raids and announcements shown inline as they happen
- Cheers (bits), bans, timeouts and deleted messages: moderated messages are struck through
- Headless `record` mode that stores chat from many channels in SQLite
- Web UI (`serve`) with dashboards, full-text search and user history
- Optional stream status (online/offline, title, category, viewers) when API credentials are provided
- Send chat messages as your own account, from the web UI or its API

## Usage

```sh
cargo run --release -- <channel>
```

`<channel>` is the channel name as it appears in the twitch.tv URL (with or without a leading `#`).

## Recording chat to a database

`record` runs headless (no terminal UI) and stores every chat message, event (subs, cheers, raids, ...) and moderation action (bans, timeouts, deleted messages) from one or more channels in a SQLite database. With [API credentials](#twitch-api-credentials-optional) it also records each live channel's viewer count, title and category once a minute. It is meant to run unattended on a server:

```sh
twitch-chat record --db chat.db papaplatte xqc
```

- Channels can also be added and removed in the web UI (see below). The recorder checks for changes every 5 seconds and joins or leaves channels without restarting. Channels given on the command line are always recorded and can only be removed there.
- Messages are written in batches (every second, or every 500 messages).
- Each message's Twitch id is stored with a unique constraint, so duplicates are skipped.
- It reconnects automatically. Messages sent while disconnected are lost, because Twitch IRC has no replay.
- `Ctrl+C` / `SIGTERM` flush pending messages before exiting.
- A line like `stored 412 message(s), 3 event(s) and 1 moderation action(s) in the last 60s` is logged to stderr every minute.
- A ban or timeout marks the user's messages from the 10 minutes before it as deleted (`deleted_at`), like Twitch's chat clearing what's still on screen. A deleted single message is marked exactly.

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
    received_at  INTEGER NOT NULL,
    bits         INTEGER,       -- bits cheered with the message
    emotes       TEXT,          -- raw tag, e.g. '25:0-4,6-10' (character positions in text)
    deleted_at   INTEGER        -- set when a moderator removed it
);

-- Subs, cheers, raids, announcements, watch streaks, ... (IRC USERNOTICEs,
-- plus a 'cheer' row for each message with bits).
-- Only events that happen while the recorder runs are captured.
CREATE TABLE events (
    id                     INTEGER PRIMARY KEY,
    msg_id                 TEXT UNIQUE,
    channel                TEXT NOT NULL,
    room_id                TEXT,
    user_id                TEXT,    -- who triggered it: subscriber, gifter, raider
    login                  TEXT NOT NULL,
    display_name           TEXT NOT NULL,
    color                  TEXT,
    kind                   TEXT NOT NULL, -- 'sub', 'resub', 'subgift', 'submysterygift', 'cheer', 'raid', 'announcement', ...
    tier                   TEXT,    -- 'Prime', '1000', '2000', '3000'
    months                 INTEGER, -- cumulative months (the recipient's, for gifts)
    count                  INTEGER, -- subs in a gift bomb, bits in a cheer, or viewers in a raid
    recipient_login        TEXT,
    recipient_display_name TEXT,
    bulk_gift              INTEGER NOT NULL, -- 1 for each single gift of a gift bomb
    system_text            TEXT NOT NULL,    -- Twitch's wording, e.g. 'X subscribed at Tier 1.'
    text                   TEXT,             -- the user's attached message
    sent_at                INTEGER NOT NULL,
    received_at            INTEGER NOT NULL
);

-- Bans, timeouts, /clear and single deleted messages (IRC CLEARCHAT / CLEARMSG).
CREATE TABLE moderation (
    id             INTEGER PRIMARY KEY,
    channel        TEXT NOT NULL,
    room_id        TEXT,
    kind           TEXT NOT NULL, -- 'ban', 'timeout', 'clear', 'delete'
    duration       INTEGER,       -- seconds, for timeouts
    target_user_id TEXT,
    target_login   TEXT,
    target_msg_id  TEXT,          -- for 'delete': messages.msg_id
    text           TEXT,          -- for 'delete': the deleted message
    sent_at        INTEGER NOT NULL,
    received_at    INTEGER NOT NULL
);

-- Viewer counts, once a minute while a channel is live (needs API credentials).
CREATE TABLE stream_samples (
    id           INTEGER PRIMARY KEY,
    channel      TEXT NOT NULL,
    stream_id    TEXT NOT NULL,
    sampled_at   INTEGER NOT NULL,
    started_at   INTEGER NOT NULL,
    viewer_count INTEGER NOT NULL,
    title        TEXT NOT NULL,
    game_name    TEXT NOT NULL
);
```

Databases from older versions get the new tables and columns automatically when opened.

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

- **Channels**: every recorded channel, its message count and when the last message arrived. The **Recording** panel adds or removes channels for the recorder (it accepts a name, `#name` or a twitch.tv link).
- **Channel dashboard**: messages over time, unique chatters, busiest period, subs, bits, moderation counts, live lists of subs/cheers/raids and of timeouts/bans/deletions, top chatters and most repeated messages; with API credentials also viewer counts over time, whether the channel is live now, and its profile picture. All of it for the last hour, 24 hours, 7 days, 30 days or all time. Messages show Twitch emotes, cheered bits, and (with API credentials) real badge images; moderated messages are struck through. for the last hour, 24 hours, 7 days, 30 days or all time. Click a point on the chart to read the chat from that moment. **Delete channel data…** removes everything recorded in the channel (and can stop recording it). The database file doesn't shrink afterwards; the space is reused for new messages, or run `sqlite3 chat.db VACUUM` while nothing else has it open.
- **Search**: full-text search across all messages, filterable by channel and user. Every word must appear, and words match as prefixes (`kek` finds `KEKW`).
- **Context**: click a message's timestamp to see the conversation around it
- **Users**: a chatter's message history, the names they used, the channels they chat in, their subs and cheers, and any timeouts, bans or deleted messages

It listens on localhost only by default. To expose it, set `TWITCH_CHAT_PASSWORD` (any username is accepted) and put it behind a TLS reverse proxy, or keep it on localhost and use an SSH tunnel: `ssh -L 8080:localhost:8080 your-server`.

### Running with Docker

`docker-compose.yml` runs the recorder and the web UI as two containers built from the same image, sharing the database on a named volume (`chat-data`).

1. Optionally set channels to always record on the `record` service's `command:` line in `docker-compose.yml`. You can add more later in the web UI.
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

## Twitch API credentials (optional)

Create a [Twitch application](https://dev.twitch.tv/console/apps) and set its credentials in a `.env` file (see `.env.example`) or the environment:

```
TWITCH_CLIENT_ID=your_client_id
TWITCH_CLIENT_SECRET=your_client_secret
```

They enable:

- **Terminal UI**: live stream status (title, category, viewers, uptime).
- **`record`**: viewer counts, title and category of live channels, once a minute.
- **`serve`**: profile pictures and real badge images.

Only public data is read (app access token, no login). Without credentials everything else works as before. Sending messages additionally needs a login; see [Sending messages](#sending-messages).

## Sending messages

`serve` can post chat messages as your Twitch account. It's off until you log in:

1. Set up your [Twitch application's credentials](#twitch-api-credentials-optional). Only `TWITCH_CLIENT_ID` is needed if the app is registered as a *Public* client.
2. Open the web UI and click **Log in to Twitch** in the **Twitch account** panel on the Channels page. It shows a code: enter it at twitch.tv/activate while logged in as the account that should send messages. The panel switches to "Sending messages as …" once you've approved it. **Switch account** and **Log out** are in the same panel.

   You can also log in from the command line, before starting the server:

   ```sh
   twitch-chat login                      # saves twitch-token.json
   twitch-chat serve --db chat.db         # picks up twitch-token.json
   ```

   Use `--token <path>` on both commands to keep the token elsewhere. The Docker setup keeps it at `/data/twitch-token.json`.

3. Send messages from the box above **Latest messages** on a channel's page, or through the API:

   ```sh
   curl -X POST http://127.0.0.1:8080/api/channels/papaplatte/messages \
        -H 'Content-Type: application/json' \
        -d '{"message": "hello chat"}'
   # {"msg_id":"…"}
   ```

   Add `"reply_to": "<msg_id>"` to reply to a message (`msg_id` from `/api/messages`). Your message shows up in the web UI once the recorder receives it, if it records that channel.

Errors come back as `{"error": "…"}`. `400` means the message is empty, longer than 500 characters, or Twitch refused it (you're banned, followers-only mode, AutoMod, duplicate message, …). `404` means the channel doesn't exist. `502` means Twitch couldn't be reached or the login expired; log in again. `503` means you haven't logged in yet.

The account API, for scripts:

| Request | Does |
| --- | --- |
| `GET /api/chat-account` | `{"enabled", "login", "pending", "error"}`: `login` is the account messages are sent as; `pending` holds `user_code`, `verification_uri` and `expires_in` (seconds) while a login waits for the code |
| `POST /api/chat-account/login` with body `{}` | Starts a login (replacing a pending one) and returns the same object. The account switches once the code is entered |
| `DELETE /api/chat-account` | Cancels a pending login, or logs out: revokes the token at Twitch and deletes the token file |

The token is renewed automatically and written back to the file. Anyone who can reach the server can post as you and change the logged-in account, so set `TWITCH_CHAT_PASSWORD` whenever it isn't on localhost only, and keep `twitch-token.json` private.

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
