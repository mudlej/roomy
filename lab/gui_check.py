#!/usr/bin/env python3
"""Lab run for roomy's window, against real QEMU, a real guest and a real desktop session.
Runs inside the Arch lab box (distrobox.ini), except `distro` mode, which runs inside one of the other boxes there.

Builds everything, then drives the real window through its test hooks (see
crates/gui/src/lab.rs): pictures of the window at each stage, the keyboard
checked from inside the guest, the window following a VM that is paused,
suspended, continued and shut down from the command line, and a
GPU-accelerated VM. Windows will appear on your screen for a few minutes;
leave them alone while it runs.

Everything is written to work/lab/results/gui-<timestamp>/ (also work/lab/results/latest-gui).
Everything, about ten minutes:   just lab-gui
A short run of the sound, microphone, settings and dialog checks,
about two and a half minutes:   just lab-gui quick
A short run of only the pointer and clipboard checks, which install a bare
graphical session in the guest, about four minutes:   just lab-gui desktop
The single-file release (just appimage), built and then put through
the quick checks in place of the separate programs:   just lab-gui appimage
That same file tried on another distribution, inside a box of it (see
distrobox.ini), where roomy must first install what it needs:   just lab-distro
"""
import contextlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

import engine_check as ec

TS = ec.TS


def distribution():
    """This system's ID from os-release, for example "fedora"."""
    with contextlib.suppress(OSError):
        for line in Path("/etc/os-release").read_text().splitlines():
            if line.startswith("ID="):
                return line[3:].strip().strip("\"'") or "linux"
    return "linux"


# "distro" tries the single-file release that already exists (built by `just appimage` in the Arch box) on
# whatever distribution this is: roomy has to install what it needs here by itself, then pass the quick checks.
DISTRO = "distro" in sys.argv[1:]
KIND = f"distro-{distribution()}" if DISTRO else "gui"
SINGLE_FILE = ec.REPO / "work" / "dist" / "roomy-x86_64.AppImage"
GUI = ec.REPO / "target" / "release" / "roomy-gui"
log, sh, roomy, step, expect = ec.log, ec.sh, ec.roomy, ec.step, ec.expect
# A short run of one group of checks: "quick" (sound, microphone, settings, dialogs) or "desktop" (pointer, clipboard).
# "appimage" builds roomy's single-file release first and runs the quick checks with that file as the `roomy` program.
APPIMAGE = "appimage" in sys.argv[1:]
ONLY = next((word for word in sys.argv[1:] if word in ("quick", "desktop")), "quick" if APPIMAGE or DISTRO else None)
QUICK = ONLY is not None


class Window:
    """A running roomy window in test mode."""

    def __init__(self, name, vm=None, seconds=900, extra_env=None):
        self.name = name
        self.dir = ec.OUT / name
        self.dir.mkdir(parents=True)
        env = dict(os.environ, ROOMY_GUI_LAB=str(self.dir), ROOMY_GUI_LAB_SECONDS=str(seconds))
        env.update(extra_env or {})
        if vm:
            env["ROOMY_GUI_LAB_VM"] = str(vm)
        self.output = open(ec.OUT / f"{name}-window.log", "w")
        # Started the way a user would: the roomy command opens the window.
        self.process = subprocess.Popen([str(ec.ROOMY)], env=env, stdout=self.output, stderr=subprocess.STDOUT)

    def facts(self):
        try:
            return json.loads((self.dir / "facts.json").read_text())
        except (OSError, ValueError):
            return {}

    def vm(self):
        return self.facts().get("vm") or {}

    def alive(self):
        return self.process.poll() is None

    def tell(self, kind, text, timeout=20):
        """Drop a trigger file and wait until the window has obeyed it."""
        target = self.dir / kind
        partial = self.dir / f".{kind}.partial"
        partial.write_text(text)
        os.replace(partial, target)
        deadline = time.monotonic() + timeout
        while target.exists() and self.alive() and time.monotonic() < deadline:
            time.sleep(0.1)
        return not target.exists()

    def shot(self, name):
        return self.tell("shot", name)

    def type(self, text):
        obeyed = self.tell("type", text)
        time.sleep(1.5)
        return obeyed

    def wait(self, what, condition, timeout=60):
        """Seconds until `condition(vm facts)` holds, or None."""
        t0 = time.monotonic()
        while time.monotonic() - t0 < timeout:
            if not self.alive():
                return None
            if condition(self.vm()):
                return round(time.monotonic() - t0, 2)
            time.sleep(0.25)
        log(f"    {self.name}: timed out waiting for {what}; last facts: {self.vm()}")
        return None

    def stop(self):
        if self.alive():
            self.tell("stop", "now", timeout=10)
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.output.close()
        text = (ec.OUT / f"{self.name}-window.log").read_text(errors="replace")
        return {"exit_status": self.process.returncode, "output": text[-4000:],
                "gtk_complaints": [line for line in text.splitlines() if "CRITICAL" in line or "WARNING" in line][:40],
                "final_facts": self.facts()}


