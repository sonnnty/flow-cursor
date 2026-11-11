#!/usr/bin/env bash
# End-to-end self-update test on Linux, in Docker (no systemd, no desktop:
# the simulated input backend and a systemctl stand-in):
#   1. builds an "old" agent (FLOW_BUILD_ID=e2e.old) next to the current
#      build in build/out (from build/build.sh);
#   2. update: starts the hub image flow-cursor-server:$TAG (carrying the
#      current build), installs the old build with install.sh in an
#      ubuntu:22.04 container, joins, and waits until the agent replaced
#      itself with the hub's build and reports it current;
#   3. rollback: a hub whose Linux agent build is broken (a program that
#      exits at once; manifest regenerated, so hashes match): the old agent
#      installs it, the guard sees it never start, restores the old version
#      and restarts it, which reports the failure and does not retry.
# Usage: FLOW_TAG=<tag> build/e2e-update.sh [update|rollback|all]
set -euo pipefail
cd "$(dirname "$0")/.."
TAG="${FLOW_TAG:-flowcursor-mvp}"
IMAGE="flow-cursor-server:$TAG"
BUILDER="flow-cursor-builder:$TAG"
WHAT="${1:-all}"
OUT="$PWD/build/out"
MANIFEST="$OUT/hub/agent-builds/manifest.json"
[ -f "$MANIFEST" ] || { echo "run build/build.sh first"; exit 1; }
json() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)"; }
VERSION="$(json 'd["version"]' < "$MANIFEST")"
NEW="$VERSION+$(json 'd["build"]' < "$MANIFEST")"
OLD="$VERSION+e2e.old"

if [ ! -x "$OUT/e2e/old/flow-agent" ] || ! "$OUT/e2e/old/flow-agent" --version | grep -q "e2e.old"; then
  echo "==> building the old agent ($OLD)"
  USER_ARGS=()
  if ! docker info -f '{{.SecurityOptions}}' 2>/dev/null | grep -q rootless; then
    USER_ARGS=(-u "$(id -u):$(id -g)" -e HOME=/tmp)
  fi
  docker run --rm "${USER_ARGS[@]}" -v "$PWD":/src -v "$PWD/target/docker-cargo":/cargo-home \
    -e CARGO_HOME=/cargo-home -e RUSTUP_HOME=/opt/rustup -e CARGO_TARGET_DIR=/src/target/docker \
    -e PATH=/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    -e FLOW_BUILD_ID=e2e.old "$BUILDER" cargo build --release --locked -p flow-agent
  install -Dm755 target/docker/release/flow-agent "$OUT/e2e/old/flow-agent"
fi

NET="fc-e2e-$$"
CONTAINERS=()
cleanup() {
  for c in "${CONTAINERS[@]}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
docker network create "$NET" >/dev/null

# start_hub NAME [AGENT_BUILDS_DIR]: prints nothing, sets TOKEN and API.
start_hub() {
  local name="$1" vol=()
  [ -n "${2:-}" ] && vol=(-v "$2":/usr/share/flowcursor/agent-builds:ro)
  CONTAINERS+=("$name")
  docker run -d --name "$name" --network "$NET" -e FLOW_ADMIN_TOKEN=e2e -e FLOW_NO_MDNS=1 \
    -p 127.0.0.1::7880 "${vol[@]}" "$IMAGE" >/dev/null
  for _ in $(seq 50); do
    docker logs "$name" 2>&1 | grep -q "join token" && break
    sleep 0.2
  done
  TOKEN="$(docker exec "$name" /usr/local/bin/flow-server --print-token)"
  API="http://$(docker port "$name" 7880/tcp | head -1)/api/state"
}

node_of_hub() {
  python3 - "$API" <<'PY'
import json, sys, urllib.request
req = urllib.request.Request(sys.argv[1], headers={"Authorization": "Bearer e2e"})
d = json.load(urllib.request.urlopen(req))
n = d["nodes"][0]
u = n.get("update") or {}
f = u.get("failed") or {}
print(n["agent_version"], n["status"], u.get("state"), f.get("target", "-"), f.get("rolled_back", "-"), "|", f.get("reason", ""))
PY
}

client() {
  docker run --rm --network "$NET" --hostname e2e-laptop -e TOKEN="$TOKEN" -e HUB="$1:7878" -e MODE="$2" \
    -e OLD="$OLD" -e NEW="$NEW" -e TIMEOUT="${3:-150}" \
    -v "$OUT":/in:ro -v "$PWD/build/e2e-update":/e2e:ro ubuntu:22.04 bash /e2e/client.sh
}

if [ "$WHAT" = all ] || [ "$WHAT" = update ]; then
  echo "==> update: $OLD -> $NEW"
  start_hub "fc-e2e-hub-$$"
  client "fc-e2e-hub-$$" update
  got="$(node_of_hub)"; echo "    hub sees: $got"
  [[ "$got" == "$NEW online current"* ]] || { echo "!! hub does not see the new version as current"; exit 1; }
fi

if [ "$WHAT" = all ] || [ "$WHAT" = rollback ]; then
  echo "==> rollback: $OLD -> broken $NEW -> $OLD"
  BROKEN="$OUT/e2e/broken"
  rm -rf "$BROKEN" && mkdir -p "$BROKEN"
  cp -r "$OUT/hub/agent-builds/linux-x86_64" "$BROKEN/"
  printf '#!/bin/sh\necho "broken build" >&2\nexit 1\n' > "$BROKEN/linux-x86_64/flow-agent"
  docker run --rm -u 0 --entrypoint /usr/local/bin/flow-server -v "$BROKEN":/ab "$IMAGE" --write-agent-manifest /ab
  start_hub "fc-e2e-hub2-$$" "$BROKEN"
  client "fc-e2e-hub2-$$" rollback 200
  got="$(node_of_hub)"; echo "    hub sees: $got"
  [[ "$got" == "$OLD online available $NEW True"* ]] || { echo "!! hub does not show the rolled-back failure"; exit 1; }
fi
echo "==> e2e self-update: OK"
