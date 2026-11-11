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

# --- Linux: binaries + per-user installer + unit, desktop entries, icons ---
install -m755 "$R/flow-agent" "$R/flow-tray" "$OUT/linux/"
install -m755 deploy/linux/install.sh deploy/linux/uninstall.sh "$OUT/linux/"
install -m644 deploy/linux/flow-agent.service deploy/linux/flowcursor.desktop \
  deploy/linux/flowcursor-tray.desktop "$OUT/linux/"
install -d "$OUT/linux/icons"
install -m644 deploy/icons/flowcursor-*.png deploy/icons/flowcursor.svg "$OUT/linux/icons/"

# --- Linux: .deb ----------------------------------------------------------------
PKG=$(mktemp -d)
install -Dm755 "$R/flow-agent" "$PKG/usr/bin/flow-agent"
install -Dm755 "$R/flow-tray" "$PKG/usr/bin/flow-tray"
install -d "$PKG/usr/lib/systemd/user" "$PKG/usr/share/applications" "$PKG/etc/xdg/autostart"
sed "s|@BINDIR@|/usr/bin|" deploy/linux/flow-agent.service > "$PKG/usr/lib/systemd/user/flow-agent.service"
sed "s|@BINDIR@|/usr/bin|" deploy/linux/flowcursor.desktop > "$PKG/usr/share/applications/flowcursor.desktop"
sed "s|@BINDIR@|/usr/bin|" deploy/linux/flowcursor-tray.desktop > "$PKG/etc/xdg/autostart/flowcursor-tray.desktop"
for png in deploy/icons/flowcursor-*.png; do
  size="$(basename "$png" .png)"; size="${size#flowcursor-}"
  install -Dm644 "$png" "$PKG/usr/share/icons/hicolor/${size}x${size}/apps/flowcursor.png"
done
install -Dm644 deploy/icons/flowcursor.svg "$PKG/usr/share/icons/hicolor/scalable/apps/flowcursor.svg"
install -Dm644 README.md "$PKG/usr/share/doc/flow-agent/README.md"
install -d "$PKG/DEBIAN"
# /etc/xdg/autostart is a conffile: dpkg keeps a user's edit (or removal).
echo /etc/xdg/autostart/flowcursor-tray.desktop > "$PKG/DEBIAN/conffiles"
cat > "$PKG/DEBIAN/control" <<CTRL
Package: flow-agent
Version: $VERSION
Section: utils
Priority: optional
Architecture: amd64
Depends: libc6 (>= 2.35)
Recommends: xdg-desktop-portal, xdg-desktop-portal-gnome, libxkbcommon0, libegl1, gnome-shell-extension-appindicator
Maintainer: FlowCursor <noreply@localhost>
Description: FlowCursor agent and tray icon - share mouse, keyboard and clipboard over the LAN
 Connects to a FlowCursor hub; captures input with the InputCapture portal
 (Wayland) or XInput2 (X11) and injects it with the RemoteDesktop portal /
 libei or XTEST. The agent runs as a systemd user service; flow-tray shows
 its state in the top bar (StatusNotifierItem / AppIndicator) and has a
 status window. The tray starts at login for every user.
CTRL
cat > "$PKG/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
echo "FlowCursor installed. As your normal user run:"
echo "  systemctl --user enable --now flow-agent"
echo "  flow-tray --show      (or log out and in: the tray icon starts at login)"
echo "and enter the join token in the FlowCursor window (or: flow-agent join <TOKEN> [--hub ADDRESS])."
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