def text_is_top_left(window, shot):
    """Whether the picture taken as `shot` has its bright pixels where a login prompt is: near the top-left.

    A scrambled picture has them spread over the width or at the bottom; an upside-down one at the bottom.
    """
    area = ((window.facts().get("shots") or {}).get(shot) or {}).get("bright_area")
    if not area:
        return False, area
    left, top, right, bottom = area
    return left < 0.1 and top < 0.1 and right < 0.6 and bottom < 0.25, area


def console_text(guest):
    """What the guest's first text console shows right now."""
    return guest.g("sudo cat /dev/vcs1 | tr -s ' \\0' ' ' | tail -c 600")


def empty_library():
    with step("window:empty-library") as d:
        window = Window("empty", seconds=60)
        try:
            d["facts_after_s"] = None
            t0 = time.monotonic()
            while time.monotonic() - t0 < 20 and window.alive() and not window.facts():
                time.sleep(0.25)
            d["facts_after_s"] = round(time.monotonic() - t0, 2)
            time.sleep(2)  # let the doctor finish
            d["shot"] = window.shot("empty")
            d["facts"] = window.facts()
            expect(d, bool(d["facts"]), "the window never reported anything (did it start?)")
            expect(d, d["shot"], "the window did not take a picture when asked")
            expect(d, "working" in str(d["facts"].get("doctor", "")), "the window's own check of this computer did not pass")
        finally:
            d["window"] = window.stop()
        expect(d, d["window"]["exit_status"] == 0, "the window did not exit cleanly")


def download_window():
    """The window that downloads installation images: it finds Fedora's editions on the real servers."""
    with step("window:downloads") as d:
        window = Window("downloads", seconds=180)
        try:
            t0 = time.monotonic()
            while time.monotonic() - t0 < 20 and window.alive() and not window.facts():
                time.sleep(0.25)
            d["opened"] = window.tell("open", "downloads:fedora")
            editions = []
            t0 = time.monotonic()
            while time.monotonic() - t0 < 90 and window.alive():
                editions = ((window.facts().get("downloads") or {}).get("editions")) or []
                if editions and "looking" not in editions:
                    break
                time.sleep(0.5)
            d["looked_up_after_s"] = round(time.monotonic() - t0, 1)
            time.sleep(2)  # let the screenshot arrive and the rows settle
            d["shot"] = window.shot("fedora")
            d["facts"] = (window.facts().get("downloads")) or {}
            found = [e for e in editions if isinstance(e, dict) and "found" in e]
            d["found"], d["failed"] = len(found), [e["failed"] for e in editions if isinstance(e, dict) and "failed" in e]
            expect(d, d["opened"], "the window did not open the download window")
            expect(d, len(found) == len(editions) and found, "not every Fedora edition was found; see facts")
            expect(d, d["shot"], "the window did not take a picture when asked")
        finally:
            d["window"] = window.stop()
        expect(d, d["window"]["exit_status"] == 0, "the window did not exit cleanly")


