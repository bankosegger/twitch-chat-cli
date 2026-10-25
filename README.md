# twitch-chat

Twitch chat client in Rust: a terminal UI ([ratatui](https://github.com/ratatui-org/ratatui)), a headless recorder that stores chat in SQLite, and a web UI with dashboards, search and [7TV](https://7tv.app) emotes.

```sh
twitch-chat <channel>                            # terminal UI
twitch-chat record --db chat.db papaplatte xqc   # record channels
twitch-chat serve --db chat.db [--bind 0.0.0.0:8080]  # web UI, default 127.0.0.1:8080
twitch-chat login [--token <path>]               # log in for sending messages
```

## Terminal UI

`q`/`Esc`/`Ctrl+C` quit · `↑↓`/`jk` scroll · `PgUp`/`PgDn` page · `Home`/`g`, `End`/`G` top/bottom · `/` filter · `m`/`s` highlight mods/subs.

## Recording

Stores messages, events (subs, cheers, raids, …), moderation (bans, timeouts, deletions) and, with API credentials, per-minute viewer counts. Schema: `src/store.rs` (tables `messages`, `events`, `moderation`, `stream_samples`, FTS5 index `messages_fts`; timestamps in Unix ms).

- Channels can be added/removed live from the web UI; CLI channels are always recorded.
- Reconnects automatically; messages sent while disconnected are lost.
- Bans/timeouts mark the user's messages from the prior 10 minutes as deleted.

## Web UI

Channel dashboards, full-text search (prefix match), message context, user histories, channel data deletion. Can run alongside the recorder.

Listens on localhost by default. When exposing it, set `TWITCH_CHAT_PASSWORD` and use a TLS proxy (or an SSH tunnel).

## Twitch API credentials (optional)

Put `TWITCH_CLIENT_ID` and `TWITCH_CLIENT_SECRET` from a [Twitch app](https://dev.twitch.tv/console/apps) in `.env` (see `.env.example`). Enables stream status, viewer sampling, profile pictures and badges.

## Sending messages

Log in via **Log in to Twitch** on the Channels page (device code at twitch.tv/activate) or `twitch-chat login`. Then send from a channel page or:

```sh
curl -X POST localhost:8080/api/channels/<channel>/messages \
     -H 'Content-Type: application/json' -d '{"message": "hi", "reply_to": "<msg_id>"}'
```

Account API: `GET|DELETE /api/chat-account`, `POST /api/chat-account/login`. Anyone who can reach the server can post as you — keep it private.

## Docker

```sh
docker compose up -d --build   # recorder + web UI on 127.0.0.1:8080, data in volume chat-data
docker compose cp web:/data ./chat-data   # export DB (include chat.db-wal)
```
