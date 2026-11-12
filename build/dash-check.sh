#!/usr/bin/env bash
# Checks the hub dashboard layout in headless Chromium (Docker, Playwright
# image) with a mocked hub API: no page scroll and every Settings control
# visible at 1024x700, 1130x900, 1366x650 and 1920x900 (SIZES=WxH,...).
#   build/dash-check.sh [OUT_DIR]     screenshots go to OUT_DIR (build/out/dash-check)
# Needs network once to fetch playwright-core (cached in build/out/.pw).
set -euo pipefail
cd "$(dirname "$0")/.."
IMAGE="${PW_IMAGE:-mcr.microsoft.com/playwright:v1.64.1-noble}"
OUT="$(realpath -m "${1:-build/out/dash-check}")"
CACHE="$(realpath -m build/out/.pw)"
mkdir -p "$OUT" "$CACHE"
docker run --rm -e SIZES="${SIZES:-}" \
  -v "$PWD/server/static":/static:ro -v "$PWD/build/dash-check":/check:ro -v "$OUT":/out -v "$CACHE":/pw \
  "$IMAGE" bash -c '
    set -e
    [ -d /pw/node_modules/playwright-core ] || npm install --silent --prefix /pw playwright-core@1.64.1 >/dev/null
    export CHROMIUM="$(ls -d /ms-playwright/chromium-*/chrome-linux*/chrome | head -1)"
    NODE_PATH=/pw/node_modules node /check/check.cjs /static/index.html /out'