def main_vm(key, key_pub):
    with step("main:create") as d:
        folder, port, created = ec.create_vm("gui-main", key_pub)
        d["create"] = created
        d["library"] = roomy("vm", "list")
    if not ec.REPORT["steps"][-1]["ok"]:
        return  # without a VM there is nothing more to try
    guest = ec.Guest(folder, port, key)

    # In the desktop run QEMU also writes down what it does with the clipboard (into the VM's qemu.log).
    tracing = ({"ROOMY_QEMU_TRACE": "vdagent_* dbus_clipboard_* clipboard_* dbus_mouse_* input_event_* input_mouse_*"}
               if ONLY == "desktop" else None)
    window = Window("main", vm=folder, extra_env=tracing)
    try:
        with step("main:window-starts-the-vm") as d:
            d["first_frame_after_s"] = window.wait("the first picture", lambda v: v.get("frames_drawn", 0) > 0, timeout=90)
            d["vm"] = window.vm()
            if d["first_frame_after_s"] is None and d["vm"].get("window_suspended"):
                # The desktop tells a window nobody can see to stop drawing; that is not the window's fault.
                expect(d, False, "the VM's window was covered, minimised, on another desktop or behind a locked screen the "
                                 "whole time, so it drew nothing: run again with the window in view")
            expect(d, d["first_frame_after_s"] is not None, "the VM's screen never appeared in the window")
            d["shot_boot"] = window.shot("1-booting")
            d["boot_to_ssh_s"] = guest.wait_ssh()
            d["status_cli"] = guest.status()
            expect(d, d["status_cli"] == "running", "the VM is not running")
            time.sleep(8)  # the login prompt appears a little after the network is up
            d["vm_at_login"] = window.vm()
            expect(d, not d["vm_at_login"].get("problems"), "the window reported display problems")

        with step("main:keyboard") as d:
            d["before"] = console_text(guest)
            d["typed"] = window.type("roomylab")
            d["after"] = console_text(guest)
            d["shot"] = window.shot("2-typed")
            time.sleep(1)  # the window writes its facts twice a second
            d["picture_ok"], d["bright_area"] = text_is_top_left(window, "2-typed")
            expect(d, d["picture_ok"], f"the screen does not show the login prompt at its top-left (bright area {d['bright_area']})")
            expect(d, "roomylab" in d["after"] and "roomylab" not in d["before"],
                   "text typed in the window did not arrive on the guest's console")

        if ONLY == "quick":
            feature_checks(window, guest, folder)
            return
        if ONLY == "desktop":
            desktop_features(window, guest, folder)
            return
        with step("main:follows-pause-and-resume") as d:
            d["pause"] = roomy("vm", "pause", folder)
            d["window_saw_paused_after_s"] = window.wait("paused", lambda v: v.get("state") == "paused", timeout=15)
            time.sleep(1)
            d["shot"] = window.shot("3-paused")
            d["resume"] = roomy("vm", "resume", folder)
            d["window_saw_running_after_s"] = window.wait("running", lambda v: v.get("state") == "running", timeout=15)
            expect(d, d["window_saw_paused_after_s"] is not None and d["window_saw_running_after_s"] is not None,
                   "the window did not follow pause and resume")

        with step("main:follows-suspend-and-continue") as d:
            frames_before = window.vm().get("frames_drawn", 0)
            d["suspend"] = roomy("vm", "suspend", folder, timeout=600)
            d["window_saw_suspended_after_s"] = window.wait("suspended", lambda v: v.get("state") == "suspended", timeout=30)
            time.sleep(1)
            d["shot_suspended"] = window.shot("4-suspended")
            # The window keeps a small picture of the screen in the VM's folder; the library's list shows it.
            thumbnail = Path(folder) / "thumbnail.png"
            d["thumbnail_bytes"] = thumbnail.stat().st_size if thumbnail.is_file() else None
            if thumbnail.is_file():
                shutil.copy(thumbnail, ec.OUT / "main" / "thumbnail.png")
            expect(d, d["thumbnail_bytes"], "the window saved no picture of the suspended VM's screen for the library")
            d["start"] = roomy("vm", "start", folder)
            d["window_saw_running_after_s"] = window.wait("running", lambda v: v.get("state") == "running", timeout=30)
            d["new_picture_after_s"] = window.wait("a new picture", lambda v: v.get("frames_drawn", 0) > frames_before, timeout=30)
            d["ssh_after_s"] = guest.wait_ssh(timeout=60)
            d["typed"] = window.type("again")
            d["console"] = console_text(guest)
            d["shot_continued"] = window.shot("5-continued")
            d["vm"] = window.vm()
            expect(d, d["window_saw_suspended_after_s"] is not None, "the window did not show the VM as suspended")
            expect(d, d["new_picture_after_s"] is not None, "the screen did not come back after continuing")
            expect(d, "roomylabagain" in d["console"], "the keyboard did not work after continuing")
            expect(d, not d["vm"].get("problems"), "the window reported display problems")

        feature_checks(window, guest, folder)
        desktop_features(window, guest, folder)

        with step("main:follows-shutdown") as d:
            d["shutdown"] = roomy("vm", "shutdown", folder)
            d["window_saw_off_after_s"] = window.wait("off", lambda v: v.get("state") == "off", timeout=120)
            time.sleep(1)
            d["shot"] = window.shot("6-off")
            expect(d, d["window_saw_off_after_s"] is not None, "the window did not show the VM as off")
        with step("main:hardware-changes-apply-at-the-next-boot") as d:
            d["start"] = roomy("vm", "start", folder)
            d["boot_to_ssh_s"] = guest.wait_ssh()
            d["processors_in_guest"] = guest.g("nproc").strip()
            d["shows"] = window.vm().get("shows")
            expect(d, d["processors_in_guest"] == "6", "the processor count changed while it ran did not apply at the next boot")
            expect(d, not (d["shows"] or {}).get("waiting_for_next_boot"), "the window still says changes are waiting after the boot")
            roomy("vm", "force-off", folder)
            window.wait("off", lambda v: v.get("state") == "off", timeout=20)
    finally:
        with step("main:window-exit") as d:
            d["window"] = window.stop()
            expect(d, d["window"]["exit_status"] == 0, "the window did not exit cleanly")
            expect(d, not d["window"]["gtk_complaints"], "the window toolkit printed warnings; see main-window.log")
        if QUICK:
            # Closing the window must leave the VM running. This matters most for the single-file release,
            # whose programs live in a mount that goes away with the process that opened the file.
            with step("main:vm-outlives-the-window") as d:
                time.sleep(3)
                d["status"] = guest.status()
                d["ssh"] = guest.ssh("echo still-here", timeout=10)["out"].strip()
                d["supervisor"] = sh("pgrep -af 'supervise .*gui-main' | cut -c1-200")["out"]
                d["mounts"] = sh("grep -c 'mount_roomy' /proc/mounts")["out"].strip()
                expect(d, d["status"] == "running" and d["ssh"] == "still-here", "the VM did not keep running after its window closed")
        roomy("vm", "force-off", folder, timeout=20)


def shows(window):
    return window.vm().get("shows") or {}


TABLET_AXIS_MAX = 32767  # the guest's pointer device reports positions on this scale, whatever the screen size

