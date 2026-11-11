#!/usr/bin/env bash
# FlowCursor installer for Linux (Zorin OS / Ubuntu / Debian, GNOME).
# Installs into your home directory, no root needed:
#   ./install.sh <JOIN_TOKEN> [HUB_ADDRESS]
# Without HUB_ADDRESS the agent finds the hub with mDNS.
#   agent:  ~/.local/bin/flow-agent, systemd user service flow-agent
#   tray:   ~/.local/bin/flow-tray, started at login (~/.config/autostart),
#           menu entry "FlowCursor" (~/.local/share/applications), icons
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
token="${1:-}"
hub="${2:-}"

bindir="$HOME/.local/bin"
unitdir="$HOME/.config/systemd/user"
appdir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
icondir="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
autostartdir="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"

# A running tray keeps the old binary busy; it is started again below.
[ -x "$bindir/flow-tray" ] && "$bindir/flow-tray" --quit 2>/dev/null || true
install -Dm755 "$here/flow-agent" "$bindir/flow-agent"
install -Dm755 "$here/flow-tray" "$bindir/flow-tray"
mkdir -p "$unitdir" "$appdir" "$autostartdir"
sed "s|@BINDIR@|$bindir|" "$here/flow-agent.service" > "$unitdir/flow-agent.service"
sed "s|@BINDIR@|$bindir|" "$here/flowcursor.desktop" > "$appdir/flowcursor.desktop"
sed "s|@BINDIR@|$bindir|" "$here/flowcursor-tray.desktop" > "$autostartdir/flowcursor-tray.desktop"
for png in "$here"/icons/flowcursor-*.png; do
  size="$(basename "$png" .png)"; size="${size#flowcursor-}"
  install -Dm644 "$png" "$icondir/${size}x${size}/apps/flowcursor.png"
done
install -Dm644 "$here/icons/flowcursor.svg" "$icondir/scalable/apps/flowcursor.svg"
gtk-update-icon-cache -q -t "$icondir" 2>/dev/null || true

if [ -n "$token" ]; then
  if [ -n "$hub" ]; then
    "$bindir/flow-agent" join "$token" --hub "$hub"
  else
    "$bindir/flow-agent" join "$token"
  fi
else
  echo "No join token given; enter it in the FlowCursor window or run '$bindir/flow-agent join <TOKEN> [--hub ADDRESS]'."
fi

systemctl --user daemon-reload
systemctl --user enable flow-agent.service
systemctl --user restart flow-agent.service
# The tray icon now (later: at every login through the autostart entry).
if [ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]; then
  setsid -f "$bindir/flow-tray" >/dev/null 2>&1 < /dev/null || true
fi
echo
echo "FlowCursor installed and started (systemctl --user status flow-agent)."
echo "The tray icon needs the 'AppIndicator and KStatusNotifierItem Support' GNOME extension"
echo "(enabled by default on Zorin OS and Ubuntu). The window: menu entry 'FlowCursor' or flow-tray --show."
echo "On GNOME/Wayland two permission dialogs appear on first start:"
echo "  * 'Remote desktop' (lets other machines control this one) — allowed once, remembered;"
echo "  * 'Input capture' (lets this machine control others)."
echo "Logs: journalctl --user -u flow-agent -f"
