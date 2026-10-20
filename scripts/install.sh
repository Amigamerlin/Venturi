#!/usr/bin/env bash
set -euo pipefail

# Release CI replaces this value so downloaded installers stay on their release.
DEFAULT_VERSION=latest
RELEASES=https://github.com/VenturiVideo/Venturi/releases

fail() {
    echo "Error: $*" >&2
    exit 1
}

if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    cat <<'EOF'
Usage: bash install.sh [latest|vX.Y.Z]

Install the Linux AppImage and a menu entry for the current user.
Run again to replace the installed version. No sudo is needed.

Environment:
  INSTALL_DIR    Binary directory (default: $HOME/.local/bin)
  XDG_DATA_HOME  Desktop data directory (default: $HOME/.local/share)
EOF
    exit 0
fi
[[ $# -le 1 ]] || fail "Usage: bash install.sh [latest|vX.Y.Z]"
VERSION="${1:-$DEFAULT_VERSION}"
[[ $VERSION == latest || $VERSION =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] \
    || fail "Expected latest or a release tag such as v0.2.0."
[[ $(uname -s) == Linux ]] || fail "This installer supports Linux. Download the macOS DMG from $RELEASES."
ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|aarch64) ;;
    *) fail "Unsupported Linux architecture: $ARCH" ;;
esac
command -v curl >/dev/null || fail "Install curl and try again."

INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
for directory in "$INSTALL_DIR" "$DATA_DIR"; do
    [[ $directory == /* && $directory != *$'\n'* && $directory != *$'\r'* ]] \
        || fail "Install directories must be absolute paths without line breaks."
done

if [[ $VERSION == latest ]]; then
    URL="$RELEASES/latest/download/Venturi-$ARCH.AppImage"
else
    URL="$RELEASES/download/$VERSION/Venturi-$ARCH.AppImage"
fi

mkdir -p "$INSTALL_DIR" "$DATA_DIR/applications"
STAGING="$(mktemp -d "$INSTALL_DIR/.venturi-install.XXXXXX")"
DESKTOP_TMP=
cleanup() {
    rm -rf -- "$STAGING"
    if [[ -n $DESKTOP_TMP ]]; then rm -f -- "$DESKTOP_TMP"; fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

echo "Downloading Venturi ($VERSION, $ARCH)..."
curl --fail --location --show-error --silent --retry 3 \
    --proto '=https' --proto-redir '=https' \
    --output "$STAGING/venturi" "$URL"
[[ -s "$STAGING/venturi" ]] || fail "The downloaded AppImage is empty."
chmod 755 "$STAGING/venturi"
mv -fT -- "$STAGING/venturi" "$INSTALL_DIR/venturi"

# Desktop Exec values have their own escaping rules, separate from the shell.
EXEC_PATH="$INSTALL_DIR/venturi"
EXEC_PATH="${EXEC_PATH//\\/\\\\\\\\}"
EXEC_PATH="${EXEC_PATH//\"/\\\\\"}"
EXEC_PATH="${EXEC_PATH//\$/\\\\\$}"
EXEC_PATH="${EXEC_PATH//\`/\\\\\`}"
EXEC_PATH="${EXEC_PATH//%/%%}"
DESKTOP_TMP="$(mktemp "$DATA_DIR/applications/.venturi.XXXXXX")"
cat > "$DESKTOP_TMP" <<EOF
[Desktop Entry]
Type=Application
Name=Venturi
Comment=Video editor
Exec="$EXEC_PATH" %f
Icon=video-x-generic
Terminal=false
Categories=AudioVideo;Video;AudioVideoEditing;
EOF
chmod 644 "$DESKTOP_TMP"
mv -fT -- "$DESKTOP_TMP" "$DATA_DIR/applications/venturi.desktop"
if command -v update-desktop-database >/dev/null; then
    update-desktop-database "$DATA_DIR/applications" || true
fi

echo "Installed $INSTALL_DIR/venturi. Launch Venturi from your application menu."
case ":$PATH:" in
    *":$INSTALL_DIR:"*) echo "You can also run: venturi" ;;
    *) echo "Add $INSTALL_DIR to PATH to run venturi from the terminal." ;;
esac