DESKTOP_SESSION = """#!/bin/sh
# The bare graphical session the clipboard check runs in: the clipboard helper and nothing else.
spice-vdagent -x -d > /tmp/vdagent.log 2>&1 &
exec sleep 100000
"""


def desktop_features(window, guest, folder):
    """The pointer and the clipboard, which need more in the guest than a text console offers."""
    with step("main:pointer") as d:
        d["install"] = guest.ssh("sudo pacman -Sy --noconfirm --needed evtest 2>&1 | tail -n 2", timeout=240)
        device = guest.g("grep -A6 'QEMU USB Tablet' /proc/bus/input/devices | grep -o 'event[0-9]*' | head -n 1").strip()
        d["device"] = device
        d["absolute"] = shows(window).get("pointer_absolute")
        d["input_devices"] = guest.g("grep -E '^N:|^H:' /proc/bus/input/devices")
        # Listen to the guest's pointer device from inside the guest while the window points at two places,
        # and to every other input device too, in case the pointer turns up somewhere unexpected.
        guest.ssh("sudo sh -c 'for e in /dev/input/event*; do nohup timeout 7 stdbuf -oL evtest $e > /tmp/heard-$(basename $e).txt 2>&1 "
                  "< /dev/null & done'")
        guest.ssh(f"sudo sh -c 'nohup timeout 7 stdbuf -oL evtest /dev/input/{device} > /tmp/pointer.txt 2>&1 < /dev/null &'; sleep 1.5")
        places = []
        for across, down, click in ((0.25, 0.5, ""), (0.75, 0.25, " click")):
            d[f"obeyed_{across}"] = window.tell("pointer", f"{across} {down}{click}")
            time.sleep(1.2)  # the window writes its facts twice a second
            places.append({"asked": [across, down], "guest_pixel": shows(window).get("pointed_at"),
                           "picture_size": window.vm().get("picture_size")})
        time.sleep(4)  # until the listener in the guest has finished
        heard = guest.g("grep -E 'ABS_X|ABS_Y|BTN_LEFT' /tmp/pointer.txt | grep -v 'Event code' | tail -n 60")
        d["heard_tail"] = heard[-1500:]
        d["heard_per_device"] = guest.g("grep -c '^Event: ' /tmp/heard-event*.txt")
        d["qemu_trace"] = sh("grep -aE 'dbus_mouse|input_event|input_mouse_mode' '%s' | tail -n 30" % (folder / "logs" / "qemu.log"))["out"][-3000:]
        d["listener"] = guest.g("wc -l < /tmp/pointer.txt; head -n 4 /tmp/pointer.txt; echo ...; tail -n 4 /tmp/pointer.txt")[-1500:]
        seen = {"ABS_X": [], "ABS_Y": [], "BTN_LEFT": []}
        for line in heard.splitlines():
            for name in seen:
                if f"({name})" in line and "value" in line:
                    with contextlib.suppress(ValueError):
                        seen[name].append(int(line.rsplit("value", 1)[1].strip()))
        d["places"], d["seen"] = places, {name: values[-6:] for name, values in seen.items()}
        expect(d, bool(device), "the guest has no pointer device from roomy")
        expect(d, all(place["guest_pixel"] and place["picture_size"] for place in places),
               "the window could not tell which guest pixel it pointed at")
        if d.get("ok", True) is not False:
            for index, name in ((0, "ABS_X"), (1, "ABS_Y")):
                for place in places:
                    wanted = place["guest_pixel"][index] / place["picture_size"][index] * TABLET_AXIS_MAX
                    place.setdefault("wanted_on_guest_scale", []).append(round(wanted))
                    expect(d, any(abs(value - wanted) <= 0.02 * TABLET_AXIS_MAX for value in seen[name]),
                           f"the guest never saw the pointer at {name} near {round(wanted)} (it saw {seen[name][-6:]})")
            expect(d, 1 in seen["BTN_LEFT"] and 0 in seen["BTN_LEFT"], "the click did not arrive in the guest")
        d["problems"] = window.vm().get("problems")

    with step("main:clipboard") as d:
        d["install"] = guest.ssh("sudo pacman -Sy --noconfirm --needed xorg-server xorg-xinit xclip spice-vdagent 2>&1 | tail -n 3",
                                 timeout=900)
        d["agent_port"] = guest.g("ls /dev/virtio-ports/")
        guest.ssh("cat > /tmp/desk.sh; chmod +x /tmp/desk.sh", input=DESKTOP_SESSION)
        # Nobody is logged in at the guest's screen, so the helper's service is told not to insist on a login session (-X).
        d["service"] = guest.g(
            "sudo systemctl stop spice-vdagentd.service spice-vdagentd.socket; sudo modprobe uinput; sudo mkdir -p /run/spice-vdagentd; "
            "sudo sh -c 'nohup spice-vdagentd -x -X -d > /tmp/vdagentd.log 2>&1 < /dev/null &'; sleep 1; pgrep -a spice-vdagentd")
        d["session"] = guest.g(
            "sudo sh -c 'nohup xinit /tmp/desk.sh -- :0 vt2 > /tmp/xinit.log 2>&1 < /dev/null &'; sleep 7; pgrep -a 'Xorg|spice-vdagent'")
        d["shot"] = window.shot("10-desktop")
        stamp = TS.replace("-", "")
        to_guest, from_guest = f"roomy-to-guest-{stamp}", f"roomy-from-guest-{stamp}"

        d["offered"] = window.tell("clipboard", to_guest)
        time.sleep(1.5)
        d["pasted_in_guest"] = guest.g("sudo env DISPLAY=:0 timeout 6 xclip -o -selection clipboard", timeout=15)
        expect(d, d["pasted_in_guest"].strip() == to_guest, "text offered by this computer could not be pasted in the guest")

        d["copy_in_guest"] = guest.ssh(
            f"printf %s {from_guest} | sudo env DISPLAY=:0 xclip -i -selection clipboard > /dev/null 2>&1; echo copied", timeout=15)
        d["arrived_after_s"] = window.wait("the guest's copy", lambda v: (v.get("shows") or {}).get("guest_copied") == from_guest, timeout=12)
        d["window_got"] = shows(window).get("guest_copied")
        expect(d, d["arrived_after_s"] is not None, "text copied in the guest did not arrive on this computer")
        d["logs"] = guest.g("for f in /tmp/vdagentd.log /tmp/vdagent.log /tmp/xinit.log; do echo \"== $f\"; tail -n 25 $f; done")[-5000:]
        d["qemu_trace"] = sh("grep -aE 'clipboard|vdagent' '%s' | grep -avE 'vdagent_send_msg|vdagent_recv_chunk' | tail -n 60"
                             % (folder / "logs" / "qemu.log"))["out"][-6000:]
        d["problems"] = window.vm().get("problems")
        expect(d, not d["problems"], "the window reported display problems")


