#!/usr/bin/env bash
# Headless smoke test of the tray and the status window (Linux) against
# real agents with the simulated backend, in Docker:
#   build/ui-smoke.sh [BIN_DIR] [OUT_DIR]
# BIN_DIR holds flow-server, flow-agent, flow-tray (default: build/out/linux
# plus build/out/server). Writes screenshots and logs to OUT_DIR (default
# build/out/ui-smoke). BASE=ubuntu:22.04 (Zorin OS 17's base) by default;
# WITH_WESTON=1 also tries the window on a headless Weston (Wayland).
set -euo pipefail
cd "$(dirname "$0")/.."
BASE="${BASE:-ubuntu:22.04}"
OUT="$(realpath -m "${2:-build/out/ui-smoke}")"
if [ -n "${1:-}" ]; then
  BIN="$(realpath "$1")"
else
  BIN="$(mktemp -d)"
  cp build/out/linux/flow-agent build/out/linux/flow-tray build/out/server/flow-server "$BIN/"
fi
mkdir -p "$OUT"
docker run --rm --hostname zorin-laptop \
  -e WITH_WESTON="${WITH_WESTON:-}" \
  -v "$BIN":/bin-in:ro -v "$PWD/build/ui-smoke":/smoke:ro -v "$OUT":/out \
  "$BASE" bash /smoke/inner.sh
echo "==> results in $OUT"
ls -la "$OUT"
