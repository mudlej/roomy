#!/usr/bin/env python3
"""roomy lab: drive the real `roomy` program against real QEMU and a real guest.
Runs inside the Arch lab box (distrobox.ini) only.

Builds roomy, creates a VM from the cached Arch cloud image, and walks it
through its lifecycle (start, shared folder, pause, suspend, continue,
restart, shut down, force off, plus a GPU-accelerated VM and a killed
supervisor). Everything observed is written to work/lab/results/engine-<time>/.
gui_check.py, gpu_check.py and probe.py import their shared helpers from here.

Usage: just lab-engine        (about 6 minutes)
Set ROOMY_LAB_KEEP=1 to keep the test VMs in work/lab/scratch/ afterwards.
"""
import contextlib
import json
import os
import shutil
import socket
import subprocess
import sys
import time
import traceback
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
# Everything a run downloads or writes lives under the checkout's work/ folder, which git ignores.
DATA = REPO / "work" / "lab"
TS = time.strftime("%Y%m%d-%H%M%S")
OUT = WORK = None  # set by start_run()
BASE_IMAGE = Path(os.environ.get("ROOMY_LAB_IMAGE") or DATA / "cache" / "Arch-Linux-x86_64-cloudimg.qcow2")
# ROOMY_LAB_PROGRAM: another build of roomy, for dry runs of these scripts.
ROOMY = Path(os.environ.get("ROOMY_LAB_PROGRAM") or REPO / "target" / "release" / "roomy")
KEEP = os.environ.get("ROOMY_LAB_KEEP") == "1"

REPORT = {"started": TS, "steps": []}
VMS = []          # folders of every VM created, for cleanup
_log_file = None


def log(msg):
    line = f"[{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    if _log_file:
        _log_file.write(line + "\n")
        _log_file.flush()


def log_to(path):
    """From now on, also append every log line to `path`."""
    global _log_file
    _log_file = open(path, "a")


def link_latest(name, folder):
    """Point work/lab/results/<name> at `folder`, the newest run of its kind."""
    latest = folder.parent / name
    with contextlib.suppress(OSError):
        if latest.is_symlink():
            latest.unlink()
        latest.symlink_to(folder.name)


def start_run(kind):
    """Begin a run: results go to work/lab/results/<kind>-<time>, scratch files to work/lab/scratch/<kind>-<time>."""
    global OUT, WORK
    OUT = DATA / "results" / f"{kind}-{TS}"
    WORK = DATA / "scratch" / f"{kind}-{TS}"
    OUT.mkdir(parents=True)
    WORK.mkdir(parents=True)
    # Lab VMs must not show up in (or disturb) the library of the person running this.
    os.environ["XDG_CONFIG_HOME"] = str(WORK / "config")
    log_to(OUT / "run.log")
    link_latest(f"latest-{kind}", OUT)
    log(f"results: {OUT}")


def step_note(s):
    return str(s.get("error") or s["data"].get("why") or "")[:150]


def finish_run(title, stop=None, note=step_note, extra_lines=()):
    """End a run, whatever happened: force off its VMs and keep their logs, call `stop` for anything else
    still running, write report.json and SUMMARY.txt (`note(step)` follows each step's name), and remove
    the scratch folder unless ROOMY_LAB_KEEP=1."""
    for folder in VMS:
        with contextlib.suppress(Exception):
            roomy("vm", "force-off", folder, timeout=20)
            collect_logs(folder, folder.name)
    if stop:
        stop()
    time.sleep(1)
    REPORT["processes_at_end"] = processes()
    REPORT["finished"] = time.strftime("%Y%m%d-%H%M%S")
    save()
    lines = [title, ""]
    for s in REPORT["steps"]:
        verdict = {True: "ok", False: "FAILED"}.get(s["ok"], "interrupted")
        lines.append(f"{verdict:11} {s.get('secs', ''):>7}  {s['name']}  {note(s)}")
    failed = sum(1 for s in REPORT["steps"] if s["ok"] is False)
    lines += ["", f"{len(REPORT['steps']) - failed} ok, {failed} failed", *extra_lines,
              f"processes left at the end: {REPORT['processes_at_end'] or 'none'}"]
    (OUT / "SUMMARY.txt").write_text("\n".join(lines) + "\n")
    print("\n" + "\n".join(lines))
    if not KEEP:
        shutil.rmtree(WORK, ignore_errors=True)
        with contextlib.suppress(OSError):
            (DATA / "scratch").rmdir()
    log(f"done. Everything is in {OUT}")


