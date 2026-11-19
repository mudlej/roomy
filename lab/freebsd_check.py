#!/usr/bin/env python3
"""Lab run for a FreeBSD guest, against real QEMU and a real desktop session.
Runs inside the Arch lab box (distrobox.ini).

Finds out what a FreeBSD guest can and cannot do in roomy, instead of assuming it: boots FreeBSD's
own ready-made disk image in roomy's window, types at its console, installs the guest helper with
the very command the window shows, and checks the clock after Pause and Suspend, sound, the
pointer and shutting down. It also pictures the parts of the window that differ for a FreeBSD
guest: the download window's BSD list, the "New Virtual Machine" dialog, Settings and Guest Setup.
A window will appear on your screen; leave it alone while this runs.

The first run downloads FreeBSD's disk image (about 650 MB) into work/lab/cache.
Everything is written to work/lab/results/freebsd-<timestamp>/ (also work/lab/results/latest-freebsd).

Usage: just lab-freebsd        (about 10 minutes; the guest's first boot takes most of them)
"""
import contextlib
import hashlib
import os
import re
import sys
import time

import engine_check as ec
import gui_check as gc

log, sh, roomy, step, expect = ec.log, ec.sh, ec.roomy, ec.step, ec.expect

RELEASES = "https://download.freebsd.org/releases/VM-IMAGES"
# The checks need root, which FreeBSD's SSH service refuses until told otherwise.
FIRST_BOOT = """runcmd:
  - mkdir -p /root/.ssh
  - cp /home/freebsd/.ssh/authorized_keys /root/.ssh/authorized_keys
  - echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config
  - service sshd restart
"""


def ok_so_far():
    return ec.REPORT["steps"][-1]["ok"]


def in_view(window, d):
    """Whether the desktop lets the VM's window draw; nothing that needs its picture can be checked otherwise."""
    hidden = bool(window.vm().get("window_suspended"))
    expect(d, not hidden, "roomy's window was covered, minimised or on another desktop, so it drew nothing: "
                          "run again and keep the window in view")
    return not hidden


def disk_image(d):
    """FreeBSD's ready-made disk image of its current release, downloaded and checked once."""
    listed = roomy("image", "list", "freebsd", timeout=120)
    d["listed"] = listed["out"]
    found = re.search(r"FreeBSD-([0-9.]+)-RELEASE-amd64-disc1\.iso", listed["out"])
    if not found:
        raise RuntimeError(f"roomy did not find FreeBSD's current release: {listed}")
    d["version"] = version = found.group(1)
    name = f"FreeBSD-{version}-RELEASE-amd64-BASIC-CLOUDINIT-ufs.qcow2"
    image = ec.DATA / "cache" / name
    if image.is_file():
        d["cached"] = True
        return image
    image.parent.mkdir(parents=True, exist_ok=True)
    folder = f"{RELEASES}/{version}-RELEASE/amd64/Latest"
    packed = image.parent / f"{name}.xz"
    fetched = sh(["curl", "-fL", "--proto", "=https", "--retry", "3", "-C", "-", "-sS", "-o", str(packed), f"{folder}/{name}.xz"],
                 timeout=3600)
    sums = sh(["curl", "-fsSL", "--proto", "=https", f"{folder}/CHECKSUM.SHA256"], timeout=60)
    wanted = re.search(r"SHA256 \(%s\.xz\) = ([0-9a-f]{64})" % re.escape(name), sums["out"])
    if fetched["rc"] != 0 or not wanted:
        raise RuntimeError(f"FreeBSD's disk image could not be downloaded: {fetched} {sums['err']}")
    digest = hashlib.sha256()
    with open(packed, "rb") as file:
        for block in iter(lambda: file.read(1 << 20), b""):
            digest.update(block)
    if digest.hexdigest() != wanted.group(1):
        packed.unlink()
        raise RuntimeError("the downloaded disk image does not match FreeBSD's checksum; it was removed")
    # Under another name first, so that an image by its own name is always a whole one.
    partial = image.parent / f"{name}.unpacking"
    unpacked = sh(f"xz -dc '{packed}' > '{partial}'", timeout=900)
    if unpacked["rc"] != 0:
        partial.unlink(missing_ok=True)
        raise RuntimeError(f"the disk image could not be unpacked: {unpacked}")
    os.replace(partial, image)
    packed.unlink()
    d["downloaded_bytes"] = image.stat().st_size
    return image


