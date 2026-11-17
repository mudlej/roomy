#!/usr/bin/env python3
"""Minimal QEMU D-Bus display client for the roomy lab.
Runs inside the Arch lab box (distrobox.ini) only, started by probe.py.

Connects to a QEMU `-display dbus` instance (over a bus address or an
already-connected peer-to-peer socket), dumps what QEMU exposes, registers a
display listener, and records every call QEMU makes on it. The result is
written as JSON to --out. Not meant to be used by hand.
"""
import argparse
import json
import os
import signal
import socket
import struct
import sys
import time
import traceback

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib  # noqa: E402

# Signatures copied from QEMU v11.1.0 ui/dbus-display1.xml.
LISTENER_XML = """
<node>
  <interface name="org.qemu.Display1.Listener">
    <method name="Scanout">
      <arg type="u" name="width" direction="in"/>
      <arg type="u" name="height" direction="in"/>
      <arg type="u" name="stride" direction="in"/>
      <arg type="u" name="pixman_format" direction="in"/>
      <arg type="ay" name="data" direction="in"/>
    </method>
    <method name="Update">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
      <arg type="i" name="width" direction="in"/>
      <arg type="i" name="height" direction="in"/>
      <arg type="u" name="stride" direction="in"/>
      <arg type="u" name="pixman_format" direction="in"/>
      <arg type="ay" name="data" direction="in"/>
    </method>
    <method name="ScanoutDMABUF">
      <arg type="h" name="dmabuf" direction="in"/>
      <arg type="u" name="width" direction="in"/>
      <arg type="u" name="height" direction="in"/>
      <arg type="u" name="stride" direction="in"/>
      <arg type="u" name="fourcc" direction="in"/>
      <arg type="t" name="modifier" direction="in"/>
      <arg type="b" name="y0_top" direction="in"/>
    </method>
    <method name="UpdateDMABUF">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
      <arg type="i" name="width" direction="in"/>
      <arg type="i" name="height" direction="in"/>
    </method>
    <method name="Disable"/>
    <method name="MouseSet">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
      <arg type="i" name="on" direction="in"/>
    </method>
    <method name="CursorDefine">
      <arg type="i" name="width" direction="in"/>
      <arg type="i" name="height" direction="in"/>
      <arg type="i" name="hot_x" direction="in"/>
      <arg type="i" name="hot_y" direction="in"/>
      <arg type="ay" name="data" direction="in"/>
    </method>
    <property name="Interfaces" type="as" access="read"/>
  </interface>
  <interface name="org.qemu.Display1.Listener.Unix.Map">
    <method name="ScanoutMap">
      <arg type="h" name="handle" direction="in"/>
      <arg type="u" name="offset" direction="in"/>
      <arg type="u" name="width" direction="in"/>
      <arg type="u" name="height" direction="in"/>
      <arg type="u" name="stride" direction="in"/>
      <arg type="u" name="pixman_format" direction="in"/>
    </method>
    <method name="UpdateMap">
      <arg type="i" name="x" direction="in"/>
      <arg type="i" name="y" direction="in"/>
      <arg type="i" name="width" direction="in"/>
      <arg type="i" name="height" direction="in"/>
    </method>
  </interface>
  <interface name="org.qemu.Display1.Listener.Unix.ScanoutDMABUF2">
    <method name="ScanoutDMABUF2">
      <arg type="ah" name="dmabuf" direction="in"/>
      <arg type="u" name="x" direction="in"/>
      <arg type="u" name="y" direction="in"/>
      <arg type="u" name="width" direction="in"/>
      <arg type="u" name="height" direction="in"/>
      <arg type="au" name="offset" direction="in"/>
      <arg type="au" name="stride" direction="in"/>
      <arg type="u" name="num_planes" direction="in"/>
      <arg type="u" name="fourcc" direction="in"/>
      <arg type="u" name="backing_width" direction="in"/>
      <arg type="u" name="backing_height" direction="in"/>
      <arg type="t" name="modifier" direction="in"/>
      <arg type="b" name="y0_top" direction="in"/>
    </method>
  </interface>
</node>
"""

CORE_IFACE = "org.qemu.Display1.Listener"
EXTRA_IFACES = [
    "org.qemu.Display1.Listener.Unix.Map",
    "org.qemu.Display1.Listener.Unix.ScanoutDMABUF2",
]
ROOT = "/org/qemu/Display1"

try:
    UnixFDList = Gio.UnixFDList
except AttributeError:  # newer GLib exposes Unix-only types in a separate namespace
    gi.require_version("GioUnix", "2.0")
    from gi.repository import GioUnix
    UnixFDList = GioUnix.FDList
MAX_RECORDED_CALLS = 80

T0 = time.monotonic()
OUT = {"errors": [], "calls": [], "counts": {}}
ARGS = None


def note_error(where, exc):
    OUT["errors"].append({"where": where, "error": repr(exc), "trace": traceback.format_exc()[-1500:]})


