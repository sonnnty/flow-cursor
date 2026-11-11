#!/usr/bin/env bash
# Removes the per-user FlowCursor agent installed by install.sh.
# Add --purge to also delete its identity and state.
set -euo pipefail
systemctl --user disable --now flow-agent.service 2>/dev/null || true
rm -f "$HOME/.config/systemd/user/flow-agent.service" "$HOME/.local/bin/flow-agent"
systemctl --user daemon-reload || true
if [ "${1:-}" = "--purge" ]; then
  rm -rf "$HOME/.local/share/flowcursor" "$HOME/.config/flowcursor"
  echo "Removed agent, configuration and identity."
else
  echo "Removed agent (configuration and identity kept; use --purge to delete them)."
fi
