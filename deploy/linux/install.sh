#!/usr/bin/env bash
# FlowCursor agent installer for Linux (Zorin OS / Ubuntu / Debian, GNOME).
# Installs into your home directory, no root needed:
#   ./install.sh <JOIN_TOKEN> [HUB_ADDRESS]
# Without HUB_ADDRESS the agent finds the hub with mDNS.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
token="${1:-}"
hub="${2:-}"

bindir="$HOME/.local/bin"
unitdir="$HOME/.config/systemd/user"
install -Dm755 "$here/flow-agent" "$bindir/flow-agent"
mkdir -p "$unitdir"
sed "s|@BINDIR@|$bindir|" "$here/flow-agent.service" > "$unitdir/flow-agent.service"

if [ -n "$token" ]; then
  if [ -n "$hub" ]; then
    "$bindir/flow-agent" join "$token" --hub "$hub"
  else
    "$bindir/flow-agent" join "$token"
  fi
else
  echo "No join token given; run '$bindir/flow-agent join <TOKEN> [--hub ADDRESS]' later."
fi

systemctl --user daemon-reload
systemctl --user enable flow-agent.service
systemctl --user restart flow-agent.service
echo
echo "FlowCursor agent installed and started (systemctl --user status flow-agent)."
echo "On GNOME/Wayland two permission dialogs appear on first start:"
echo "  * 'Remote desktop' (lets other machines control this one) — allowed once, remembered;"
echo "  * 'Input capture' (lets this machine control others)."
echo "Logs: journalctl --user -u flow-agent -f"
