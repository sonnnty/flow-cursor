#!/usr/bin/env bash
# The tray against a stand-in StatusNotifierWatcher and a stand-in agent, on
# a private session bus (what GNOME's AppIndicator extension does, as far as
# it matters here):
#   - it registers, shows the state's own icon status (away is not an error)
#   - it registers again after the watcher restarted
#   - it registers again when the watcher silently dropped the item
#   - after a self-update restart the new process (new pid) is registered
#     and the old one is gone
# usage: sni-checks.sh BIN_DIR OUT_DIR   (BIN_DIR has flow-tray; needs
# dbus-daemon and python3 with dbus and gi). Exit status 1 on a failed check.
set -uo pipefail
B="$(realpath "$1")"
OUT="$(realpath -m "$2")"
HERE="$(dirname "$(realpath "$0")")"
mkdir -p "$OUT"
W="$(mktemp -d)"
LOG="$OUT/sni-checks.log"
: > "$LOG"
say() { echo "== $*" | tee -a "$LOG"; }
FAILED=0
check() { # check DESCRIPTION COMMAND...   (COMMAND may be a function below)
  local d="$1"; shift
  if "$@"; then say "ok: $d"; else say "FAILED: $d"; FAILED=1; fi
}
wait_for() { # wait_for SECONDS COMMAND...
  local n=$(( $1 * 10 )); shift
  while [ "$n" -gt 0 ]; do "$@" && return 0; sleep 0.1; n=$((n - 1)); done
  return 1
}
eq() { [ "$1" = "$2" ]; }
ne() { [ "$1" != "$2" ]; }

export XDG_RUNTIME_DIR="$W/xdg" HOME="$W/home"
mkdir -p -m 700 "$XDG_RUNTIME_DIR" "$HOME" "$W/ctl"
unset DISPLAY WAYLAND_DISPLAY
BUS_OUT="$(dbus-daemon --session --fork --print-address=1 --print-pid=2 2>"$W/bus.pid")"
export DBUS_SESSION_BUS_ADDRESS="$BUS_OUT"
BUS_PID="$(cat "$W/bus.pid")"
export FLOW_AGENT_IPC="$W/agent.sock"
export FLOW_TRAY_SNI_CHECK_SECS=2
cleanup() {
  pkill -f "$W/" 2>/dev/null
  [ -f "$XDG_RUNTIME_DIR/flowcursor/tray.lock" ] && kill "$(cat "$XDG_RUNTIME_DIR/flowcursor/tray.lock")" 2>/dev/null
  kill "$BUS_PID" 2>/dev/null
  rm -rf "$W"
}
trap cleanup EXIT

start_watcher() { python3 "$HERE/sni-watcher.py" "$OUT/sni-checks-watcher.log" & WATCHER=$!; }
watcher_up() { dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus \
  org.freedesktop.DBus.NameHasOwner string:org.kde.StatusNotifierWatcher 2>/dev/null | grep -q 'boolean true'; }
watcher_down() { ! watcher_up; }
# The item names the watcher lists.
items() { dbus-send --session --print-reply --dest=org.kde.StatusNotifierWatcher /StatusNotifierWatcher \
  org.freedesktop.DBus.Properties.Get string:org.kde.StatusNotifierWatcher string:RegisteredStatusNotifierItems \
  2>/dev/null | grep -o 'StatusNotifierItem-[0-9]*-[0-9]*'; }
items_line() { items | tr '\n' ' '; }
no_items() { [ -z "$(items)" ]; }
tray_pid() { cat "$XDG_RUNTIME_DIR/flowcursor/tray.lock" 2>/dev/null; }
have_tray_pid() { [ -n "$(tray_pid)" ]; }
tray_pid_is() { [ "$(tray_pid)" = "$1" ]; }
tray_pid_not() { [ -n "$(tray_pid)" ] && [ "$(tray_pid)" != "$1" ]; }
dead() { ! kill -0 "$1" 2>/dev/null; }
listed() { items | grep -q "StatusNotifierItem-$1-"; }
not_listed() { ! listed "$1"; }
n_listed() { eq "$(items | grep -c "StatusNotifierItem-$1-")" "$2"; }
# What the host shows right now: the last Status / ToolTip the watcher logged.
shown_is() { eq "$(grep -o 'Status: [A-Za-z]*' "$OUT/sni-checks-watcher.log" | tail -1)" "Status: $1"; }
tip_has() { grep '  ToolTip: ' "$OUT/sni-checks-watcher.log" | tail -1 | grep -q "$1"; }
has_socket() { [ -S "$1" ]; }
one_tray_process() { eq "$(pgrep -cf "$B/flow-tray$")" 1; }

: > "$OUT/sni-checks-watcher.log"
start_watcher
python3 "$HERE/fake-agent.py" "$FLOW_AGENT_IPC" "$W/ctl" &
wait_for 5 watcher_up && wait_for 5 has_socket "$FLOW_AGENT_IPC" || { say "FAILED: bus or agent did not start"; exit 1; }

"$B/flow-tray" 2> "$OUT/sni-checks-tray.log" &
wait_for 10 have_tray_pid
PID1="$(tray_pid)"
say "tray pid $PID1"
check "the item is registered" wait_for 10 listed "$PID1"
check "idle: the host shows it as Active" wait_for 5 shown_is Active

say "away: the session is closed on purpose, not an error"
touch "$W/ctl/away"
check "the tooltip says away" wait_for 6 tip_has away
sleep 1
check "away is not NeedsAttention" shown_is Active
rm "$W/ctl/away"
say "a refused permission dialog is an error"
touch "$W/ctl/refused"
check "refused shows NeedsAttention" wait_for 6 shown_is NeedsAttention
rm "$W/ctl/refused"
check "back to Active" wait_for 6 shown_is Active

say "the watcher restarts"
kill "$WATCHER"; wait "$WATCHER" 2>/dev/null
wait_for 5 watcher_down
start_watcher
check "the item is listed again" wait_for 15 listed "$PID1"
check "exactly once" n_listed "$PID1" 1

# The tray looks at the list 5 s after the watcher came back; what follows
# is the periodic check alone.
sleep 7
say "the watcher silently drops the item"
BEFORE="$(items_line)"
dbus-send --session --type=method_call --print-reply --dest=org.kde.StatusNotifierWatcher /StatusNotifierWatcher org.flowcursor.Test.Forget >/dev/null
check "dropped" no_items
check "the tray registered it again" wait_for 15 listed "$PID1"
check "under a new name (a new connection, not the dropped one)" ne "$(items_line)" "$BEFORE"
check "exactly once" n_listed "$PID1" 1
check "still the same process" tray_pid_is "$PID1"

say "self-update: the tray restarts"
touch "$W/ctl/update"
check "a new process holds the lock" wait_for 15 tray_pid_not "$PID1"
PID2="$(tray_pid)"
say "new tray pid $PID2"
check "the old process exited" wait_for 5 dead "$PID1"
check "the new process is registered" wait_for 15 listed "$PID2"
check "the old item is gone" wait_for 5 not_listed "$PID1"
check "exactly one item" eq "$(items | wc -l)" 1
check "the host shows it" wait_for 5 shown_is Active
sleep 5
check "no restart loop" tray_pid_is "$PID2"
check "no second tray process" one_tray_process

say "tray log:"; sed 's/^/   /' "$OUT/sni-checks-tray.log" | tee -a "$LOG"
if [ "$FAILED" = 0 ]; then say "all checks passed"; else say "SOME CHECKS FAILED"; fi
exit "$FAILED"
