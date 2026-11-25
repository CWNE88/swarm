#!/usr/bin/env python3
"""A stand-in for the desktop's portal (org.freedesktop.portal.Desktop),
for labs and `remote_wayland`'s test: its RemoteDesktop and ScreenCast
sessions, answering at once as if the dialog was allowed, and writing
every call made to it as a line of a log.

Like the real one (xdg-desktop-portal), it turns down a caller whose
/proc/<pid>/root it can't open: a process that isn't dumpable, as swarm
itself isn't (`no_dumps`).

    portal-lab.py <log> [cancel]

Run it on a private bus (dbus-daemon --session --print-address). With
`cancel`, Start answers as if the dialog was cancelled. Calling
`lab.Lab.CloseAll` on /lab closes every session from the desktop's side
(the `Closed` signal), as GNOME's own "stop" does.
"""

import os
import sys

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

PATH = "/org/freedesktop/portal/desktop"
RD = "org.freedesktop.portal.RemoteDesktop"
SC = "org.freedesktop.portal.ScreenCast"
STREAM = 44
SIZE = (1920, 1080)

log_path = sys.argv[1]
cancel = "cancel" in sys.argv[2:]
sessions = {}


def say(line):
    with open(log_path, "a") as f:
        f.write(line + "\n")


def plain(v):
    """A D-Bus value as plain Python, for the log."""
    if isinstance(v, dbus.Dictionary):
        return {str(k): plain(x) for k, x in sorted(v.items())}
    if isinstance(v, (dbus.Array, dbus.Struct, list, tuple)):
        return [plain(x) for x in v]
    if isinstance(v, dbus.Boolean):
        return bool(v)
    if isinstance(v, (dbus.String, dbus.ObjectPath)):
        return str(v)
    if isinstance(v, dbus.Double):
        return float(v)
    return int(v)


class Request(dbus.service.Object):
    @dbus.service.signal("org.freedesktop.portal.Request", signature="ua{sv}")
    def Response(self, code, results):
        pass


class Session(dbus.service.Object):
    def __init__(self, bus, path):
        super().__init__(bus, path)
        self.path = path

    @dbus.service.method("org.freedesktop.portal.Session", in_signature="", out_signature="")
    def Close(self):
        say("Close")
        sessions.pop(self.path, None)
        self.remove_from_connection()

    @dbus.service.signal("org.freedesktop.portal.Session", signature="a{sv}")
    def Closed(self, details):
        pass


def answer(bus, sender, options, code, results):
    """The request object for this call, answered just after it returns."""
    name = sender.lstrip(":").replace(".", "_")
    path = f"/org/freedesktop/portal/desktop/request/{name}/{options['handle_token']}"
    req = Request(bus, path)

    def respond():
        req.Response(dbus.UInt32(code), dbus.Dictionary(results, signature="sv"))
        req.remove_from_connection()
        return False

    GLib.timeout_add(10, respond)
    return dbus.ObjectPath(path)


def check_caller(bus, sender):
    """As xdg-desktop-portal does to find who's asking: their /proc/<pid>/root."""
    pid = bus.get_object("org.freedesktop.DBus", "/org/freedesktop/DBus").GetConnectionUnixProcessID(sender, dbus_interface="org.freedesktop.DBus")
    try:
        os.close(os.open(f"/proc/{int(pid)}/root", os.O_PATH))
    except OSError:
        say(f"refused {int(pid)}")
        raise dbus.exceptions.DBusException(f"Portal operation not allowed: Unable to open /proc/{int(pid)}/root", name="org.freedesktop.DBus.Error.AccessDenied")


def without_token(options):
    return {k: v for k, v in plain(options).items() if k not in ("handle_token", "session_handle_token")}


class ScreenCast(dbus.service.Object):
    """ScreenCast's own session (sharing a screen), its methods named as
    RemoteDesktop's are: dbus-python finds each by its interface, class
    by class."""

    @dbus.service.method(SC, in_signature="a{sv}", out_signature="o", sender_keyword="sender")
    def CreateSession(self, options, sender=None):
        check_caller(self.bus, sender)
        path = f"/org/freedesktop/portal/desktop/session/{sender.lstrip(':').replace('.', '_')}/{options['session_handle_token']}"
        sessions[path] = Session(self.bus, path)
        say("ScreenCast.CreateSession")
        return answer(self.bus, sender, options, 0, {"session_handle": dbus.String(path)})

    @dbus.service.method(SC, in_signature="osa{sv}", out_signature="o", sender_keyword="sender")
    def Start(self, session, parent, options, sender=None):
        say("ScreenCast.Start")
        if cancel:
            return answer(self.bus, sender, options, 1, {})
        stream = dbus.Struct((dbus.UInt32(STREAM), dbus.Dictionary({"size": dbus.Struct(SIZE, signature="ii")}, signature="sv")), signature="ua{sv}")
        return answer(self.bus, sender, options, 0, {"streams": dbus.Array([stream], signature="(ua{sv})"), "restore_token": dbus.String("screen-1")})

    @dbus.service.method(SC, in_signature="oa{sv}", out_signature="h")
    def OpenPipeWireRemote(self, session, options):
        say("OpenPipeWireRemote")
        # Any descriptor stands in for PipeWire's.
        return dbus.types.UnixFd(os.open("/dev/null", os.O_RDONLY))