def feature_checks(window, guest, folder):
    """Sound, microphone, settings and the dialogs, on the running main VM."""
    with step("main:guest-agent-is-noticed") as d:
        d["after_s"] = window.wait("the guest agent", lambda v: (v.get("shows") or {}).get("guest_agent"), timeout=30)
        expect(d, d["after_s"] is not None, "the window did not notice the guest's agent")

    with step("main:sound") as d:
        d["install"] = guest.ssh("sudo pacman -Sy --noconfirm --needed alsa-utils 2>&1 | tail -n 3", timeout=240)
        # Over SSH nobody is sitting at the guest's screen, so only root may use its sound card.
        d["cards"] = guest.g("sudo aplay -l 2>&1 | head -n 6")
        d["guest_sound_devices"] = guest.g("cat /proc/asound/cards 2>&1; ls -l /dev/snd 2>&1; lsmod | grep -c '^snd'")
        d["guest_sound_hardware"] = guest.g("lspci -nnk 2>/dev/null | grep -iA3 audio; ls /sys/bus/hdaudio/devices 2>&1")
        if "no soundcards" in d["guest_sound_devices"] or "No such file" in d["guest_sound_devices"]:
            # The card is there but nothing in the guest drives it: say so, try the driver by hand, and carry on.
            d["driver_loaded_by_hand"] = guest.ssh(
                "sudo modprobe -a snd_hda_intel snd_hda_codec_generic 2>&1; sleep 2; cat /proc/asound/cards; "
                "sudo dmesg | grep -iE 'snd|hda|audio' | tail -n 12", timeout=30)
        before = shows(window).get("sound")
        d["sound_before"] = before
        # A fresh guest has its output muted and at zero; a tone nobody can hear proves little.
        d["unmute"] = guest.ssh("for c in Master PCM Speaker Headphone; do sudo amixer -q sset $c 70% unmute 2>/dev/null; done; sudo amixer sget Master 2>&1 | tail -n 2")
        # A quiet tone on each of the two channels, about six seconds in all, played through this computer's speakers.
        d["play"] = guest.ssh("sudo timeout 8 speaker-test -t sine -f 440 -c 2 -S 3 -l 1 2>&1 | tail -n 4", timeout=30)
        time.sleep(1.5)
        after = shows(window).get("sound")
        d["sound_after"] = after
        expect(d, before is not None, "the window has no sound support (is libpulse installed in the box?)")
        if before and after:
            expect(d, after["received"] > before["received"], "no sound arrived from the guest")
            expect(d, after["played"] > before["played"], "sound arrived but this computer's sound server did not take it")

    with step("main:microphone") as d:
        d["on"] = roomy("vm", "set", folder, "--microphone", "on")
        time.sleep(1.5)  # the window re-reads the settings twice a second
        before = shows(window).get("sound") or {}
        d["record"] = guest.ssh("sudo arecord -q -d 2 -f S16_LE -r 48000 -c 2 /tmp/mic.wav 2>&1; stat -c %s /tmp/mic.wav", timeout=30)
        time.sleep(0.5)
        after = shows(window).get("sound") or {}
        d["sound_before"], d["sound_after"] = before, after
        d["off"] = roomy("vm", "set", folder, "--microphone", "off")
        expect(d, after.get("microphone_requested", 0) > before.get("microphone_requested", 0),
               "the guest's recording never asked this computer for microphone sound")
        # Whether anything was recorded depends on this computer having a microphone; it is noted, not required.
        d["host_microphone_delivered_sound"] = after.get("microphone_recorded", 0) > before.get("microphone_recorded", 0)

    with step("main:settings-while-running") as d:
        d["disk"] = roomy("vm", "set", folder, "--disk", "30")
        d["disk_in_guest"] = guest.g("lsblk -bdno SIZE /dev/vda").strip()
        expect(d, d["disk_in_guest"] == str(30 << 30), "the guest does not see the larger disk")
        d["network_off"] = roomy("vm", "set", folder, "--network", "off")
        try:
            d["ssh_gone_after_s"] = guest.wait_ssh_gone(timeout=30)
        except TimeoutError:
            d["ssh_gone_after_s"] = None
        # Plugged back in whatever happened, so the remaining steps can still reach the guest.
        d["network_on"] = roomy("vm", "set", folder, "--network", "on")
        try:
            d["ssh_back_after_s"] = guest.wait_ssh(timeout=60)
        except TimeoutError:
            d["ssh_back_after_s"] = None
        expect(d, d["ssh_gone_after_s"] is not None and d["ssh_back_after_s"] is not None,
               "unplugging and replugging the network did not behave as expected")
        d["processors"] = roomy("vm", "set", folder, "--cpus", "6")
        expect(d, "takes effect the next time" in d["processors"]["out"], "a processor change was not reported as waiting")
        d["window_says_waiting_after_s"] = window.wait(
            "the waiting banner", lambda v: (v.get("shows") or {}).get("waiting_for_next_boot") == ["processors"], timeout=10)
        d["shows"] = shows(window)
        d["shot"] = window.shot("7-waiting-for-next-boot")
        expect(d, d["window_says_waiting_after_s"] is not None, "the window does not say that a change is waiting for the next boot")
        expect(d, "processors" in (d["shows"].get("banner") or ""), "the banner does not name the waiting change")

    with step("main:dialogs") as d:
        for action, shot in (("settings", "8-settings"), ("guest-setup", "9-guest-setup")):
            d[f"{action}_opened"] = window.tell("action", action)
            time.sleep(2)
            d[f"{action}_shot"] = window.shot(shot)
            d[f"{action}_closed"] = window.tell("action", "close-dialog")
            time.sleep(0.7)
            expect(d, d[f"{action}_opened"] and d[f"{action}_shot"], f"the {action} dialog could not be opened and pictured")
        expect(d, window.alive(), "the window did not survive opening its dialogs")


