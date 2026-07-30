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

# Hintergrunddienst: sichert von Claude rotierte Tokens laufend ins aktive Profil.
# Ohne ihn veraltet der gespeicherte Snapshot und ein spaeterer Wechsel endet in "Login expired".
systemd_user_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
service_unit="$systemd_user_dir/claude-account-sync.service"
service_installed=0

if [[ -f "$repo_dir/systemd/claude-account-sync.service" ]]; then
  mkdir -p -- "$systemd_user_dir"
  service_temporary="$(mktemp "${service_unit}.XXXXXX")"
  sed "s|__BINARY__|$binary|g" "$repo_dir/systemd/claude-account-sync.service" >"$service_temporary"
  chmod 644 "$service_temporary"
  mv -f -- "$service_temporary" "$service_unit"

  if systemctl --user daemon-reload >/dev/null 2>&1 &&
    systemctl --user enable --now claude-account-sync.service >/dev/null 2>&1; then
    service_installed=1
  else
    printf 'Warnung: Hintergrunddienst konnte nicht gestartet werden (kein systemd-User-Bus?).\n' >&2
    printf '         Unit liegt unter %s; spaeter starten mit:\n' "$service_unit" >&2
    printf '         systemctl --user daemon-reload && systemctl --user enable --now claude-account-sync.service\n' >&2
  fi
fi

printf '\nInstalliert:\n  %s\n  %s\n' "$desktop_launcher" "$application_launcher"
if [[ "$service_installed" -eq 1 ]]; then
  printf '  %s (laeuft)\n' "$service_unit"
fi
printf '\nJetzt "Claude Account Swapper" doppelklicken.\n'