class Portal(ScreenCast):
    def __init__(self, bus):
        super().__init__(bus, PATH)
        self.bus = bus

    PROPS = {
        RD: {"version": dbus.UInt32(2), "AvailableDeviceTypes": dbus.UInt32(7)},
        SC: {"version": dbus.UInt32(4), "AvailableSourceTypes": dbus.UInt32(3), "AvailableCursorModes": dbus.UInt32(7)},
    }

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature="ss", out_signature="v")
    def Get(self, interface, prop):
        return self.PROPS[interface][prop]

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature="s", out_signature="a{sv}")
    def GetAll(self, interface):
        return dbus.Dictionary(self.PROPS.get(interface, {}), signature="sv")

    @dbus.service.method(RD, in_signature="a{sv}", out_signature="o", sender_keyword="sender")
    def CreateSession(self, options, sender=None):
        check_caller(self.bus, sender)
        path = f"/org/freedesktop/portal/desktop/session/{sender.lstrip(':').replace('.', '_')}/{options['session_handle_token']}"
        sessions[path] = Session(self.bus, path)
        say("CreateSession")
        return answer(self.bus, sender, options, 0, {"session_handle": dbus.String(path)})

    @dbus.service.method(RD, in_signature="oa{sv}", out_signature="o", sender_keyword="sender")
    def SelectDevices(self, session, options, sender=None):
        say(f"SelectDevices {without_token(options)}")
        return answer(self.bus, sender, options, 0, {})

    @dbus.service.method(SC, in_signature="oa{sv}", out_signature="o", sender_keyword="sender")
    def SelectSources(self, session, options, sender=None):
        say(f"SelectSources {without_token(options)}")
        return answer(self.bus, sender, options, 0, {})

    @dbus.service.method(RD, in_signature="osa{sv}", out_signature="o", sender_keyword="sender")
    def Start(self, session, parent, options, sender=None):
        say("Start")
        if cancel:
            return answer(self.bus, sender, options, 1, {})
        stream = dbus.Struct((dbus.UInt32(STREAM), dbus.Dictionary({"size": dbus.Struct(SIZE, signature="ii")}, signature="sv")), signature="ua{sv}")
        results = {
            "devices": dbus.UInt32(3),
            "streams": dbus.Array([stream], signature="(ua{sv})"),
            "restore_token": dbus.String("restore-1"),
        }
        return answer(self.bus, sender, options, 0, results)

    @dbus.service.method(RD, in_signature="oa{sv}udd")
    def NotifyPointerMotionAbsolute(self, session, options, stream, x, y):
        say(f"motion {int(stream)} {float(x):.0f} {float(y):.0f}")

    @dbus.service.method(RD, in_signature="oa{sv}iu")
    def NotifyPointerButton(self, session, options, button, state):
        say(f"button {int(button):#x} {int(state)}")

    @dbus.service.method(RD, in_signature="oa{sv}ui")
    def NotifyPointerAxisDiscrete(self, session, options, axis, steps):
        say(f"axis {int(axis)} {int(steps)}")

    @dbus.service.method(RD, in_signature="oa{sv}iu")
    def NotifyKeyboardKeysym(self, session, options, keysym, state):
        say(f"keysym {int(keysym):#x} {int(state)}")


class Lab(dbus.service.Object):
    @dbus.service.method("lab.Lab", in_signature="", out_signature="")
    def CloseAll(self):
        for session in list(sessions.values()):
            session.Closed(dbus.Dictionary({}, signature="sv"))
            session.remove_from_connection()
        sessions.clear()


DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
name = dbus.service.BusName("org.freedesktop.portal.Desktop", bus)
Portal(bus)
Lab(bus, "/lab")
say("ready")
GLib.MainLoop().run()