def save():
    tmp = OUT / "report.json.tmp"
    tmp.write_text(json.dumps(REPORT, indent=1, default=str))
    os.replace(tmp, OUT / "report.json")


def sh(cmd, timeout=60, input=None):
    """Run a command; returns {rc, out, err, secs}. Never raises."""
    t0 = time.monotonic()
    try:
        p = subprocess.run(cmd, shell=isinstance(cmd, str), capture_output=True, text=True, errors="replace",
                           timeout=timeout, input=input)
        r = {"rc": p.returncode, "out": p.stdout[-6000:], "err": p.stderr[-6000:]}
    except subprocess.TimeoutExpired as e:
        r = {"rc": None, "timeout_s": timeout, "out": str(e.stdout or "")[-3000:], "err": str(e.stderr or "")[-3000:]}
    except OSError as e:
        r = {"rc": None, "out": "", "err": repr(e)}
    r["secs"] = round(time.monotonic() - t0, 2)
    return r


def roomy(*args, timeout=180):
    r = sh([str(ROOMY), *map(str, args)], timeout=timeout)
    r["cmd"] = "roomy " + " ".join(map(str, args[:2]))
    r["out"], r["err"] = r["out"].strip(), r["err"].strip()
    return r


@contextlib.contextmanager
def step(name):
    rec = {"name": name, "ok": None, "data": {}}
    REPORT["steps"].append(rec)
    log(f">>> {name}")
    t0 = time.monotonic()
    try:
        yield rec["data"]
        rec["ok"] = rec["data"].pop("ok", True)
    except Exception as e:
        rec["ok"] = False
        rec["error"] = repr(e)
        rec["trace"] = traceback.format_exc()
    finally:
        rec["secs"] = round(time.monotonic() - t0, 2)
        save()
        verdict = {True: "ok", False: "FAILED"}.get(rec["ok"], "interrupted")
        log(f"<<< {name}: {verdict} ({rec['secs']}s) {rec.get('error', '') or rec['data'].get('why', '')}")


def expect(d, condition, why):
    """Mark the current step failed (without aborting it) when a condition does not hold."""
    if not condition:
        d["ok"] = False
        d.setdefault("why", why)


def ssh_command(key, port, cmd):
    """The ssh command line that runs `cmd` in a lab guest whose SSH port is forwarded to `port` here."""
    return ["ssh", "-F", "/dev/null", "-i", key, "-p", str(port), "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
            "-o", "ConnectTimeout=5", "-o", "LogLevel=ERROR", "arch@127.0.0.1", cmd]


def clock_lag(guest):
    """Seconds the guest's wall clock is behind this computer's (includes about 0.1s of SSH latency)."""
    host = time.time()
    try:
        return round(host - float(guest.g("date +%s.%N").split()[0]), 2)
    except (ValueError, IndexError):
        return None


