#!/bin/sh
set -eu

# amux one-command installer (macOS / Linux)
# curl -fsSL https://amux.cc/install.sh | sh

REPO="xiaoxiunique/amux"
BIN_DIR="$HOME/.local/bin"

# --- detect platform ---
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64)   TARGET="aarch64-apple-darwin" ;;
      x86_64)  TARGET="x86_64-apple-darwin" ;;
      *)       echo "unsupported arch: $ARCH" >&2; exit 1 ;;
    esac
    EXT="tar.gz"
    ;;
  Linux)
    case "$ARCH" in
      x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
      aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
      *)       echo "unsupported arch: $ARCH" >&2; exit 1 ;;
    esac
    EXT="tar.gz"
    ;;
  *)
    echo "unsupported OS: $OS (use the PowerShell installer on Windows)" >&2
    exit 1
    ;;
esac

# --- latest release ---
echo "==> fetching latest release…"
LATEST=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
  | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
VERSION="${LATEST#v}"
ASSET="amux-v${VERSION}-${TARGET}.${EXT}"
URL="https://github.com/$REPO/releases/download/$LATEST/$ASSET"

echo "==> downloading amux ${VERSION} (${TARGET})…"
TMP=$(mktemp -d)
curl -fsSL "$URL" -o "$TMP/$ASSET"

# --- extract ---
echo "==> installing to ${BIN_DIR}…"
mkdir -p "$BIN_DIR"
if [ "$EXT" = "tar.gz" ]; then
  tar -xzf "$TMP/$ASSET" -C "$TMP"
  # The tarball contains a single binary named 'amux'.
  cp "$TMP/amux" "$BIN_DIR/amux"
else
  # zip (Windows), not reached here but keep in case
  unzip -o "$TMP/$ASSET" -d "$TMP"
  cp "$TMP/amux.exe" "$BIN_DIR/amux.exe"
fi
chmod +x "$BIN_DIR/amux"
rm -rf "$TMP"

# --- ensure ~/.local/bin is on PATH ---
case "$(basename "${SHELL:-sh}")" in
  zsh) RC="$HOME/.zshrc" ;;
  bash) RC="$HOME/.bashrc" ;;
  *) RC="" ;;
esac
if [ -n "$RC" ] && ! grep -qF '$HOME/.local/bin' "$RC" 2>/dev/null; then
  echo "export PATH=\"\$HOME/.local/bin:\$PATH\"" >> "$RC"
  echo "Added ~/.local/bin to ${RC}; source it or restart your shell."
fi

# --- now let install-cli handle rmux + shell aliases + mux config ---
echo "==> running amux install-cli…"
"$BIN_DIR/amux" install-cli

echo
echo "Done. Run 'amux' to open the TUI, or 'cc' / 'cx' to start an agent."
