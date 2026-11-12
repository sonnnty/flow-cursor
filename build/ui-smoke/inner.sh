#!/usr/bin/env bash
# Runs inside the smoke-test container (see build/ui-smoke.sh): a hub, two
# agents with the simulated backend, the tray against a StatusNotifierWatcher
# stub on a private session bus, status-window screenshots under Xvfb, and
# sni-checks.sh (re-registration, restart after an update, away not an error).
# Binaries in /bin-in, results in /out.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends xvfb libgl1-mesa-dri libegl1 libgl1 libxkbcommon-x11-0 \
  libxcursor1 libxrandr2 libxi6 dbus python3-dbus python3-gi fonts-dejavu-core fonts-thai-tlwg-ttf \
  ${WITH_WESTON:+weston} >/dev/null
B=/bin-in
OUT=/out
mkdir -p "$OUT"
: > "$OUT/smoke.log"
log() { echo "== $*" | tee -a "$OUT/smoke.log"; }

export XDG_RUNTIME_DIR=/tmp/xdg && mkdir -p -m 700 $XDG_RUNTIME_DIR
export DBUS_SESSION_BUS_ADDRESS="$(dbus-daemon --session --fork --print-address)"
python3 /smoke/sni-watcher.py "$OUT/sni.log" &
sleep 1

# --- hub ------------------------------------------------------------------
export FLOW_NO_MDNS=1
$B/flow-server --data /tmp/hub --quic 127.0.0.1:7878 --http 127.0.0.1:7880 > "$OUT/hub.log" 2>&1 &
sleep 1.5
TOKEN=$($B/flow-server --data /tmp/hub --print-token)
log "hub up, token ${TOKEN:0:12}…"

# --- a second machine ("pc") -----------------------------------------------
cat > /tmp/pc.toml <<TOML
hub = "127.0.0.1:7878"
backend = "sim"
state_dir = "/tmp/pc-state"
TOML
FLOW_AGENT_IPC=/tmp/pc.sock $B/flow-agent --config /tmp/pc.toml join "$TOKEN" >/dev/null
FLOW_AGENT_IPC=/tmp/pc.sock $B/flow-agent --config /tmp/pc.toml run > "$OUT/agent-pc.log" 2>&1 &

# --- this machine: not joined yet (first run) --------------------------------
mkdir -p ~/.config/flowcursor
printf 'backend = "sim"\np2p_port = 7979\n' > ~/.config/flowcursor/agent.toml
$B/flow-agent run > "$OUT/agent.log" 2>&1 &
sleep 1.5
log "flow-agent status (not joined):"; $B/flow-agent status | tee -a "$OUT/smoke.log"

export DISPLAY=:99
Xvfb :99 -screen 0 1280x1024x24 >/dev/null 2>&1 &
sleep 1
$B/flow-tray > "$OUT/tray.log" 2>&1 &
sleep 2
log "tray process: $(pgrep -c flow-tray) running (first-run window auto-opened: $(pgrep -fc -- '--window' || true))"
pkill -f -- 'flow-tray --window' || true
while pgrep -f -- 'flow-tray --window' >/dev/null; do sleep 0.2; done
FLOW_TRAY_SCREENSHOT="$OUT/status-window-first-run.png" timeout 30 $B/flow-tray --window 2>>"$OUT/tray.log" || log "window exit $?"

# --- join through the agent's IPC (what the window's Join button does) -----
log "join via IPC:"; $B/flow-agent join "$TOKEN" --hub 127.0.0.1 | tee -a "$OUT/smoke.log"
sleep 4
$B/flow-agent status | tee -a "$OUT/smoke.log"
FLOW_TRAY_SCREENSHOT="$OUT/status-window.png" timeout 30 $B/flow-tray --window 2>>"$OUT/tray.log" || log "window exit $?"

# --- pause: the tray icon and menu follow ---------------------------------
log "pause:"; $B/flow-agent pause | tee -a "$OUT/smoke.log"
sleep 3
FLOW_TRAY_SCREENSHOT="$OUT/status-window-paused.png" timeout 30 $B/flow-tray --window 2>>"$OUT/tray.log" || log "window exit $?"
$B/flow-agent resume >/dev/null

# --- second tray instance refused, --quit ends the first ------------------
log "second tray: $($B/flow-tray 2>&1 || true)"
$B/flow-tray --quit; sleep 1
log "tray processes after --quit: $(pgrep -c flow-tray || true)"

# --- tray icon registration: watcher restart, dropped item, self-update restart,
# away is not an error (own bus, stand-in watcher and agent)
log "tray registration checks:"
bash /smoke/sni-checks.sh "$B" "$OUT" || { log "sni checks failed"; exit 1; }

if [ -n "${WITH_WESTON:-}" ]; then
  log "Wayland (weston headless):"
  weston --backend=headless-backend.so --socket=wayland-1 --idle-time=0 > "$OUT/weston.log" 2>&1 &
  sleep 2
  WAYLAND_DISPLAY=wayland-1 DISPLAY= LIBGL_ALWAYS_SOFTWARE=1 FLOW_TRAY_SCREENSHOT="$OUT/status-window-wayland.png" \
    timeout 30 $B/flow-tray --window 2>>"$OUT/tray-wayland.log" || log "wayland window exit $?"
fi
log "done"