def other_card_warning(window, folder):
    """A GPU VM drawing on another card than the window must be flagged, not shown scrambled in silence."""
    with step("gpu:other-card-is-flagged") as d:
        mine = window.facts().get("window_render_node")
        others = sorted(n for n in (str(p) for p in Path("/dev/dri").glob("renderD*")) if n != mine)
        d["window_card"], d["other_cards"] = mine, others
        if not mine or not others:
            d["skipped"] = "this computer has one graphics card"
            return
        d["start"] = sh(["env", f"ROOMY_RENDER_NODE={others[0]}", str(ec.ROOMY), "vm", "start", str(folder)], timeout=120)
        d["flagged_after_s"] = window.wait("the warning", lambda v: (v.get("shows") or {}).get("other_graphics_card"), timeout=20)
        time.sleep(2)
        d["shows"] = shows(window)
        d["shot"] = window.shot("3-gpu-other-card")
        roomy("vm", "force-off", folder)
        window.wait("off", lambda v: v.get("state") == "off", timeout=20)
        expect(d, d["flagged_after_s"] is not None, "the window did not warn that the VM draws on another graphics card")


def gpu_vm(key, key_pub):
    with step("gpu:create") as d:
        folder, port, created = ec.create_vm("gui-gpu", key_pub, ["--gpu"])
        d["create"] = created
    if not ec.REPORT["steps"][-1]["ok"]:
        return
    guest = ec.Guest(folder, port, key)
    window = Window("gpu", vm=folder)
    try:
        with step("gpu:screen-from-graphics-memory") as d:
            d["first_frame_after_s"] = window.wait("the first picture", lambda v: v.get("frames_drawn", 0) > 0, timeout=90)
            d["boot_to_ssh_s"] = guest.wait_ssh()
            time.sleep(8)
            d["typed"] = window.type("gpulab")
            d["console"] = console_text(guest)
            d["vm"] = window.vm()
            d["shot"] = window.shot("1-gpu-running")
            time.sleep(1)
            d["picture_ok"], d["bright_area"] = text_is_top_left(window, "1-gpu-running")
            expect(d, d["picture_ok"],
                   f"the GPU VM's screen is scrambled or upside down (bright area {d['bright_area']}, expected the top-left)")
            expect(d, d["first_frame_after_s"] is not None, "the GPU VM's screen never appeared in the window")
            expect(d, d["vm"].get("screen_kind") == "graphics memory",
                   f"expected the screen to come from graphics memory, got {d['vm'].get('screen_kind')!r}")
            expect(d, "gpulab" in d["console"], "the keyboard did not reach the GPU VM")
            expect(d, not d["vm"].get("problems"), "the window reported display problems")
        with step("gpu:pause-resume-off") as d:
            d["pause"] = roomy("vm", "pause", folder)
            d["window_saw_paused_after_s"] = window.wait("paused", lambda v: v.get("state") == "paused", timeout=15)
            d["shot"] = window.shot("2-gpu-paused")
            d["resume"] = roomy("vm", "resume", folder)
            window.wait("running", lambda v: v.get("state") == "running", timeout=15)
            d["force_off"] = roomy("vm", "force-off", folder)
            d["window_saw_off_after_s"] = window.wait("off", lambda v: v.get("state") == "off", timeout=20)
            expect(d, d["window_saw_paused_after_s"] is not None and d["window_saw_off_after_s"] is not None,
                   "the window did not follow the GPU VM's state")
        other_card_warning(window, folder)
    finally:
        with step("gpu:window-exit") as d:
            d["window"] = window.stop()
            expect(d, d["window"]["exit_status"] == 0, "the window did not exit cleanly")
        roomy("vm", "force-off", folder, timeout=20)


