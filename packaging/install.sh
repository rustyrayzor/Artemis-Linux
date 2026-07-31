#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PACKAGE_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
BIN_DIR=${XDG_BIN_HOME:-"$HOME/.local/bin"}
DATA_DIR=${XDG_DATA_HOME:-"$HOME/.local/share"}
STATE_DIR=${XDG_STATE_HOME:-"$HOME/.local/state"}/artemis-linux/install
BACKUP_ROOT="$STATE_DIR/backups"
DESKTOP_DIR="$DATA_DIR/applications"
ICON_DIR="$DATA_DIR/icons/hicolor/scalable/apps"

if [ -n "${ARTEMIS_BINARY:-}" ]; then
    SOURCE_BINARY=$ARTEMIS_BINARY
elif [ -x "$PACKAGE_ROOT/bin/artemis-linux" ]; then
    SOURCE_BINARY="$PACKAGE_ROOT/bin/artemis-linux"
else
    SOURCE_BINARY="$PACKAGE_ROOT/target/release/artemis-linux"
fi

if [ ! -x "$SOURCE_BINARY" ]; then
    echo "Artemis binary not found or not executable: $SOURCE_BINARY" >&2
    echo "Build with 'cargo build --release --locked' or set ARTEMIS_BINARY." >&2
    exit 1
fi

timestamp=$(date -u +%Y%m%dT%H%M%SZ)
backup="$BACKUP_ROOT/$timestamp"
suffix=0
while [ -e "$backup" ]; do
    suffix=$((suffix + 1))
    backup="$BACKUP_ROOT/$timestamp-$suffix"
done
mkdir -p "$backup" "$BIN_DIR" "$DESKTOP_DIR" "$ICON_DIR"

backup_file() {
    source_path=$1
    backup_name=$2
    if [ -f "$source_path" ]; then
        cp -p "$source_path" "$backup/$backup_name"
    else
        : > "$backup/$backup_name.absent"
    fi
}

backup_file "$BIN_DIR/artemis-linux" artemis-linux
backup_file "$DESKTOP_DIR/artemis-linux.desktop" artemis-linux.desktop
backup_file "$ICON_DIR/artemis-linux.svg" artemis-linux.svg

install -m755 "$SOURCE_BINARY" "$BIN_DIR/artemis-linux"
install -m644 "$SCRIPT_DIR/artemis-linux.desktop" "$DESKTOP_DIR/artemis-linux.desktop"
install -m644 "$SCRIPT_DIR/artemis-linux.svg" "$ICON_DIR/artemis-linux.svg"
printf '%s\n' "$backup" > "$STATE_DIR/last-backup"

if [ "${ARTEMIS_SKIP_DESKTOP_DATABASE:-0}" != "1" ]; then
    command -v update-desktop-database >/dev/null 2>&1 && \
        update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
    command -v xdg-mime >/dev/null 2>&1 && \
        xdg-mime default artemis-linux.desktop x-scheme-handler/art || true
fi

echo "Installed Artemis Linux to $BIN_DIR/artemis-linux"
echo "Rollback snapshot: $backup"
