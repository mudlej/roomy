#!/usr/bin/env python3
"""Lab experiment: which pairing of graphics card and drawing method shows a GPU-accelerated VM correctly?
Runs inside the Arch lab box (distrobox.ini) only:   just lab-gpu

A GPU-accelerated VM draws its screen on one of this computer's graphics
cards and hands the picture to roomy's window, which draws with a card of
its own. This boots the same VM once per combination of

  - the card QEMU draws on (each card in turn, plus roomy's own choice), and
  - the method the window toolkit draws with (its default, OpenGL, Vulkan),

and saves a picture of the window each time, with what each side was using.
Windows will appear on your screen for about four minutes; leave them alone.

Everything is written to work/lab/results/gpu-<timestamp>/ (also work/lab/results/latest-gpu).
"""
import glob
import os
import sys
import time
from pathlib import Path

import engine_check as ec
import gui_check as gc

log, sh, roomy, step, expect = ec.log, ec.sh, ec.roomy, ec.step, ec.expect


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def describe_cards():
    """What the kernel says about each graphics card."""
    cards = []
    for card in sorted(glob.glob("/sys/class/drm/card[0-9]*")):
        name = os.path.basename(card)
        if "-" in name:
            continue
        connectors = {os.path.basename(c)[len(name) + 1:]: read(f"{c}/status") for c in sorted(glob.glob(f"{card}-*"))}
        driver = os.path.basename(os.path.realpath(f"{card}/device/driver"))
        render = [os.path.basename(r) for r in glob.glob(f"{card}/device/drm/renderD*")]
        cards.append({"card": name, "render_node": render, "driver": driver, "boot_vga": read(f"{card}/device/boot_vga"),
                      "pci": os.path.basename(os.path.realpath(f"{card}/device")),
                      "vendor": read(f"{card}/device/vendor"), "device": read(f"{card}/device/device"),
                      "connectors": connectors})
    return cards


def graphics_devices_of(pattern):
    """Which /dev/dri devices the processes matching `pattern` have open."""
    found = {}
    for pid in sh(["pgrep", "-f", pattern])["out"].split():
        links = sh(f"ls -l /proc/{pid}/fd 2>/dev/null | grep -o '/dev/dri/[A-Za-z0-9]*' | sort | uniq -c")["out"]
        found[pid] = " ".join(links.split())
    return found


def one_combination(folder, name, node, renderer):
    extra = {}
    if node:
        extra["ROOMY_RENDER_NODE"] = node
    if renderer:
        extra["GSK_RENDERER"] = renderer
    with step(f"combination:{name}") as d:
        d["qemu_card_requested"], d["window_renderer_requested"] = node or "roomy's choice", renderer or "toolkit default"
        window = gc.Window(name, vm=folder, extra_env=extra)
        try:
            d["first_frame_after_s"] = window.wait("the first picture", lambda v: v.get("frames_drawn", 0) > 0, timeout=60)
            # Long enough for the guest to print start-up messages, which make a picture that is easy to judge.
            time.sleep(float(os.environ.get("ROOMY_LAB_SETTLE", 14)))
            d["shot"] = window.shot(name)
            d["facts"] = window.facts()
            d["qemu_devices"] = graphics_devices_of("qemu-system-x86_64.*guest=gpu-probe")
            d["qemu_display_argument"] = [
                arg for pid in d["qemu_devices"] for arg in (read(f"/proc/{pid}/cmdline") or "").split("\0") if arg.startswith("dbus,")]
            expect(d, d["first_frame_after_s"] is not None, "no picture appeared")
            expect(d, d["shot"], "no picture of the window was saved")
        finally:
            roomy("vm", "force-off", folder, timeout=20)
            d["window"] = window.stop()
            d["supervisor_log"] = sh(["tail", "-n", "5", str(folder / "logs" / "supervisor.log")])["out"]
            d["qemu_log"] = sh(["tail", "-n", "15", str(folder / "logs" / "qemu.log")])["out"]
            time.sleep(1)


def summary_note(s):
    """For each combination: what QEMU and the window drew with, and what the window made of the picture."""
    note = s.get("error") or s["data"].get("why") or ""
    if not s["name"].startswith("combination:"):
        return str(note)[:150]
    data = s["data"]
    facts = data.get("facts") or {}
    vm = facts.get("vm") or {}
    note = (f"qemu on {' '.join(data.get('qemu_devices', {}).values()) or '?'} | window: {facts.get('renderer')} on "
            f"{' '.join(facts.get('graphics_devices_open') or []) or '?'} | {vm.get('screen_kind')}: {vm.get('screen_detail')} "
            f"| problems: {vm.get('problems')} {note}")
    return note[:400]


def main():
    ec.start_run("gpu")
    try:
        if os.environ.get("ROOMY_LAB_SKIP_BUILD") != "1":
            if not gc.build():
                return
            with step("tests") as d:
                r = sh("cargo test --workspace > '%s' 2>&1; echo rc=$?" % (ec.OUT / "tests.log"), timeout=1800)
                d["cargo"] = r["out"].strip()
                d["results"] = sh("grep -E 'test result|FAILED|panicked' '%s'" % (ec.OUT / "tests.log"))["out"]
                expect(d, "rc=0" in d["cargo"], "tests failed on this machine; see tests.log")
        with step("cards") as d:
            d["cards"] = describe_cards()
            d["dev_dri"] = sh("ls -l /dev/dri /dev/dri/by-path 2>&1")["out"]
            d["lspci"] = sh("lspci -nn 2>/dev/null | grep -Ei 'vga|3d|display'")["out"]
            d["session"] = {k: os.environ.get(k) for k in ("WAYLAND_DISPLAY", "DISPLAY", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP",
                                                           "GSK_RENDERER", "DRI_PRIME", "MESA_VK_DEVICE_SELECT")}
            d["egl"] = sh("eglinfo -B 2>&1 | grep -Ei 'platform|vendor|renderer|device' | head -40", timeout=20)["out"]
            d["vulkan"] = sh("vulkaninfo --summary 2>&1 | grep -Ei 'deviceName|deviceType|driverName' | head -20", timeout=20)["out"]
            (ec.OUT / "doctor.txt").write_text(roomy("doctor")["out"])
        nodes = sorted(f"/dev/dri/{n}" for card in ec.REPORT["steps"][-1]["data"].get("cards", []) for n in card["render_node"])
        with step("prepare") as d:
            ec.require_base_image()
            _key, key_pub = ec.new_key(ec.WORK)
            folder, _port, created = ec.create_vm("gpu-probe", key_pub, ["--gpu"])
            d["create"] = created
            d["nodes"] = nodes
        if ec.REPORT["steps"][-1]["ok"]:
            for node in [None, *nodes]:
                for renderer in (None, "ngl", "vulkan"):
                    name = f"{os.path.basename(node) if node else 'auto'}-{renderer or 'default'}"
                    one_combination(folder, name, node, renderer)
    except KeyboardInterrupt:
        log("interrupted")
        ec.REPORT["interrupted"] = True
    finally:
        pictures = sorted(str(p.relative_to(ec.OUT)) for p in ec.OUT.glob("*/*-vm.png"))
        ec.finish_run(f"roomy GPU picture experiment {ec.TS}", stop=lambda: sh(["pkill", "-f", str(gc.GUI)]),
                      note=summary_note, extra_lines=[f"pictures: {', '.join(pictures) or 'none'}"])


if __name__ == "__main__":
    sys.exit(main())
