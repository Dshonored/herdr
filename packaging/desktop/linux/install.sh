#!/bin/sh
# Installs the herdr desktop app for the current user: the app goes to
# ~/.local/share/herdr-desktop and a launcher entry with the herdr icon is added,
# so herdr appears in the application menu. Re-run it to repair the entry.
set -eu

src="$(cd "$(dirname "$0")" && pwd)"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
dest="$data/herdr-desktop"
apps="$data/applications"
icons="$data/icons/hicolor/512x512/apps"

mkdir -p "$dest" "$apps" "$icons"
if [ "$src" != "$dest" ]; then
  cp -R "$src"/. "$dest"/
fi
chmod 755 "$dest/herdr" "$dest/install.sh"

cp "$dest/herdr-desktop.png" "$icons/herdr-desktop.png"
sed "s|@EXEC@|$dest/herdr|g" "$dest/herdr-desktop.desktop.in" > "$apps/herdr-desktop.desktop"
chmod 644 "$apps/herdr-desktop.desktop"
update-desktop-database "$apps" >/dev/null 2>&1 || true
gtk-update-icon-cache -q "$data/icons/hicolor" >/dev/null 2>&1 || true

echo "herdr is installed. Open it from your application menu, or run:"
echo "  \"$dest/herdr\" gui"
