# twitch-chat

A terminal-based Twitch chat client built in Rust with [ratatui](https://github.com/ratatui-org/ratatui).

Connects to a channel's chat over IRC and displays it in a scrollable terminal UI, with optional live stream status (title, category, viewer count) via the Twitch Helix API.

## Features

- Live chat via Twitch IRC (anonymous, read-only)
- Scrollable message history
- Message filtering (search-as-you-type)
- Highlight moderators / subscribers
- Optional stream status (online/offline, title, category, viewers) when API credentials are provided

## Usage

```sh
cargo run --release -- <channel>
```

`<channel>` is the channel name as it appears in the twitch.tv URL (with or without a leading `#`).

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