def package():
    """Build the Arch package from this checkout and look inside it. Nothing is installed."""
    with step("package") as d:
        if not shutil.which("makepkg"):
            d["skipped"] = "makepkg is not available here"
            return
        build, dest = ec.WORK / "makepkg", ec.WORK / "package"
        r = sh("cd packaging/arch && BUILDDIR='%s' PKGDEST='%s' makepkg -f --nodeps --noconfirm > '%s' 2>&1; echo rc=$?"
               % (build, dest, ec.OUT / "package.log"), timeout=1800)
        d["makepkg"] = r["out"].strip()
        d["tail"] = sh(["tail", "-n", "15", str(ec.OUT / "package.log")])["out"]
        packages = sorted(dest.glob("roomy-*.pkg.tar.*")) if dest.is_dir() else []
        d["contents"] = sh(["pacman", "-Qlp", str(packages[0])])["out"] if packages else ""
        d["desktop_file"] = sh("desktop-file-validate packaging/com.mudlej.Roomy.desktop 2>&1")["out"].strip()
        expect(d, "rc=0" in d["makepkg"] and packages, "the package could not be built; see package.log")
        for path in ("/usr/bin/roomy", "/usr/bin/roomy-gui", "/usr/share/applications/com.mudlej.Roomy.desktop",
                     "/usr/share/icons/hicolor/scalable/apps/com.mudlej.Roomy.svg"):
            expect(d, path in d["contents"], f"the package lacks {path}")


def main():
    ec.start_run(KIND)
    session = None
    try:
        if os.environ.get("ROOMY_LAB_SKIP_BUILD") == "1":  # for dry runs of this script itself
            session = run_checks()
            return
        if DISTRO:
            if on_this_distribution():
                session = run_checks()
            return
        with step("build") as d:
            # Keep the full compiler output in build.log.
            r = sh("cargo build --release --workspace > '%s' 2>&1; echo rc=$?" % (ec.OUT / "build.log"), timeout=1800)
            d["cargo"] = r["out"].strip()
            d["tail"] = sh(["tail", "-n", "40", str(ec.OUT / "build.log")])["out"]
            expect(d, "rc=0" in d["cargo"] and GUI.is_file(), "the build failed; see build.log")
        build_ok = ec.REPORT["steps"][-1]["ok"]
        if APPIMAGE and build_ok:
            with step("appimage") as d:
                image = SINGLE_FILE
                r = sh("just appimage > '%s' 2>&1; echo rc=$?" % (ec.OUT / "appimage.log"), timeout=1800)
                d["script"] = r["out"].strip()
                d["tail"] = sh(["tail", "-n", "6", str(ec.OUT / "appimage.log")])["out"]
                d["bytes"] = image.stat().st_size if image.is_file() else None
                expect(d, "rc=0" in d["script"] and d["bytes"], "the single file could not be built; see appimage.log")
                if d["bytes"]:
                    if not (shutil.which("fusermount3") or shutil.which("fusermount")):
                        d["helper_installed"] = sh("sudo -n pacman -S --needed --noconfirm fuse3 2>&1 | tail -n 2", timeout=300)["out"]
                    use_single_file(d, image)
            build_ok = ec.REPORT["steps"][-1]["ok"]
        if QUICK:
            if build_ok:
                session = run_checks()
            return
        with step("tests") as d:
            r = sh("cargo test --workspace > '%s' 2>&1; echo rc=$?" % (ec.OUT / "tests.log"), timeout=1800)
            d["cargo"] = r["out"].strip()
            d["results"] = sh("grep -E 'test result|FAILED|panicked' '%s'" % (ec.OUT / "tests.log"))["out"]
            expect(d, "rc=0" in d["cargo"], "tests failed on this machine; see tests.log")
        if build_ok:
            session = run_checks()
    except KeyboardInterrupt:
        log("interrupted")
        ec.REPORT["interrupted"] = True
    finally:
        finish(session)