class Guest:
    def __init__(self, folder, port, key):
        self.folder, self.port, self.key = str(folder), port, key

    def ssh(self, cmd, timeout=30, input=None):
        return sh(ssh_command(self.key, self.port, cmd), timeout=timeout, input=input)

    def g(self, cmd, timeout=30):
        r = self.ssh(cmd + " 2>&1", timeout=timeout)
        return r["out"].rstrip() + ("" if r["rc"] == 0 else f"\n[rc={r['rc']}] {r['err'].strip()}")

    def wait_ssh(self, timeout=300):
        t0 = time.monotonic()
        while time.monotonic() - t0 < timeout:
            if self.ssh("true", timeout=8)["rc"] == 0:
                return round(time.monotonic() - t0, 2)
            time.sleep(1)
        raise TimeoutError(f"guest did not answer over SSH within {timeout}s")

    def wait_ssh_gone(self, timeout=90):
        t0 = time.monotonic()
        while time.monotonic() - t0 < timeout:
            if self.ssh("true", timeout=4)["rc"] != 0:
                return round(time.monotonic() - t0, 2)
            time.sleep(0.5)
        raise TimeoutError("guest kept answering over SSH")

    def status(self):
        return roomy("vm", "status", self.folder)["out"]

    def wait_status(self, wanted, timeout=120):
        t0 = time.monotonic()
        while time.monotonic() - t0 < timeout:
            if self.status() == wanted:
                return round(time.monotonic() - t0, 2)
            time.sleep(0.25)
        raise TimeoutError(f"status stayed '{self.status()}', expected '{wanted}'")

    def boot_id(self):
        """Changes on every boot of the guest and never while it runs."""
        return self.g("cat /proc/sys/kernel/random/boot_id").strip() or None

    def uptime(self):
        try:
            return float(self.g("cat /proc/uptime").split()[0])
        except (ValueError, IndexError):
            return None


def processes(*programs):
    """Running processes whose program is one of `programs` (default: everything a VM consists of)."""
    programs = programs or ("roomy", "qemu-system-x86_64", "virtiofsd")
    found = []
    for line in sh(["pgrep", "-af", "|".join(programs)])["out"].splitlines():
        parts = line.split()
        if len(parts) > 1 and os.path.basename(parts[1]) in programs and "doctor" not in parts[2:3]:
            found.append(line[:300])
    return found


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def require_base_image():
    if not BASE_IMAGE.is_file():
        raise RuntimeError(f"{BASE_IMAGE} is missing: run just lab-probe once first (it downloads the guest image)")


def new_key(folder):
    """A new SSH key pair in `folder` for reaching lab guests: (private key path, public key)."""
    key = str(Path(folder) / "id_ed25519")
    r = sh(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "roomy-lab", "-f", key])
    if r["rc"] != 0:
        raise RuntimeError(f"ssh-keygen failed: {r}")
    return key, Path(key + ".pub").read_text().strip()


def make_seed(iso, hostname, key_pub):
    """Write a cloud-init seed image at `iso` that names the guest and lets `key_pub` in over SSH."""
    folder = Path(iso).with_suffix("")
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "user-data").write_text(f"#cloud-config\nssh_pwauth: false\nssh_authorized_keys:\n  - {key_pub}\n")
    (folder / "meta-data").write_text(f"instance-id: {hostname}-{TS}\nlocal-hostname: {hostname}\n")
    r = sh(["xorriso", "-as", "mkisofs", "-output", str(iso), "-volid", "CIDATA", "-joliet", "-rock",
            str(folder / "user-data"), str(folder / "meta-data")])
    if r["rc"] != 0:
        raise RuntimeError(f"xorriso failed: {r}")


def create_vm(name, key_pub, extra=()):
    """Create a VM from the cloud image with a cloud-init seed that installs our SSH key."""
    seed = WORK / f"seed-{name}.iso"
    make_seed(seed, name, key_pub)
    port = free_port()
    r = roomy("vm", "create", "--name", name, "--in", WORK / "vms", "--os", "archlinux", "--cpus", 4, "--memory", 4096,
              "--disk", 20, "--from-disk", BASE_IMAGE, "--iso", seed, "--forward", f"{port}:22", *extra)
    if r["rc"] != 0:
        raise RuntimeError(f"create failed: {r}")
    folder = WORK / "vms" / name
    VMS.append(folder)
    return folder, port, r


