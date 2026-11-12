#!/usr/bin/env python3
"""A stand-in flow-agent for the tray checks: serves the local IPC protocol
(Subscribe -> a status line every 200 ms) on a Unix socket. What it reports
follows files in a control directory, so a script can change the state:

  update    the last update installed another tray build (the tray restarts)
  away      away (not on an allowed network): the session is closed on purpose
  paused    sharing paused
  refused   the GNOME permission dialog was refused (a real problem)

usage: fake-agent.py SOCKET CONTROL_DIR
"""
import json
import os
import socketserver
import sys
import time

SOCK, CTL = sys.argv[1], sys.argv[2]


def has(name):
    return os.path.exists(os.path.join(CTL, name))


def status():
    s = {
        "agent_version": "0.0.0+fake",
        "node_id": 1,
        "node_name": "laptop",
        "hub_connected": True,
        "hub_address": "192.168.1.10",
        "can_capture": True,
        "can_inject": True,
        "can_clipboard": True,
        "controllable": True,
        "session_wanted": True,
        "update": {"state": "current"},
    }
    if has("update"):
        s["update"]["tray_build"] = "9.9.9+fake-update"
    if has("away") or has("paused"):
        s.update(away=has("away"), paused=has("paused"), can_inject=False, can_clipboard=False,
                 controllable=False, session_wanted=False,
                 session_idle="paused" if has("paused") else "away")
    if has("refused"):
        s.update(can_inject=False, can_clipboard=False, controllable=False,
                 problems=["remote control permission (RemoteDesktop portal) was refused in the GNOME dialog"])
    return s


class H(socketserver.StreamRequestHandler):
    def handle(self):
        for line in self.rfile:
            if json.loads(line).get("cmd") != "subscribe":
                continue
            while True:
                msg = {"v": 1, "type": "status", "status": status()}
                try:
                    self.wfile.write((json.dumps(msg) + "\n").encode())
                    self.wfile.flush()
                except OSError:
                    return
                time.sleep(0.2)


class S(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


if os.path.exists(SOCK):
    os.unlink(SOCK)
S(SOCK, H).serve_forever()