def write_out():
    OUT["elapsed_s"] = round(time.monotonic() - T0, 3)
    tmp = ARGS.out + ".tmp"
    with open(tmp, "w") as f:
        json.dump(OUT, f, indent=1, default=str)
    os.replace(tmp, ARGS.out)


def fourcc_str(v):
    return struct.pack("<I", v).decode("ascii", "replace")


def fd_info(invocation, index):
    """Describe file descriptor `index` attached to the method call."""
    fdl = invocation.get_message().get_unix_fd_list()
    if fdl is None:
        return {"fd": "no fd list on message"}
    info = {"fds_on_message": fdl.get_length()}
    fd = fdl.get(index)
    try:
        st = os.fstat(fd)
        info["st_size"] = st.st_size
        try:
            info["seek_end"] = os.lseek(fd, 0, os.SEEK_END)
        except OSError as e:
            info["seek_end"] = repr(e)
        try:
            info["link"] = os.readlink(f"/proc/self/fd/{fd}")
        except OSError:
            pass
    finally:
        os.close(fd)
    return info


def on_method_call(conn, sender, path, iface, method, params, invocation):
    rec = {"t": round(time.monotonic() - T0, 3), "iface": iface, "method": method,
           "sig": params.get_type_string()}
    try:
        child = params.get_child_value
        if method == "Scanout":
            rec.update(width=child(0).unpack(), height=child(1).unpack(), stride=child(2).unpack(),
                       pixman_format=hex(child(3).unpack()), data_bytes=child(4).get_size())
        elif method == "Update":
            rec.update(x=child(0).unpack(), y=child(1).unpack(), width=child(2).unpack(),
                       height=child(3).unpack(), stride=child(4).unpack(),
                       pixman_format=hex(child(5).unpack()), data_bytes=child(6).get_size())
        elif method == "ScanoutDMABUF":
            rec.update(width=child(1).unpack(), height=child(2).unpack(), stride=child(3).unpack(),
                       fourcc=fourcc_str(child(4).unpack()), modifier=hex(child(5).unpack()),
                       y0_top=child(6).unpack(), fd=fd_info(invocation, child(0).unpack()))
        elif method == "ScanoutDMABUF2":
            handles = child(0).unpack()
            rec.update(planes_fds=len(handles), x=child(1).unpack(), y=child(2).unpack(),
                       width=child(3).unpack(), height=child(4).unpack(), offset=child(5).unpack(),
                       stride=child(6).unpack(), num_planes=child(7).unpack(),
                       fourcc=fourcc_str(child(8).unpack()), backing_width=child(9).unpack(),
                       backing_height=child(10).unpack(), modifier=hex(child(11).unpack()),
                       y0_top=child(12).unpack())
            if handles:
                rec["fd"] = fd_info(invocation, handles[0])
        elif method == "ScanoutMap":
            rec.update(offset=child(1).unpack(), width=child(2).unpack(), height=child(3).unpack(),
                       stride=child(4).unpack(), pixman_format=hex(child(5).unpack()),
                       fd=fd_info(invocation, child(0).unpack()))
        elif method == "CursorDefine":
            rec.update(width=child(0).unpack(), height=child(1).unpack(), hot_x=child(2).unpack(),
                       hot_y=child(3).unpack(), data_bytes=child(4).get_size())
        else:
            rec["args"] = params.unpack()
    except Exception as e:  # keep going: the reply matters more than the record
        rec["decode_error"] = repr(e)
    OUT["counts"][method] = OUT["counts"].get(method, 0) + 1
    if len(OUT["calls"]) < MAX_RECORDED_CALLS:
        OUT["calls"].append(rec)
    invocation.return_value(None)


def on_get_property(conn, sender, path, iface, prop):
    if prop == "Interfaces":
        return GLib.Variant("as", EXTRA_IFACES if ARGS.extras else [])
    return None


def stream_from_fd(fd):
    sock = Gio.Socket.new_from_fd(fd)
    return sock.connection_factory_create_connection()