def create_dialog():
    """The "New Virtual Machine" dialog as an image of FreeBSD leads to it."""
    with step("window:create-dialog") as d:
        window = gc.Window("create", seconds=60)
        try:
            window.wait_for_facts()
            d["opened"] = window.tell("open", "create:freebsd")
            time.sleep(2)
            d["shot"] = window.shot("create-freebsd")
            expect(d, d["opened"] and d["shot"], "the dialog could not be opened and pictured")
        finally:
            d["window"] = window.stop()
        expect(d, d["window"]["exit_status"] == 0, "the window did not exit cleanly")
        expect(d, not d["window"]["gtk_complaints"], "the window toolkit printed warnings; see create-window.log")


def guest_checks(image, key, key_pub):
    with step("freebsd:create") as d:
        folder, port, created = ec.create_vm("freebsd", key_pub, system="freebsd", image=image, first_boot=FIRST_BOOT)
        d["create"] = created
        d["config"] = (folder / "roomy.toml").read_text()
        expect(d, "clipboard = false" in d["config"], "clipboard sharing was switched on for a guest that cannot use it")
        refused = roomy("vm", "set", folder, "--gpu", "on")
        d["gpu_refused"] = refused["err"]
        expect(d, refused["rc"] != 0 and "FreeBSD guest cannot use GPU acceleration" in refused["err"],
               "GPU acceleration was not refused for a FreeBSD guest")
    if not ok_so_far():
        return
    guest = ec.Guest(folder, port, key, user="root")
    window = gc.Window("freebsd", vm=folder, seconds=1800)
    try:
        with step("freebsd:boots") as d:
            d["first_frame_after_s"] = window.wait("the first picture", lambda v: v.get("frames_drawn", 0) > 0, timeout=120)
            expect(d, d["first_frame_after_s"] is not None, "the guest's screen never appeared in the window")
            d["shot_booting"] = window.shot("1-booting")
            # The first boot grows the disk, makes keys, installs updates and may restart once.
            d["boot_to_ssh_s"] = guest.wait_ssh(timeout=1200)
            t0 = time.monotonic()
            while time.monotonic() - t0 < 600 and "settled" not in guest.g("test -e /firstboot || echo settled", timeout=15):
                time.sleep(5)
            d["first_boot_finished_after_s"] = round(time.monotonic() - t0, 1)
            time.sleep(20)  # a restart asked for by the updates begins right after the first boot ends
            d["ssh_after_first_boot_s"] = guest.wait_ssh(timeout=300)
            d["system"] = guest.g("freebsd-version -kru; uname -m; sysctl -n kern.vm_guest hw.ncpu hw.physmem")
            d["first_boot_log"] = guest.g("tail -n 30 /var/log/nuageinit.log")[-3000:]
            d["devices"] = guest.g("pciconf -l | cut -c1-90")[-4000:]
            d["drivers"] = guest.g("kldstat | cut -c1-80; dmesg | grep -iE 'virtio|vtnet|vtblk|vtcon|efifb|VT\\(|hdac|pcm[0-9]|usbhid|hms|hmt|ums|uhid|tablet' "
                                   "| cut -c1-160 | tail -n 50")[-6000:]
            d["status_cli"] = guest.status()
            d["vm"] = window.vm()
            expect(d, d["status_cli"] == "running", "the VM is not running")
            expect(d, not d["vm"].get("problems"), "the window reported display problems")
        if guest.ssh("true", timeout=10)["rc"] != 0:
            log("the guest cannot be reached; skipping the checks that need it")
            window.shot("guest-not-reached")
            return

        with step("freebsd:keyboard") as d:
            # root has no password on this image.
            d["typed_login"] = window.type("root\n")
            time.sleep(4)
            d["typed_command"] = window.type("touch roomylab\n")
            time.sleep(2)
            d["shot"] = window.shot("2-typed")
            d["file"] = guest.g("ls -l /root/roomylab")
            in_view(window, d)
            expect(d, "No such file" not in d["file"] and "roomylab" in d["file"],
                   "text typed in the window did not arrive at the guest's console; see 2-typed-vm.png")

        with step("freebsd:screen-size") as d:
            vm = window.vm()
            d["picture_size"], d["window_size"], d["screen_kind"] = vm.get("picture_size"), vm.get("window_size"), vm.get("screen_kind")
            d["console"] = guest.g("sysctl -n kern.vt.fb.default_mode 2>/dev/null; kenv -q screen.width screen.height 2>/dev/null; "
                                   "vidcontrol -i mode < /dev/ttyv0 2>&1 | head -n 5")
            expect(d, bool(d["picture_size"]), "the window has no picture of the guest's screen")

        with step("freebsd:guest-agent") as d:
            command = gc.shows(window).get("install_helpers")
            d["command"] = command
            expect(d, bool(command), "the window offers no command to install the helpers")
            if command:
                ran = guest.ssh(command, timeout=900)
                d["install"] = {"rc": ran["rc"], "out": ran["out"][-2500:], "err": ran["err"][-1500:], "secs": ran["secs"]}
                expect(d, ran["rc"] == 0, "the command the window shows for installing the helpers failed in the guest")
            d["service"] = guest.g("service qemu-guest-agent status; ls /dev/vtcon; kldstat | grep -c virtio_console; "
                                   "sysrc -n qemu_guest_agent_enable; grep virtio_console /boot/loader.conf")
            d["noticed_after_s"] = window.wait("the guest agent", lambda v: (v.get("shows") or {}).get("guest_agent"), timeout=60)
            expect(d, d["noticed_after_s"] is not None, "the window did not notice the guest's agent")

        with step("freebsd:clock-after-pause") as d:
            d["lag_before_s"] = ec.clock_lag(guest)
            d["pause"] = roomy("vm", "pause", folder)
            d["window_saw_paused_after_s"] = window.wait("paused", lambda v: v.get("state") == "paused", timeout=15)
            time.sleep(20)
            d["resume"] = roomy("vm", "resume", folder)
            window.wait("running", lambda v: v.get("state") == "running", timeout=15)
            time.sleep(3)
            d["lag_after_s"] = ec.clock_lag(guest)
            expect(d, d["window_saw_paused_after_s"] is not None, "the window did not show the VM as paused")
            expect(d, d["lag_after_s"] is not None and abs(d["lag_after_s"]) < 3,
                   f"the guest's clock is {d['lag_after_s']} s behind after a 20 s pause; the agent should have set it")

        with step("freebsd:suspend-and-continue") as d:
            # Lives in memory only: it is still there if the guest carried on, and gone if it started afresh.
            d["marker"] = guest.g("mkdir -p /tmp/lab && mount -t tmpfs tmpfs /tmp/lab && touch /tmp/lab/alive && ls /tmp/lab")
            frames_before = window.vm().get("frames_drawn", 0)
            d["suspend"] = roomy("vm", "suspend", folder, timeout=600)
            d["window_saw_suspended_after_s"] = window.wait("suspended", lambda v: v.get("state") == "suspended", timeout=60)
            time.sleep(15)
            d["start"] = roomy("vm", "start", folder)
            d["window_saw_running_after_s"] = window.wait("running", lambda v: v.get("state") == "running", timeout=60)
            d["new_picture_after_s"] = window.wait("a new picture", lambda v: v.get("frames_drawn", 0) > frames_before, timeout=30)
            with contextlib.suppress(TimeoutError):
                d["ssh_after_s"] = guest.wait_ssh(timeout=90)
            d["marker_after"] = guest.g("ls /tmp/lab")
            time.sleep(2)
            d["lag_after_s"] = ec.clock_lag(guest)
            d["shot"] = window.shot("3-continued")
            if in_view(window, d):
                expect(d, d["new_picture_after_s"] is not None, "the screen did not come back after continuing")
            expect(d, d["suspend"]["rc"] == 0 and d["window_saw_suspended_after_s"] is not None, "the VM could not be suspended")
            expect(d, "alive" in d["marker_after"], "the guest did not carry on where it was suspended")
            expect(d, d["lag_after_s"] is not None and abs(d["lag_after_s"]) < 3,
                   f"the guest's clock is {d['lag_after_s']} s behind after being suspended for 15 s")

        with step("freebsd:sound") as d:
            d["cards"] = guest.g("cat /dev/sndstat")
            before = gc.shows(window).get("sound")
            d["tone"] = guest.ssh("beep -D 1500 -F 440 -g 20 2>&1; echo rc=$?", timeout=30)["out"]
            d["silence"] = guest.ssh("dd if=/dev/zero of=/dev/dsp bs=4k count=100 2>&1 | tail -n 1", timeout=30)["out"]
            time.sleep(1.5)
            after = gc.shows(window).get("sound")
            d["sound_before"], d["sound_after"] = before, after
            expect(d, "pcm" in d["cards"], "FreeBSD found no sound card in the VM")
            expect(d, before is not None, "the window has no sound support (is libpulse installed in the box?)")
            if before and after:
                expect(d, after["received"] > before["received"], "no sound arrived from the guest")

        with step("freebsd:pointer") as d:
            d["install"] = guest.ssh("pkg install -y evtest 2>&1 | tail -n 3", timeout=600)["out"]
            d["input_devices"] = guest.g("sysctl kern.evdev.input 2>&1 | grep -E 'name|phys' | cut -c1-120")
            number = re.search(r"kern\.evdev\.input\.(\d+)\.name: .*Tablet", d["input_devices"])
            d["absolute"] = gc.shows(window).get("pointer_absolute")
            expect(d, bool(number), "FreeBSD shows no pointer device for roomy's tablet; a desktop in it would have no usable pointer")
            if number and in_view(window, d):
                device = f"/dev/input/event{number.group(1)}"
                guest.ssh(f"nohup timeout 7 stdbuf -oL evtest {device} > /tmp/pointer.txt 2>&1 < /dev/null &", timeout=10)
                time.sleep(1.5)
                gc.pointer_reaches_guest(d, window, guest)
                d["what_the_device_says"] = guest.g("head -n 25 /tmp/pointer.txt")[-2500:]

        with step("freebsd:dialogs") as d:
            in_view(window, d)
            for action, shot in (("settings", "4-settings"), ("guest-setup", "5-guest-setup")):
                d[f"{action}_opened"] = window.tell("action", action)
                time.sleep(2)
                d[f"{action}_shot"] = window.shot(shot)
                d[f"{action}_closed"] = window.tell("action", "close-dialog")
                time.sleep(0.7)
                expect(d, d[f"{action}_opened"] and d[f"{action}_shot"], f"the {action} dialog could not be opened and pictured")
            expect(d, window.alive(), "the window did not survive opening its dialogs")

        with step("freebsd:shutdown") as d:
            d["shutdown"] = roomy("vm", "shutdown", folder)
            d["window_saw_off_after_s"] = window.wait("off", lambda v: v.get("state") == "off", timeout=180)
            expect(d, d["window_saw_off_after_s"] is not None, "FreeBSD did not shut down when asked to")
    finally:
        with step("freebsd:window-exit") as d:
            d["window"] = window.stop()
            expect(d, d["window"]["exit_status"] == 0, "the window did not exit cleanly")
            expect(d, not d["window"]["gtk_complaints"], "the window toolkit printed warnings; see freebsd-window.log")
        roomy("vm", "force-off", folder, timeout=20)


def main():
    ec.start_run("freebsd")
    try:
        if os.environ.get("ROOMY_LAB_SKIP_BUILD") != "1" and not gc.build():  # set for dry runs of this script itself
            return
        with step("doctor") as d:
            r = roomy("doctor", "--report-file", ec.OUT / "doctor.txt")
            d["rc"] = r["rc"]
            expect(d, r["rc"] == 0, "roomy doctor reports problems; see doctor.txt (just lab-gui quick sets the box up)")
        if not ok_so_far():
            return
        gc.download_window("freebsd")
        create_dialog()
        with step("prepare") as d:
            image = disk_image(d)
            key, key_pub = ec.new_key(ec.WORK)
        if ok_so_far():
            guest_checks(image, key, key_pub)
    except KeyboardInterrupt:
        log("interrupted")
        ec.REPORT["interrupted"] = True
    finally:
        pictures = sorted(str(p.relative_to(ec.OUT)) for p in ec.OUT.glob("*/*.png"))
        ec.finish_run(f"roomy FreeBSD guest check {ec.TS}", stop=lambda: sh(["pkill", "-f", str(gc.GUI)]),
                      extra_lines=[f"pictures: {', '.join(pictures) or 'none'}"])


if __name__ == "__main__":
    sys.exit(main())
