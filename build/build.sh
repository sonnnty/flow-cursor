#!/usr/bin/env bash
# Builds every FlowCursor artifact with Docker only (no local Rust needed):
#   build/out/linux/    flow-agent (x86_64, glibc >= 2.35), .deb, install.sh, systemd user unit
#   build/out/windows/  flow-agent.exe (x86_64), install.ps1 / uninstall.ps1
#   build/out/server/   flow-server binary + compose.yaml
#   build/out/tools/    flow-bench
# and the hub image flow-cursor-server:$TAG.
#
# Usage: build/build.sh [--no-image]      (TAG defaults to flowcursor-mvp)
set -euo pipefail
cd "$(dirname "$0")/.."
TAG="${FLOW_TAG:-flowcursor-mvp}"
BUILDER="flow-cursor-builder:$TAG"
IMAGE="flow-cursor-server:$TAG"
BUILD_IMAGE=1
[ "${1:-}" = "--no-image" ] && BUILD_IMAGE=0

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
  "$BUILDER" bash build/package.sh

if [ "$BUILD_IMAGE" = 1 ]; then
  echo "==> hub image $IMAGE"
  DOCKER_BUILDKIT=1 docker build -t "$IMAGE" -f server/Dockerfile .
fi
echo "==> done: build/out/"
