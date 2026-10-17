#!/usr/bin/env bash
#
# twitch-chat-cli installer
#
# Usage:
#   curl -fsSL https://bankosegger.me/twitch-chat-cli-install | bash
#
# Downloads the latest release binary for this OS/architecture from
# https://github.com/bankosegger/twitch-chat-cli and installs it to
# ~/.local/bin/twitch-chat.

set -euo pipefail

REPO="bankosegger/twitch-chat-cli"
BIN_NAME="twitch-chat"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

case "$(uname -s)" in
  Linux)  OS="linux" ;;
  Darwin) OS="macos" ;;
  *) echo "Error: unsupported OS '$(uname -s)'." >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64)  ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) echo "Error: unsupported architecture '$(uname -m)'." >&2; exit 1 ;;
esac

ASSET="${BIN_NAME}-${OS}-${ARCH}"

echo "Fetching latest release of $REPO for $OS/$ARCH..."

URLS=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep -o "\"browser_download_url\": *\"[^\"]*\"" \
  | grep -o 'https://[^"]*')

URL=$(echo "$URLS" | grep "/${ASSET}\$" | head -1 || true)

# Releases up to v0.1.0 shipped a single Linux x86_64 binary named plainly.
if [ -z "$URL" ] && [ "$OS/$ARCH" = "linux/x86_64" ]; then
  URL=$(echo "$URLS" | grep "/${BIN_NAME}\$" | head -1 || true)
fi

if [ -z "$URL" ]; then
  echo "Error: the latest release has no binary for $OS/$ARCH (expected '$ASSET')." >&2
  echo "Available assets:" >&2
  echo "$URLS" | sed 's|.*/|  |' >&2
  echo "You can build from source instead: cargo install --git https://github.com/$REPO" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
curl -fsSL "$URL" -o "$INSTALL_DIR/$BIN_NAME"
chmod +x "$INSTALL_DIR/$BIN_NAME"

echo "Installed $BIN_NAME to $INSTALL_DIR/$BIN_NAME"
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) echo "Add it to your PATH: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac

echo "Run it with: $BIN_NAME"
