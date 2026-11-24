#!/usr/bin/env bash
# User-local install: binary, .desktop and icons. Root only to install missing system packages.
set -euo pipefail
cd "$(dirname "$0")"
APP_ID=dev.turbinebmw.TJ
# gpui draws with Vulkan and reads the keyboard through xkbcommon; git lists project files
# and changes.
deps=(vulkan-icd-loader libxkbcommon libxkbcommon-x11 libxcb git)
if command -v pacman >/dev/null; then
  missing=$(pacman -T "${deps[@]}" || true)
  if [ -n "$missing" ]; then
    echo "installing missing packages: $missing"
    sudo pacman -S --needed $missing
  fi
else
  echo "not Arch: make sure these (or your distro's equivalents) are installed: ${deps[*]}"
fi
if ! command -v cargo >/dev/null; then
  echo "cargo is needed to build TJ: install Rust (pacman -S rust, or rustup)" >&2
  exit 1
fi
cargo build --release
install -Dm755 target/release/tj ~/.local/bin/tj
# Launchers often lack ~/.local/bin on PATH, and .desktop files don't expand ~,
# so bake the absolute binary path into the installed copy.
mkdir -p ~/.local/share/applications
sed "s#^Exec=.*#Exec=$HOME/.local/bin/tj#" data/$APP_ID.desktop > ~/.local/share/applications/$APP_ID.desktop
chmod 644 ~/.local/share/applications/$APP_ID.desktop
for n in 16 32 48 64 128 256 512; do
  install -Dm644 data/icons/tj-$n.png ~/.local/share/icons/hicolor/${n}x${n}/apps/$APP_ID.png
done
# The scalable icon wraps the full-size art, so launchers that take an SVG over
# PNGs (Omarchy's does) draw it sharp at any size instead of an upscaled small PNG.
install -Dm644 data/icons/tj.svg ~/.local/share/icons/hicolor/scalable/apps/$APP_ID.svg
gtk4-update-icon-cache -q -t -f ~/.local/share/icons/hicolor 2>/dev/null || true
update-desktop-database ~/.local/share/applications 2>/dev/null || true
echo "installed: ~/.local/bin/tj"
# TJ drives Claude Code; it isn't a system package, so it's only checked for.
if ! command -v claude >/dev/null; then
  echo "note: the claude command (Claude Code) wasn't found; TJ needs it to run agents."
  echo "      Install it from https://docs.claude.com/en/docs/claude-code and run: claude auth login"
fi
