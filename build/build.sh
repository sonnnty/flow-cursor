#!/usr/bin/env bash
# Builds every FlowCursor artifact with Docker only (no local Rust needed):
#   build/out/linux/    flow-agent, flow-tray (x86_64, glibc >= 2.35), .deb, install.sh,
#                       systemd user unit, desktop entries, icons
#   build/out/windows/  FlowCursor-setup-<version>.exe (NSIS), flow-agent.exe, flow-tray.exe,
#                       install.ps1 / uninstall.ps1 (scripted / elevated installs)
#   build/out/server/   flow-server binary + compose.yaml
#   build/out/hub/      what goes into the hub image: agent-builds/ (manifest.json +
#                       linux-x86_64/ + windows-x86_64/, offered to agents for self-update)
#   build/out/tools/    flow-bench
# and the hub image flow-cursor-server:$TAG, which carries the agent builds of
# this same build: agents that connect to it update themselves to them.
#
# Every binary gets the build id FLOW_BUILD_ID (default: <commit count>.g<commit>,
# plus .dirty.<UTC time> for uncommitted changes; see crates/version).
#
# Usage: build/build.sh [--no-image]      (TAG defaults to flowcursor-mvp)
set -euo pipefail
cd "$(dirname "$0")/.."
TAG="${FLOW_TAG:-flowcursor-mvp}"
BUILDER="flow-cursor-builder:$TAG"
IMAGE="flow-cursor-server:$TAG"
BUILD_IMAGE=1
[ "${1:-}" = "--no-image" ] && BUILD_IMAGE=0

if [ -z "${FLOW_BUILD_ID:-}" ]; then
  if git rev-parse --git-dir >/dev/null 2>&1; then
    FLOW_BUILD_ID="$(git rev-list --count HEAD).g$(git rev-parse --short=10 HEAD)"
    FLOW_BUILD_COMMIT="$(git rev-parse HEAD)"
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
      FLOW_BUILD_ID="$FLOW_BUILD_ID.dirty.$(date -u +%Y%m%d%H%M%S)"
    fi
  else
    FLOW_BUILD_ID="local.$(date -u +%Y%m%d%H%M%S)"
  fi
fi
export FLOW_BUILD_ID FLOW_BUILD_COMMIT="${FLOW_BUILD_COMMIT:-}"
echo "==> build id $FLOW_BUILD_ID"

echo "==> builder image $BUILDER"
docker build -q -t "$BUILDER" -f build/Dockerfile.build build >/dev/null

# Rootless Docker maps container root to the calling user; with rootful
# Docker run as the calling user so the outputs are not owned by root.
USER_ARGS=()
if ! docker info -f '{{.SecurityOptions}}' 2>/dev/null | grep -q rootless; then
  USER_ARGS=(-u "$(id -u):$(id -g)" -e HOME=/tmp)
fi
mkdir -p target/docker-cargo
docker run --rm "${USER_ARGS[@]}" \
  -v "$PWD":/src \
  -v "$PWD/target/docker-cargo":/cargo-home \
  -e CARGO_HOME=/cargo-home \
  -e RUSTUP_HOME=/opt/rustup \
  -e PATH=/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  -e FLOW_BUILD_ID -e FLOW_BUILD_COMMIT \
  "$BUILDER" bash build/package.sh

if [ "$BUILD_IMAGE" = 1 ]; then
  echo "==> hub image $IMAGE (with the agent builds)"
  # Context: build/out (the binaries built above, not a second compile).
  DOCKER_BUILDKIT=1 docker build -t "$IMAGE" -f build/Dockerfile.hub build/out
  echo "    image size: $(docker image inspect -f '{{.Size}}' "$IMAGE" | awk '{printf "%.1f MB", $1/1e6}')," \
    "agent builds: $(du -sh build/out/hub/agent-builds | cut -f1)"
fi
echo "==> done: build/out/"
