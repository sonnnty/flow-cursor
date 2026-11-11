#!/usr/bin/env bash
# Inside the end-to-end container (ubuntu:22.04, run as root): installs the
# "old" build with install.sh for user fc, joins the hub and waits until the
# agent has replaced itself with the hub's build (MODE=update) or rolled back
# to the old one (MODE=rollback). /in is build/out (linux/: the new build's
# installer files, e2e/old/flow-agent: the old agent); /e2e this directory.
set -euo pipefail
: "${TOKEN:?}" "${HUB:?}" "${MODE:?}" "${OLD:?}" "${NEW:?}"
useradd -m -s /bin/bash fc
install -d -o fc -m 700 /run/user/fc
# The installer bundle of the old build: the old agent, everything else as shipped.
pkg=/home/fc/pkg
cp -r /in/linux "$pkg"
cp /in/e2e/old/flow-agent "$pkg/flow-agent"
install -d /home/fc/stub && cp /e2e/systemctl /home/fc/stub/systemctl
install -d /home/fc/.config/flowcursor
echo 'backend = "sim"' > /home/fc/.config/flowcursor/agent.toml
chown -R fc:fc /home/fc
as_fc() {
  runuser -u fc -- env HOME=/home/fc XDG_RUNTIME_DIR=/run/user/fc PATH=/home/fc/stub:/usr/bin:/bin "$@"
}
echo "== install.sh with the old build ($OLD)"
as_fc bash -c "cd $pkg && ./install.sh '$TOKEN' '$HUB'" | sed 's/^/   /'
as_fc /home/fc/.local/bin/flow-agent --version

want="$NEW"; [ "$MODE" = rollback ] && want="$OLD"
deadline=$((SECONDS + ${TIMEOUT:-150}))
seen_new=0
while :; do
  st="$(as_fc /home/fc/.local/bin/flow-agent status 2>&1 || true)"
  ver="$(sed -n 's/^running agent: *pid [0-9]* (version \([^,]*\),.*/\1/p' <<<"$st")"
  upd="$(sed -n 's/^update: *//p' <<<"$st")"
  failed="$(sed -n 's/^update failed: *//p' <<<"$st")"
  echo "   t=${SECONDS}s running=${ver:-none} update=[${upd}] ${failed:+failed=[$failed]}"
  [ "$ver" = "$NEW" ] && seen_new=1
  if [ "$MODE" = update ] && [ "$ver" = "$NEW" ] && [[ "$upd" == current* ]]; then break; fi
  if [ "$MODE" = rollback ] && [ "$ver" = "$OLD" ] && [ -n "$failed" ]; then break; fi
  if [ $SECONDS -ge $deadline ]; then
    echo "!! timeout waiting for $want"; echo "== agent log"; tail -60 /home/fc/agent.log
    for f in /home/fc/.local/share/flowcursor/update/guard.log; do [ -f "$f" ] && { echo "== $f"; cat "$f"; }; done
    exit 1
  fi
  sleep 2
done
echo "== installed binary: $(as_fc /home/fc/.local/bin/flow-agent --version)"
echo "== previous version kept for rollback:"; ls -la /home/fc/.local/share/flowcursor/update/previous/
echo "== update state"; cat /home/fc/.local/share/flowcursor/update/update-state.json; echo
if [ "$MODE" = rollback ]; then
  # No retry loop: the same build is not tried again on its own.
  sleep 15
  st="$(as_fc /home/fc/.local/bin/flow-agent status 2>&1 || true)"
  grep -E "^(running agent|update)" <<<"$st" | sed 's/^/   /'
  grep -q "version $OLD" <<<"$st" || { echo "!! no longer on the old version"; exit 1; }
  grep -q "^update: *available" <<<"$st" || { echo "!! expected the update to stay 'available'"; exit 1; }
  [ -f /home/fc/.local/share/flowcursor/update/guard.log ] && { echo "== guard log"; cat /home/fc/.local/share/flowcursor/update/guard.log; }
fi
echo "== agent log (update lines)"; grep -aiE "update|trial|guard|restart|roll" /home/fc/agent.log | sed 's/\x1b\[[0-9;]*m//g' | tail -25
echo "OK ($MODE)"