def collect_logs(folder, name):
    src = Path(folder) / "logs"
    if src.is_dir():
        shutil.copytree(src, OUT / "vm-logs" / name, dirs_exist_ok=True)
    cfg = Path(folder) / "roomy.toml"
    if cfg.is_file():
        shutil.copy(cfg, OUT / f"{name}-roomy.toml")


def main_vm_checks(key, key_pub):
    share_dir = WORK / "shared"
    share_dir.mkdir()
    (share_dir / "from-host.txt").write_text("written on the host\n")

    with step("main:create") as d:
        folder, port, created = create_vm("engine-main", key_pub, ["--share", f"labshare={share_dir}"])
        d["create"] = created
        d["folder_listing"] = sh(["ls", "-la", str(folder)])["out"]
        d["config"] = (folder / "roomy.toml").read_text()
    if not VMS:
        return
    vm = Guest(folder, port, key)

    with step("main:start-cold") as d:
        d["status_before"] = vm.status()
        d["start"] = roomy("vm", "start", vm.folder)
        expect(d, d["start"]["rc"] == 0 and d["start"]["out"] == "running", "start did not report 'running'")
        d["status"] = vm.status()
        d["processes"] = processes()
        d["boot_to_ssh_s"] = vm.wait_ssh()
        d["cloud_init"] = vm.g("sudo cloud-init status --wait", timeout=240)
        d["guest"] = vm.g("uname -r; nproc; free -m | sed -n 2p; systemctl is-active qemu-guest-agent; "
                          "sudo cat /sys/class/dmi/id/product_uuid")
        d["start_again"] = roomy("vm", "start", vm.folder)
        expect(d, d["start_again"]["rc"] == 1, "a second start should be refused")
    if vm.status() != "running":
        log("the VM is not running; skipping the lifecycle checks")
        collect_logs(folder, "engine-main")
        return

    with step("main:shared-folder") as d:
        # The guest is told about the folder through the firmware tables and mounts it by itself.
        d["systemd"] = vm.g("systemctl --version | head -n 1")
        d["mounted_by_itself"] = vm.g("findmnt -no SOURCE,FSTYPE,OPTIONS /mnt/labshare || echo not-mounted")
        d["told_to_mount"] = vm.g("sudo cat /run/credentials/@system/fstab.extra 2>&1 | head -n 5")
        expect(d, "virtiofs" in d["mounted_by_itself"],
               "the guest did not mount the shared folder by itself; see systemd's version and what it was told")
        if "virtiofs" not in d["mounted_by_itself"]:
            d["mount"] = vm.g("sudo mkdir -p /mnt/labshare && sudo mount -t virtiofs labshare /mnt/labshare")
        d["listing"] = vm.g("ls -lan /mnt/labshare")
        d["guest_write"] = vm.g("echo from-guest > /mnt/labshare/from-guest.txt && cat /mnt/labshare/from-host.txt")
        d["host_view"] = sh(["ls", "-lan", str(share_dir)])["out"]
        expect(d, (share_dir / "from-guest.txt").is_file() and "written on the host" in d["guest_write"],
               "the shared folder did not work in both directions")

    with step("main:pause-resume") as d:
        d["lag_before_s"] = clock_lag(vm)
        d["pause"] = roomy("vm", "pause", vm.folder)
        d["status_paused"] = vm.status()
        d["ssh_while_paused_rc"] = vm.ssh("true", timeout=4)["rc"]
        time.sleep(6)
        d["resume"] = roomy("vm", "resume", vm.folder)
        d["status_resumed"] = vm.status()
        d["lag_after_resume_s"] = clock_lag(vm)
        expect(d, d["status_paused"] == "paused" and d["status_resumed"] == "running", "pause/resume states are wrong")
        expect(d, d["lag_after_resume_s"] is not None and abs(d["lag_after_resume_s"]) < 1.5,
               "the guest clock was not corrected after resume")

    for cycle in (1, 2):
        with step(f"main:suspend-{cycle}") as d:
            vm.g("sudo systemctl is-active roomy-ticker || sudo systemd-run --unit=roomy-ticker --collect "
                 "bash -c 'while :; do date +%s.%N >> /var/tmp/ticks; sleep 0.5; done'")
            time.sleep(2)
            d["uptime_before"] = vm.uptime()
            d["boot_id_before"] = vm.boot_id()
            d["suspend"] = roomy("vm", "suspend", vm.folder, timeout=600)
            expect(d, d["suspend"]["rc"] == 0 and d["suspend"]["out"] == "suspended", "suspend did not report 'suspended'")
            d["became_suspended_after_s"] = vm.wait_status("suspended", timeout=60)
            time.sleep(1)
            d["processes_after"] = processes()
            expect(d, not d["processes_after"], "processes were left running after suspend")
            d["state_dir"] = sh(["ls", "-la", str(folder / "state")])["out"]
            d["state_on_disk"] = sh(["du", "-sh", str(folder / "state")])["out"].strip()
            d["pause_refused"] = roomy("vm", "pause", vm.folder)
        if vm.status() != "suspended":
            break
        time.sleep(6)
        with step(f"main:continue-{cycle}") as d:
            d["start"] = roomy("vm", "start", vm.folder)
            expect(d, d["start"]["out"] == "running (continued from saved state)", "start did not continue from the saved state")
            d["ssh_after_s"] = vm.wait_ssh(timeout=120)
            d["start_to_ssh_s"] = round(d["start"]["secs"] + d["ssh_after_s"], 2)
            d["uptime_after"] = vm.uptime()
            d["lag_after_continue_s"] = clock_lag(vm)
            d["ticker"] = vm.g("systemctl is-active roomy-ticker; wc -l < /var/tmp/ticks")
            d["share_after"] = vm.g("cat /mnt/labshare/from-host.txt && echo cycle-%d >> /mnt/labshare/from-guest.txt && echo share-ok" % cycle)
            d["state_dir_after"] = sh(["ls", "-la", str(folder / "state")])["out"]
            d["supervisor_log"] = sh(["tail", "-n", "6", str(folder / "logs" / "supervisor.log")])["out"]
            previous = REPORT["steps"][-2]["data"].get("uptime_before")
            expect(d, d["uptime_after"] and previous and d["uptime_after"] > previous, "the guest rebooted instead of continuing")
            d["boot_id_after"] = vm.boot_id()
            expect(d, d["boot_id_after"] and d["boot_id_after"] == REPORT["steps"][-2]["data"].get("boot_id_before"),
                   "the guest rebooted instead of continuing")
            expect(d, "share-ok" in d["share_after"], "the shared folder stopped working after continuing")
            expect(d, d["lag_after_continue_s"] is not None and abs(d["lag_after_continue_s"]) < 1.5,
                   "the guest clock was not corrected after continuing")
            expect(d, "suspend.state" not in d["state_dir_after"], "the saved state was not removed after continuing")

    with step("main:restart") as d:
        d["boot_id_before"] = vm.boot_id()
        d["restart"] = roomy("vm", "restart", vm.folder)
        d["ssh_gone_after_s"] = vm.wait_ssh_gone()
        d["ssh_back_after_s"] = vm.wait_ssh()
        d["boot_id_after"] = vm.boot_id()
        d["uptime_after"] = vm.uptime()
        d["status"] = vm.status()
        expect(d, d["boot_id_before"] and d["boot_id_after"] and d["boot_id_after"] != d["boot_id_before"],
               "the guest did not restart")
        expect(d, d["status"] == "running", "the VM is not reported as running after the restart")

    with step("main:shutdown") as d:
        d["shutdown"] = roomy("vm", "shutdown", vm.folder)
        d["off_after_s"] = vm.wait_status("off", timeout=180)
        time.sleep(1)
        d["processes_after"] = processes()
        expect(d, not d["processes_after"], "processes were left running after shutdown")

    with step("main:force-off") as d:
        d["start"] = roomy("vm", "start", vm.folder)
        vm.wait_ssh()
        d["force_off"] = roomy("vm", "force-off", vm.folder)
        d["off_after_s"] = vm.wait_status("off", timeout=30)
        expect(d, d["force_off"]["rc"] == 0, "force-off failed")

    with step("main:killed-supervisor") as d:
        d["start"] = roomy("vm", "start", vm.folder)
        vm.wait_ssh()
        supervisors = [line.split()[0] for line in processes("roomy") if vm.folder in line]
        d["supervisor_pids"] = supervisors
        for pid in supervisors:
            sh(["kill", "-9", pid])
        time.sleep(4)
        d["processes_after_kill"] = processes("qemu-system-x86_64", "virtiofsd")
        d["status_after_kill"] = vm.status()
        expect(d, not d["processes_after_kill"], "QEMU kept running without a supervisor")
        expect(d, d["status_after_kill"] == "off", "status is not 'off' after the supervisor was killed")
        d["start_after_kill"] = roomy("vm", "start", vm.folder)
        expect(d, d["start_after_kill"]["rc"] == 0, "the VM would not start again")
        d["ssh_s"] = vm.wait_ssh()
        d["force_off"] = roomy("vm", "force-off", vm.folder)
        vm.wait_status("off", timeout=30)

    collect_logs(folder, "engine-main")


