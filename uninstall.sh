#!/usr/bin/env bash
# Remove what install.sh put in place.
#   ./uninstall.sh            -> the app; your projects, threads and settings stay for a reinstall
#   ./uninstall.sh --purge    -> and remove them too (~/.config/tj and ~/.local/share/tj)
set -euo pipefail
APP_ID=dev.turbinebmw.TJ
BIN=~/.local/bin/tj
SHARE=~/.local/share

purge=false
for arg in "$@"; do
  case $arg in
    --purge) purge=true ;;
    -h|--help) sed -n '2,4p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $arg (try --help)" >&2; exit 2 ;;
  esac
done

if pgrep -x tj >/dev/null 2>&1; then
  echo "TJ is running; it keeps running until you close it."
fi

rm -f "$BIN"
rm -f "$SHARE/applications/$APP_ID.desktop"
for n in 16 32 48 64 128 256 512; do
  rm -f "$SHARE/icons/hicolor/${n}x${n}/apps/$APP_ID.png"
done
rm -f "$SHARE/icons/hicolor/scalable/apps/$APP_ID.svg"
gtk4-update-icon-cache -q -t -f "$SHARE/icons/hicolor" 2>/dev/null || true
update-desktop-database "$SHARE/applications" 2>/dev/null || true

if $purge; then
  # Claude's own sessions and logins (~/.claude, account folders) are Claude Code's, not TJ's: kept.
  rm -rf "${XDG_CONFIG_HOME:-$HOME/.config}/tj"    # accounts.toml, settings.toml
  rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/tj" # projects, threads, transcripts, images
  echo "Removed TJ's accounts list, settings, projects and threads."
else
  echo "Your projects, threads and settings are kept in ~/.local/share/tj and ~/.config/tj; --purge removes them."
fi
echo "uninstalled: $BIN"