def use_single_file(d, image):
    """From here on `roomy` is the single-file release. Records in `d` how it had to be started."""
    # The file mounts itself with the system's FUSE helper, as it will on a user's computer.
    # A box may lack the helper; without it the file can only unpack itself on every start.
    d["fuse"] = sh("ls -l /dev/fuse 2>&1; command -v fusermount3 fusermount")["out"]
    ec.ROOMY = image
    os.environ.pop("APPIMAGE_EXTRACT_AND_RUN", None)
    first = roomy("--help", timeout=30)
    d["mounted_run"] = {"rc": first["rc"], "err": first["err"][-400:]}
    if first["rc"] != 0:
        os.environ["APPIMAGE_EXTRACT_AND_RUN"] = "1"
        second = roomy("--help", timeout=30)
        d["unpacked_run"] = {"rc": second["rc"], "err": second["err"][-400:]}
        expect(d, second["rc"] == 0, "the single file does not start, mounted or unpacked")
    d["mode_tested"] = "unpacks itself on every start" if os.environ.get("APPIMAGE_EXTRACT_AND_RUN") else "mounts itself"


def on_this_distribution():
    """The single-file release on a distribution it was not built on, before roomy has installed anything."""
    with step("distro") as d:
        d["os_release"] = sh("grep -E '^(ID|ID_LIKE|VERSION_ID|VERSION_CODENAME|VARIANT_ID|PRETTY_NAME)=' /etc/os-release")["out"]
        d["glibc"] = sh("ldd --version 2>&1 | head -n 1")["out"].strip()
        d["single_file"] = str(SINGLE_FILE)
        if not SINGLE_FILE.is_file():
            raise RuntimeError(f"{SINGLE_FILE} is missing: build it first with 'just appimage' inside the roomy-lab box")
        d["built"] = time.strftime("%Y-%m-%d %H:%M", time.localtime(SINGLE_FILE.stat().st_mtime))
        use_single_file(d, SINGLE_FILE)
        # What the doctor makes of this system as it is, and what setup proposes, before anything is installed.
        before = roomy("doctor", "--report-file", ec.OUT / "doctor-before.txt")
        d["doctor_before_rc"] = before["rc"]
        d["problems_before"] = [line for line in before["out"].splitlines() if line.startswith(("[FAIL]", "[WARN]", "[SKIP]"))][:20]
        proposed = sh([str(ec.ROOMY), "setup"], timeout=120, input="")
        (ec.OUT / "setup-proposed.txt").write_text(proposed["out"] + "\n" + proposed["err"])
        d["proposed"] = [line for line in proposed["out"].splitlines() if line.startswith(("  ", "Note:", "roomy c", "roomy d"))][:12]
    return ec.REPORT["steps"][-1]["ok"]


def run_checks():
    """Everything after the build. Returns the fallback desktop session, if one was started."""
    session = None
    with step("setup") as d:
        # Let roomy install whatever the box lacks.
        r = roomy("setup", "--yes", timeout=1800)
        (ec.OUT / "setup.txt").write_text(r["out"] + "\n" + r["err"])
        d["rc"], d["secs"], d["tail"] = r["rc"], r["secs"], (r["out"] + "\n" + r["err"])[-1500:]
        expect(d, r["rc"] == 0, "roomy setup did not leave this box ready; see its output")
    with step("doctor") as d:
        r = roomy("doctor", "--report-file", ec.OUT / "doctor.txt")
        d["rc"] = r["rc"]
        d["window_line"] = [line for line in r["out"].splitlines() if "window" in line]
        expect(d, r["rc"] == 0, "roomy doctor reports problems; see doctor.txt")
        expect(d, any("[ OK ]" in line for line in d["window_line"]), "roomy doctor says the window cannot start")
    with step("session") as d:
        d["wayland"], d["x11"] = os.environ.get("WAYLAND_DISPLAY"), os.environ.get("DISPLAY")
        if not d["wayland"] and not d["x11"]:
            # No desktop reachable: fall back to an invisible one, which still exercises everything but GPU drawing.
            session = subprocess.Popen(["weston", "--backend=headless", "--socket=roomy-lab", "--width=1600", "--height=1000"],
                                       stdout=open(ec.OUT / "weston.log", "w"), stderr=subprocess.STDOUT)
            time.sleep(2)
            os.environ["WAYLAND_DISPLAY"] = "roomy-lab"
            d["fallback"] = "headless weston"
            expect(d, session.poll() is None, "no desktop session, and a headless one could not be started")
    if not QUICK:
        empty_library()
    download_window()
    with step("prepare"):
        ec.require_base_image()
        key, key_pub = ec.new_key(ec.WORK)
    if ec.REPORT["steps"][-1]["ok"]:
        main_vm(key, key_pub)
        if not QUICK:
            gpu_vm(key, key_pub)
    if not QUICK:
        package()
    return session


def finish(session):
    def stop():
        sh(["pkill", "-f", str(GUI)])
        if session:
            session.terminate()

    pictures = sorted(str(p.relative_to(ec.OUT)) for p in ec.OUT.glob("*/*.png"))
    ec.finish_run(f"roomy window check {TS}" + (f" on {distribution()}, single-file release" if DISTRO else ""),
                  stop=stop, extra_lines=[f"pictures: {', '.join(pictures) or 'none'}"])


if __name__ == "__main__":
    sys.exit(main())
