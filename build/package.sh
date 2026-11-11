#!/usr/bin/env bash
# Runs inside the build container (see build.sh): compiles everything and
# lays out install-ready artifacts in build/out/.
set -euo pipefail
cd /src
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
OUT=build/out
export CARGO_TARGET_DIR=/src/target/docker

echo "==> Linux x86_64 (glibc) build"
cargo build --release --locked -p flow-agent -p flow-tray -p flow-server -p flow-bench
echo "==> Windows x86_64 build (mingw)"
cargo build --release --locked --target x86_64-pc-windows-gnu -p flow-agent -p flow-tray

rm -rf "$OUT"
mkdir -p "$OUT/linux" "$OUT/windows" "$OUT/server" "$OUT/tools"
R=$CARGO_TARGET_DIR/release
W=$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release

# --- Linux agent: binary + installer + systemd user unit -------------------
install -m755 "$R/flow-agent" "$OUT/linux/flow-agent"
install -m755 deploy/linux/install.sh deploy/linux/uninstall.sh "$OUT/linux/"
install -m644 deploy/linux/flow-agent.service "$OUT/linux/flow-agent.service"

# --- Linux agent: .deb -------------------------------------------------------
PKG=$(mktemp -d)
install -Dm755 "$R/flow-agent" "$PKG/usr/bin/flow-agent"
install -d "$PKG/usr/lib/systemd/user"
sed "s|@BINDIR@|/usr/bin|" deploy/linux/flow-agent.service > "$PKG/usr/lib/systemd/user/flow-agent.service"
install -Dm644 README.md "$PKG/usr/share/doc/flow-agent/README.md"
install -d "$PKG/DEBIAN"
cat > "$PKG/DEBIAN/control" <<CTRL
Package: flow-agent
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Depends: libc6 (>= 2.35)
Recommends: xdg-desktop-portal, xdg-desktop-portal-gnome
Maintainer: FlowCursor <noreply@localhost>
Description: FlowCursor agent - share mouse, keyboard and clipboard over the LAN
 Connects to a FlowCursor hub; captures input with the InputCapture portal
 (Wayland) or XInput2 (X11) and injects it with the RemoteDesktop portal /
 libei or XTEST. Runs as a systemd user service.
CTRL
cat > "$PKG/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
echo "FlowCursor agent installed. As your normal user run:"
echo "  flow-agent join <TOKEN> [--hub ADDRESS]"
echo "  systemctl --user enable --now flow-agent"
POST
chmod 755 "$PKG/DEBIAN/postinst"
dpkg-deb --root-owner-group --build "$PKG" "$OUT/linux/flow-agent_${VERSION}_amd64.deb" >/dev/null
rm -rf "$PKG"

# --- Windows: setup exe (NSIS) and the plain exes + scripts ----------------
install -m755 "$W/flow-agent.exe" "$W/flow-tray.exe" "$OUT/windows/"
install -m644 deploy/windows/install.ps1 deploy/windows/uninstall.ps1 "$OUT/windows/"
makensis -V2 -DVERSION="$VERSION" -DBINDIR="$W" -DICON="$PWD/deploy/icons/flowcursor.ico" \
  -DOUTFILE="$PWD/$OUT/windows/FlowCursor-setup-$VERSION.exe" deploy/windows/flowcursor.nsi

# --- Hub binary and tools ----------------------------------------------------
install -m755 "$R/flow-server" "$OUT/server/flow-server"
install -m644 deploy/compose.yaml "$OUT/server/compose.yaml"
install -m755 "$R/flow-bench" "$OUT/tools/flow-bench"

( cd "$OUT" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS )
echo "==> artifacts"
ls -la "$OUT"/*