def main():
    global ARGS
    ap = argparse.ArgumentParser()
    ap.add_argument("--bus", help="D-Bus address QEMU is connected to (name org.qemu)")
    ap.add_argument("--p2p-fd", type=int, help="inherited socket fd already handed to QEMU with add_client")
    ap.add_argument("--out", required=True)
    ap.add_argument("--seconds", type=float, default=4.0)
    ap.add_argument("--extras", action="store_true", help="advertise Unix.Map and ScanoutDMABUF2 support")
    ap.add_argument("--resize", help="WxH to request with SetUIInfo one second after registering")
    ARGS = ap.parse_args()

    def watchdog(signum, frame):
        OUT["errors"].append({"where": "watchdog", "error": "client did not finish in time"})
        write_out()
        os._exit(3)

    signal.signal(signal.SIGALRM, watchdog)
    signal.alarm(int(ARGS.seconds) + 25)

    flags = Gio.DBusConnectionFlags
    if ARGS.p2p_fd is not None:
        conn = Gio.DBusConnection.new_sync(stream_from_fd(ARGS.p2p_fd), None,
                                           flags.AUTHENTICATION_CLIENT, None, None)
        dest = None
        OUT["mode"] = "p2p"
    else:
        conn = Gio.DBusConnection.new_for_address_sync(
            ARGS.bus, flags.AUTHENTICATION_CLIENT | flags.MESSAGE_BUS_CONNECTION, None, None)
        dest = "org.qemu"
        OUT["mode"] = "bus"
    OUT["connected"] = True

    def call(path, iface, method, params=None, fds=None, timeout_ms=5000):
        fdl = UnixFDList.new_from_array(fds) if fds else None
        ret, _ = conn.call_with_unix_fd_list_sync(dest, path, iface, method, params, None,
                                                  Gio.DBusCallFlags.NONE, timeout_ms, fdl, None)
        return ret

    def get_all(path, iface):
        return call(path, "org.freedesktop.DBus.Properties", "GetAll",
                    GLib.Variant("(s)", (iface,))).unpack()[0]

    console_path = f"{ROOT}/Console_0"

    # What does this QEMU expose?
    paths = [f"{ROOT}/VM", console_path, f"{ROOT}/Clipboard", f"{ROOT}/Audio"]
    try:
        managed = call(ROOT, "org.freedesktop.DBus.ObjectManager", "GetManagedObjects").unpack()[0]
        OUT["objects"] = {p: {i: props for i, props in ifaces.items()} for p, ifaces in managed.items()}
        paths = sorted(set(paths) | set(managed))
    except Exception as e:
        note_error("GetManagedObjects", e)
    try:
        OUT["vm"] = get_all(f"{ROOT}/VM", "org.qemu.Display1.VM")
    except Exception as e:
        note_error("VM properties", e)
    try:
        OUT["console_before"] = get_all(console_path, "org.qemu.Display1.Console")
    except Exception as e:
        note_error("Console properties", e)
    OUT["introspection"] = {}
    for p in paths:
        try:
            OUT["introspection"][p] = call(p, "org.freedesktop.DBus.Introspectable", "Introspect").unpack()[0]
        except Exception as e:
            OUT["introspection"][p] = "ERROR " + repr(e)
    write_out()

    # Register as a display listener. QEMU replies first, then performs the
    # server side of the D-Bus handshake on the socket we passed.
    loop = GLib.MainLoop()
    listener_conn = None
    try:
        ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        call(console_path, "org.qemu.Display1.Console", "RegisterListener",
             GLib.Variant("(h)", (0,)), fds=[theirs.detach()])
        listener_conn = Gio.DBusConnection.new_sync(
            stream_from_fd(ours.detach()), None,
            flags.AUTHENTICATION_CLIENT | flags.DELAY_MESSAGE_PROCESSING, None, None)
        node = Gio.DBusNodeInfo.new_for_xml(LISTENER_XML)
        for iface in node.interfaces:
            if iface.name == CORE_IFACE or ARGS.extras:
                listener_conn.register_object(f"{ROOT}/Listener", iface, on_method_call, on_get_property, None)
        listener_conn.start_message_processing()
        OUT["listener_registered"] = True
    except Exception as e:
        OUT["listener_registered"] = False
        note_error("RegisterListener", e)
        write_out()
        return 1

    # Everything below is asynchronous: QEMU may be calling into our listener
    # at any moment, so we must never block the main loop on a call to QEMU.
    def do_resize():
        try:
            w, h = (int(v) for v in ARGS.resize.lower().split("x"))

            def done(c, res):
                try:
                    c.call_finish(res)
                    OUT["resize"] = {"requested": [w, h], "ok": True, "t": round(time.monotonic() - T0, 3)}
                except Exception as e:
                    OUT["resize"] = {"requested": [w, h], "ok": False, "error": repr(e)}

            conn.call(dest, console_path, "org.qemu.Display1.Console", "SetUIInfo",
                      GLib.Variant("(qqiiuu)", (0, 0, 0, 0, w, h)), None,
                      Gio.DBusCallFlags.NONE, 5000, None, done)
        except Exception as e:
            note_error("SetUIInfo", e)
        return False

    def read_console_after():
        def done(c, res):
            try:
                OUT["console_after"] = c.call_finish(res).unpack()[0]
            except Exception as e:
                OUT["console_after"] = "ERROR " + repr(e)

        conn.call(dest, console_path, "org.freedesktop.DBus.Properties", "GetAll",
                  GLib.Variant("(s)", ("org.qemu.Display1.Console",)), None,
                  Gio.DBusCallFlags.NONE, 3000, None, done)
        return False

    if ARGS.resize:
        GLib.timeout_add(1000, do_resize)
    GLib.timeout_add(max(200, int(ARGS.seconds * 1000) - 700), read_console_after)
    GLib.timeout_add(int(ARGS.seconds * 1000), lambda: (loop.quit(), False)[1])
    loop.run()
    write_out()
    return 0


if __name__ == "__main__":
    try:
        rc = main()
    except Exception as e:
        note_error("main", e)
        if ARGS is not None:
            write_out()
        rc = 2
    sys.exit(rc)
