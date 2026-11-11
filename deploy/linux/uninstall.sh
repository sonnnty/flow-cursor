#!/usr/bin/env bash
# Removes the per-user FlowCursor agent and tray installed by install.sh.
# Add --purge to also delete its identity and state.
set -euo pipefail
bindir="$HOME/.local/bin"
icondir="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
[ -x "$bindir/flow-tray" ] && "$bindir/flow-tray" --quit 2>/dev/null || true
systemctl --user disable --now flow-agent.service 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/flow-agent.service" "$bindir/flow-agent" "$bindir/flow-tray"
rm -f "${XDG_DATA_HOME:-$HOME/.local/share}/applications/flowcursor.desktop"
rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/flowcursor-tray.desktop"
rm -f "$icondir"/*/apps/flowcursor.png "$icondir/scalable/apps/flowcursor.svg"
gtk-update-icon-cache -q -t "$icondir" 2>/dev/null || true
systemctl --user daemon-reload || true
if [ "${1:-}" = "--purge" ]; then
  rm -rf "$HOME/.local/share/flowcursor" "$HOME/.config/flowcursor"
  echo "Removed agent, tray, configuration and identity."
else
  echo "Removed agent and tray (configuration and identity kept; use --purge to delete them)."
fi
