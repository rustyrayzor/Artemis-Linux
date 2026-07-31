#!/bin/sh
set -eu

BIN_DIR=${XDG_BIN_HOME:-"$HOME/.local/bin"}
DATA_DIR=${XDG_DATA_HOME:-"$HOME/.local/share"}
STATE_DIR=${XDG_STATE_HOME:-"$HOME/.local/state"}/artemis-linux/install
BACKUP_ROOT="$STATE_DIR/backups"
DESKTOP_DIR="$DATA_DIR/applications"
ICON_DIR="$DATA_DIR/icons/hicolor/scalable/apps"

if [ ! -f "$STATE_DIR/last-backup" ]; then
    echo "No Artemis rollback snapshot is recorded." >&2
    exit 1
fi
backup=$(sed -n '1p' "$STATE_DIR/last-backup")
case "$backup" in
    "$BACKUP_ROOT"/*) ;;
    *) echo "Refusing rollback from an invalid snapshot path." >&2; exit 1 ;;
esac
if [ ! -d "$backup" ]; then
    echo "Rollback snapshot does not exist: $backup" >&2
    exit 1
fi

restore_file() {
    backup_name=$1
    target=$2
    if [ -f "$backup/$backup_name" ]; then
        install -D -m"$3" "$backup/$backup_name" "$target"
    elif [ -f "$backup/$backup_name.absent" ]; then
        rm -f -- "$target"
    else
        echo "Snapshot entry is missing: $backup_name" >&2
        exit 1
    fi
}

restore_file artemis-linux "$BIN_DIR/artemis-linux" 755
restore_file artemis-linux.desktop "$DESKTOP_DIR/artemis-linux.desktop" 644
restore_file artemis-linux.svg "$ICON_DIR/artemis-linux.svg" 644
command -v update-desktop-database >/dev/null 2>&1 && \
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
echo "Restored Artemis Linux from $backup"
