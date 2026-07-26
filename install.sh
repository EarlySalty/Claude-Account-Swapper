#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
binary="$cargo_home/bin/claude-account"
applications_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

if ! command -v cargo >/dev/null 2>&1; then
  printf 'Fehler: Rust/Cargo fehlt. Installation: https://rustup.rs\n' >&2
  exit 1
fi

cargo install --path "$repo_dir" --locked --force

if [[ ! -x "$binary" ]]; then
  printf 'Fehler: Binary wurde nicht gefunden: %s\n' "$binary" >&2
  exit 1
fi

desktop_dir="$HOME/Desktop"
if command -v xdg-user-dir >/dev/null 2>&1; then
  detected_desktop="$(xdg-user-dir DESKTOP 2>/dev/null || true)"
  if [[ -n "$detected_desktop" ]]; then
    desktop_dir="$detected_desktop"
  fi
fi

mkdir -p -- "$desktop_dir" "$applications_dir"

write_launcher() {
  local target="$1"
  local mode="$2"
  local temporary
  temporary="$(mktemp "${target}.XXXXXX")"
  cat >"$temporary" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=Claude Account Swapper
Comment=Claude Code Account speichern und wechseln
Exec=$binary
Terminal=true
Path=$HOME
Icon=system-users
Categories=Utility;
StartupNotify=false
EOF
  chmod "$mode" "$temporary"
  mv -f -- "$temporary" "$target"
}

desktop_launcher="$desktop_dir/Claude Account Swapper.desktop"
application_launcher="$applications_dir/claude-account-swapper.desktop"
write_launcher "$desktop_launcher" 755
write_launcher "$application_launcher" 644

if command -v gio >/dev/null 2>&1; then
  gio set "$desktop_launcher" metadata::trusted true >/dev/null 2>&1 || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$applications_dir" >/dev/null 2>&1 || true
fi

printf '\nInstalliert:\n  %s\n  %s\n\nJetzt "Claude Account Swapper" doppelklicken.\n' \
  "$desktop_launcher" "$application_launcher"
