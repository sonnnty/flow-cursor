#!/usr/bin/env python3
"""Minimal org.kde.StatusNotifierWatcher (what GNOME's AppIndicator
extension provides) for the headless UI smoke test: accepts item
registrations and logs what a tray host would show (title, status,
tooltip, icon sizes, menu labels) whenever it changes.

usage: sni-watcher.py LOGFILE
"""
import sys

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib

dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
log = open(sys.argv[1], "a", buffering=1)
IFACE = "org.kde.StatusNotifierWatcher"
items = []
last = {}


def menu_labels(owner, path):
    menu = bus.get_object(owner, path)
    _rev, layout = menu.GetLayout(0, -1, ["label", "enabled", "type"], dbus_interface="com.canonical.dbusmenu")
    out = []
    for child in layout[2]:
        _id, props, _children = child
        if props.get("type") == "separator":
            out.append("----")
        else:
            mark = "" if props.get("enabled", True) else " (disabled)"
            out.append(str(props.get("label", "")) + mark)
    return out


def dump():
    for owner, service in list(items):
        try:
            obj = bus.get_object(owner, "/StatusNotifierItem")
            p = obj.GetAll("org.kde.StatusNotifierItem", dbus_interface=dbus.PROPERTIES_IFACE)
            icons = [f"{int(w)}x{int(h)}" for (w, h, _d) in p.get("IconPixmap", [])]
            tip = p.get("ToolTip")
            text = "\n".join(
                [
                    f"item {service} (owner {owner})",
                    f"  Id: {p.get('Id')}  Title: {p.get('Title')}  Status: {p.get('Status')}",
                    f"  ToolTip: {tip[2] if tip else ''}",
                    f"  ToolTip text: {(tip[3] if tip else '').replace(chr(10), ' | ')}",
                    f"  IconPixmap sizes: {', '.join(icons)}",
                    "  Menu:",
                ]
                + [f"    {l}" for l in menu_labels(owner, p["Menu"])]
            )
        except dbus.DBusException as e:
            if e.get_dbus_name() == "org.freedesktop.DBus.Error.ServiceUnknown":
                log.write(f"item {service} went away\n\n")
                items.remove((owner, service))
                return True
            text = f"item {service}: {e.get_dbus_message()}"
        if last.get(service) != text:
            last[service] = text
            log.write(text + "\n\n")
    return True


class Watcher(dbus.service.Object):
    def __init__(self):
        name = dbus.service.BusName(IFACE, bus)
        super().__init__(name, "/StatusNotifierWatcher")

    @dbus.service.method(IFACE, in_signature="s", sender_keyword="sender")
    def RegisterStatusNotifierItem(self, service, sender=None):
        log.write(f"RegisterStatusNotifierItem({service}) from {sender}\n")
        items.append((sender, service if service.startswith(":") or "." in service else sender))
        self.StatusNotifierItemRegistered(service)

    @dbus.service.method(IFACE, in_signature="s")
    def RegisterStatusNotifierHost(self, service):
        pass

    @dbus.service.signal(IFACE, signature="s")
    def StatusNotifierItemRegistered(self, service):
        pass

    def _props(self):
        return {
            "RegisteredStatusNotifierItems": dbus.Array([s for _o, s in items], signature="s"),
            "IsStatusNotifierHostRegistered": True,
            "ProtocolVersion": dbus.Int32(0),
        }

    # Test hook: forget every item without telling anyone, like the GNOME
    # extension does when the connection that owned an item's name goes away.
    @dbus.service.method("org.flowcursor.Test")
    def Forget(self):
        log.write("Forget(): dropping all items\n")
        items.clear()
        last.clear()

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature="ss", out_signature="v")
    def Get(self, iface, prop):
        return self._props()[prop]

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature="s", out_signature="a{sv}")
    def GetAll(self, iface):
        return self._props()


Watcher()
log.write("watcher up\n")
GLib.timeout_add(1000, dump)
GLib.MainLoop().run()