def gpu_vm_checks(key, key_pub):
    with step("gpu:create-and-start") as d:
        folder, port, created = create_vm("engine-gpu", key_pub, ["--gpu"])
        vm = Guest(folder, port, key)
        d["start"] = roomy("vm", "start", vm.folder)
        expect(d, d["start"]["rc"] == 0, "the GPU-accelerated VM did not start")
        d["boot_to_ssh_s"] = vm.wait_ssh()
        d["guest_drm"] = vm.g("sudo dmesg | grep -iE 'virgl|virtio_gpu' | tail -n 4")
    with step("gpu:suspend-refused") as d:
        d["suspend"] = roomy("vm", "suspend", vm.folder)
        d["status"] = vm.status()
        d["still_answers"] = vm.ssh("true")["rc"] == 0
        expect(d, d["suspend"]["rc"] == 1 and "GPU acceleration" in d["suspend"]["err"], "suspend was not refused with an explanation")
        expect(d, d["status"] == "running" and d["still_answers"], "the guest did not keep running")
    with step("gpu:pause-resume-force-off") as d:
        d["pause"] = roomy("vm", "pause", vm.folder)
        d["resume"] = roomy("vm", "resume", vm.folder)
        d["force_off"] = roomy("vm", "force-off", vm.folder)
        vm.wait_status("off", timeout=30)
    collect_logs(folder, "engine-gpu")


def main():
    start_run("engine")
    try:
        with step("build") as d:
            d["cargo"] = sh("cargo build --release 2>&1 | tail -n 3", timeout=900)
            d["version"] = roomy("--version")
            expect(d, d["version"]["rc"] == 0, "the roomy program could not be built")
        with step("doctor") as d:
            r = roomy("doctor", "--report-file", OUT / "doctor.txt")
            d["rc"] = r["rc"]
            expect(d, r["rc"] == 0, "roomy doctor reports problems; see doctor.txt")
        if not all(s["ok"] for s in REPORT["steps"]) and os.environ.get("ROOMY_LAB_IGNORE_DOCTOR") != "1":
            return
        with step("prepare") as d:
            require_base_image()
            key, key_pub = new_key(WORK)
            d["work_filesystem"] = sh(["df", "-hT", str(WORK)])["out"]
        if REPORT["steps"][-1]["ok"]:
            main_vm_checks(key, key_pub)
            gpu_vm_checks(key, key_pub)
    except KeyboardInterrupt:
        log("interrupted")
        REPORT["interrupted"] = True
    finally:
        finish_run(f"roomy engine check {TS}")


if __name__ == "__main__":
    sys.exit(main())
