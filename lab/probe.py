#!/usr/bin/env python3
"""roomy lab: find out what this host's QEMU stack really does.

Runs a series of small experiments against the installed QEMU and writes
everything it learns to work/lab/results/<timestamp>/ (report.json, SUMMARY.txt,
per-process logs, screenshots). Nothing outside the lab/ directory is modified.

Usage:  just lab-probe                 run everything (about 10 minutes plus one image download)
        just lab-probe features        only the named VM stages (features, suspend, gpu);
                                 the environment check and the no-guest probes always run
        just lab-probe followup        second-round questions (guest agent after restore, clock, resize modes)

Environment knobs:
  ROOMY_LAB_IMAGE    path to an Arch cloud image qcow2 (skips the download)
  ROOMY_LAB_MEM_MB   guest memory, default 4096
  ROOMY_LAB_FILL_MB  random data held in guest memory during suspend, default 1536
  ROOMY_LAB_KEEP=1   keep work/lab/scratch/<timestamp> (disk overlay, state files) afterwards
  ROOMY_LAB_TM       multiply every timeout (default 1, or 6 without KVM)
"""
import atexit
import contextlib
import fnmatch
import glob
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import traceback
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
# Everything a run downloads or writes lives under the checkout's work/ folder, which git ignores.
DATA = HERE.parent / "work" / "lab"
TS = time.strftime("%Y%m%d-%H%M%S")
OUT = DATA / "results" / TS
WORK = DATA / "scratch" / TS
CACHE = DATA / "cache"
LOGS = OUT / "logs"

IMAGE_URL = "https://geo.mirror.pkgbuild.com/images/latest/Arch-Linux-x86_64-cloudimg.qcow2"
QEMU = "qemu-system-x86_64"
GUEST_USER = "arch"
GUEST_UID = 1000
MEM_MB = int(os.environ.get("ROOMY_LAB_MEM_MB", "4096"))
FILL_MB = int(os.environ.get("ROOMY_LAB_FILL_MB", "1536"))
KEEP = os.environ.get("ROOMY_LAB_KEEP") == "1"
VM_STAGES = ("features", "suspend", "gpu")
EXTRA_STAGES = ("followup",)

REPORT = {"started": TS, "argv": sys.argv, "steps": []}
F = {}            # facts and feature flags discovered along the way
PROCS = []        # every child process we started
TM = 1.0          # timeout multiplier, set in main()
RUN = None        # short runtime dir for sockets, set in main()
_run_log = None


# ----------------------------------------------------------------- helpers

def log(msg):
    line = f"[{time.strftime('%H:%M:%S')}] {msg}"
    print(line, flush=True)
    if _run_log:
        _run_log.write(line + "\n")
        _run_log.flush()


def save_report():
    tmp = OUT / "report.json.tmp"
    with open(tmp, "w") as f:
        json.dump(REPORT, f, indent=1, default=str)
    os.replace(tmp, OUT / "report.json")


def clip(text, limit=20000):
    if text is None:
        return ""
    if isinstance(text, bytes):
        text = text.decode("utf-8", "replace")
    if len(text) <= limit:
        return text
    return text[: limit // 2] + f"\n...[{len(text) - limit} chars cut]...\n" + text[-limit // 2:]


def sh(cmd, timeout=30, input=None, pass_fds=(), env=None):
    """Run a command and return {rc, out, err}; never raises."""
    t0 = time.monotonic()
    try:
        p = subprocess.run(cmd, shell=isinstance(cmd, str), capture_output=True, text=True, errors="replace",
                           timeout=timeout, input=input, pass_fds=pass_fds, env=env)
        r = {"rc": p.returncode, "out": clip(p.stdout), "err": clip(p.stderr)}
    except subprocess.TimeoutExpired as e:
        r = {"rc": None, "timeout_s": timeout, "out": clip(e.stdout), "err": clip(e.stderr)}
    except OSError as e:
        r = {"rc": None, "exec_error": repr(e), "out": "", "err": ""}
    r["secs"] = round(time.monotonic() - t0, 2)
    return r


def tail(path, lines=40):
    try:
        with open(path, "rb") as f:
            data = f.read()[-200000:]
        return "\n".join(data.decode("utf-8", "replace").splitlines()[-lines:])
    except OSError as e:
        return f"<cannot read {path}: {e}>"


def spawn(name, argv, pass_fds=()):
    path = LOGS / f"{name}.log"
    logf = open(path, "wb")
    logf.write(("$ " + " ".join(map(str, argv)) + "\n").encode())
    logf.flush()
    p = subprocess.Popen(list(map(str, argv)), stdin=subprocess.DEVNULL, stdout=logf, stderr=subprocess.STDOUT,
                         pass_fds=pass_fds)
    p.log_path = path
    p.name = name
    PROCS.append(p)
    return p


def stop_proc(p, grace=3.0):
    if p is None or p.poll() is not None:
        return
    p.terminate()
    try:
        p.wait(grace)
    except subprocess.TimeoutExpired:
        p.kill()
        p.wait(5)


def cleanup():
    for p in PROCS:
        with contextlib.suppress(Exception):
            stop_proc(p, 1.0)
    if RUN:
        shutil.rmtree(RUN, ignore_errors=True)
    if not KEEP:
        shutil.rmtree(WORK, ignore_errors=True)
        with contextlib.suppress(OSError):
            (DATA / "scratch").rmdir()


def wait_for(predicate, timeout, interval=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(interval)
    return predicate()


@contextlib.contextmanager
def step(name):
    """Record one experiment. An exception marks it failed and the run continues."""
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
        save_report()
        verdict = {True: "ok", False: "FAILED"}.get(rec["ok"], "interrupted")
        log(f"<<< {name}: {verdict} ({rec['secs']}s)" + (f"  {rec.get('error', '')}" if rec["ok"] is False else ""))


def skip(name, why):
    REPORT["steps"].append({"name": name, "ok": None, "skipped": why})
    log(f"--- {name}: skipped ({why})")
    save_report()


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# --------------------------------------------------------------------- QMP

class QMPError(Exception):
    pass


class QMP:
    """Tiny synchronous QMP client (one JSON object per line)."""

    def __init__(self, path, proc=None, timeout=20):
        deadline = time.monotonic() + timeout
        while True:
            if proc is not None and proc.poll() is not None:
                raise RuntimeError(f"QEMU exited with code {proc.returncode} before QMP was ready")
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                s.connect(str(path))
                break
            except OSError:
                s.close()
                if time.monotonic() > deadline:
                    raise TimeoutError(f"QMP socket {path} did not appear within {timeout}s")
                time.sleep(0.05)
        self.sock = s
        self.buf = b""
        self.events = []
        self._id = 0
        self.greeting = self._recv(time.monotonic() + 10)
        self.cmd("qmp_capabilities")

    def _recv(self, deadline):
        while b"\n" not in self.buf:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for QMP")
            self.sock.settimeout(remaining)
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                raise TimeoutError("timed out waiting for QMP") from None
            if not chunk:
                raise ConnectionError("QMP connection closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def _note_event(self, obj):
        obj["host_time"] = time.time()
        self.events.append(obj)

    def cmd(self, name, args=None, fds=None, timeout=30):
        self._id += 1
        msg = {"execute": name, "id": self._id}
        if args is not None:
            msg["arguments"] = args
        data = (json.dumps(msg) + "\n").encode()
        if fds:
            socket.send_fds(self.sock, [data], list(fds))
        else:
            self.sock.sendall(data)
        deadline = time.monotonic() + timeout
        while True:
            obj = self._recv(deadline)
            if "event" in obj:
                self._note_event(obj)
            elif obj.get("id") == self._id:
                if "error" in obj:
                    raise QMPError(f"{name}: {obj['error'].get('class')}: {obj['error'].get('desc')}")
                return obj.get("return")

    def try_cmd(self, name, args=None, **kw):
        """Like cmd() but returns {"error": ...} instead of raising on a QMP error."""
        try:
            return self.cmd(name, args, **kw)
        except QMPError as e:
            return {"error": str(e)}

    def wait_event(self, name, timeout, since=0):
        deadline = time.monotonic() + timeout
        seen = since
        while True:
            for ev in self.events[seen:]:
                if ev["event"] == name:
                    return ev
            seen = len(self.events)
            obj = self._recv(deadline)
            if "event" in obj:
                self._note_event(obj)

    def close(self):
        with contextlib.suppress(OSError):
            self.sock.close()


def qga(path, execute, args=None, timeout=8):
    """One QEMU guest agent command over the agent's unix socket."""
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    try:
        s.connect(str(path))
        sync_id = int(time.time() * 1000) % 2000000000
        s.sendall((json.dumps({"execute": "guest-sync", "arguments": {"id": sync_id}}) + "\n").encode())
        buf = b""
        deadline = time.monotonic() + timeout
        synced = False
        while time.monotonic() < deadline:
            while b"\n" not in buf:
                chunk = s.recv(65536)
                if not chunk:
                    raise ConnectionError("guest agent socket closed")
                buf += chunk
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            obj = json.loads(line)
            if not synced:
                if obj.get("return") == sync_id:
                    synced = True
                    msg = {"execute": execute}
                    if args is not None:
                        msg["arguments"] = args
                    s.sendall((json.dumps(msg) + "\n").encode())
                continue
            return obj
        raise TimeoutError("guest agent did not answer")
    finally:
        s.close()


# ------------------------------------------------------- host discovery

def kvm_usable():
    try:
        os.close(os.open("/dev/kvm", os.O_RDWR))
        return True, ""
    except OSError as e:
        return False, str(e)


def find_virtiofsd():
    candidates = [shutil.which("virtiofsd"), "/usr/lib/virtiofsd", "/usr/libexec/virtiofsd", "/usr/lib/qemu/virtiofsd"]
    for desc in sorted(glob.glob("/usr/share/qemu/vhost-user/*.json")):
        with contextlib.suppress(Exception):
            j = json.load(open(desc))
            if j.get("type") == "fs":
                candidates.insert(0, j.get("binary"))
    for c in candidates:
        if c and os.access(c, os.X_OK):
            return c
    return None


def find_firmware():
    """Pick a UEFI firmware (no secure boot) using QEMU firmware descriptors."""
    found = []
    for desc in sorted(glob.glob("/usr/share/qemu/firmware/*.json") + glob.glob("/etc/qemu/firmware/*.json")):
        try:
            j = json.load(open(desc))
        except Exception as e:
            found.append({"descriptor": desc, "error": repr(e)})
            continue
        m = j.get("mapping", {})
        entry = {
            "descriptor": desc,
            "interface": j.get("interface-types"),
            "device": m.get("device"),
            "mode": m.get("mode"),
            "code": m.get("executable", {}).get("filename"),
            "code_format": m.get("executable", {}).get("format"),
            "vars": m.get("nvram-template", {}).get("filename"),
            "vars_format": m.get("nvram-template", {}).get("format"),
            "features": j.get("features", []),
            "x86_64_q35": any(t.get("architecture") == "x86_64" and
                              any(fnmatch.fnmatch("pc-q35-11.1", g) for g in t.get("machines", []))
                              for t in j.get("targets", [])),
        }
        found.append(entry)
    F["firmware_descriptors"] = found
    for e in found:
        if (e.get("x86_64_q35") and "uefi" in (e.get("interface") or []) and e.get("device") == "flash"
                and e.get("code") and e.get("vars") and "secure-boot" not in e["features"]
                and os.path.exists(e["code"]) and os.path.exists(e["vars"])):
            return e
    for code, nvram in (("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
                        ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
                        ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd")):
        if os.path.exists(code) and os.path.exists(nvram):
            return {"descriptor": None, "code": code, "code_format": "raw", "vars": nvram, "vars_format": "raw"}
    return None


def accel_args():
    if F.get("kvm"):
        return ["-accel", "kvm", "-cpu", "host"]
    return ["-accel", "tcg,thread=multi", "-cpu", "max"]


def start_virtiofsd(name, sock, shared_dir, extra):
    argv = [F["virtiofsd"], "--socket-path", sock, "--shared-dir", shared_dir,
            "--sandbox", F.get("virtiofsd_sandbox", "namespace"), *extra]
    p = spawn(name, argv)
    if not wait_for(lambda: os.path.exists(sock) or p.poll() is not None, 8) or p.poll() is not None:
        stop_proc(p)
        raise RuntimeError(f"virtiofsd did not come up: {tail(p.log_path, 15)}")
    return p


def uid_squash_args():
    """Every guest user acts as the host user; host user's files show up as the guest's main user."""
    uid, gid = os.getuid(), os.getgid()
    return ["--translate-uid", f"squash-guest:0:{uid}:4294967295", "--translate-gid", f"squash-guest:0:{gid}:4294967295",
            "--translate-uid", f"host:{uid}:{GUEST_UID}:1", "--translate-gid", f"host:{gid}:{GUEST_UID}:1"]


def uid_map_args():
    uid, gid = os.getuid(), os.getgid()
    return ["--translate-uid", f"map:{GUEST_UID}:{uid}:1", "--translate-gid", f"map:{GUEST_UID}:{gid}:1"]


MIGRATION_FLAG_SETS = [
    ["--migration-mode=find-paths", "--migration-on-error=guest-error", "--migration-verify-handles",
     "--migration-confirm-paths"],
    ["--migration-mode=find-paths", "--migration-on-error=guest-error", "--migration-confirm-paths"],
    ["--migration-mode=find-paths", "--migration-on-error=guest-error"],
    [],
]


# ------------------------------------------------------ D-Bus display client

def summarize_client(out_path, proc_result):
    s = {"client_rc": proc_result.get("rc"), "file": str(Path(out_path).relative_to(OUT))}
    if proc_result.get("rc") != 0:
        s["client_stderr"] = proc_result.get("err", "")[-1500:]
        if "timeout_s" in proc_result:
            s["client_timeout"] = True
    try:
        o = json.load(open(out_path))
    except Exception as e:
        s["no_output"] = repr(e)
        return s
    for k in ("mode", "connected", "listener_registered", "counts", "vm", "console_before", "console_after", "resize"):
        if k in o:
            s[k] = o[k]
    s["errors"] = [f"{e.get('where')}: {e.get('error')}" for e in o.get("errors", [])]
    s["first_calls"] = o.get("calls", [])[:8]
    s["objects"] = {p: sorted(ifaces) for p, ifaces in o.get("objects", {}).items()}
    return s


def run_display_client(tag, qmp=None, bus=None, seconds=3.0, extras=False, resize=None):
    """Attach lab/dbus_client.py to a QEMU D-Bus display, peer-to-peer (via qmp) or over a bus."""
    out = OUT / "dbus" / f"{tag}.json"
    argv = [sys.executable, str(HERE / "dbus_client.py"), "--out", str(out), "--seconds", str(seconds)]
    if extras:
        argv.append("--extras")
    if resize:
        argv += ["--resize", resize]
    if bus:
        r = sh(argv + ["--bus", bus], timeout=seconds + 40)
    else:
        ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            qmp.cmd("getfd", {"fdname": "roomy-dbus"}, fds=[theirs.fileno()])
            qmp.cmd("add_client", {"protocol": "@dbus-display", "fdname": "roomy-dbus"})
            theirs.close()
            r = sh(argv + ["--p2p-fd", str(ours.fileno())], timeout=seconds + 40, pass_fds=[ours.fileno()])
        finally:
            ours.close()
            theirs.close()
    return summarize_client(out, r)


# --------------------------------------------------------- no-guest probes

def probe(name, extra=(), display="none", machine_extra="", hook=None, helpers=()):
    """Start a paused, diskless QEMU with some feature enabled and see whether it comes up."""
    with step(f"probe:{name}") as d:
        sock = f"{RUN}/p-{len(REPORT['steps'])}.sock"
        argv = [QEMU, "-nodefaults", "-no-user-config", "-S", "-name", f"roomy-probe-{name}",
                "-machine", F.get("machine", "q35") + machine_extra, *accel_args(), "-m", "512",
                "-display", display, "-qmp", f"unix:{sock},server=on,wait=off", *extra]
        d["argv"] = " ".join(argv)
        p = spawn(f"probe-{name}", argv)
        q = None
        try:
            try:
                q = QMP(sock, p, timeout=25 * TM)
            except Exception as e:
                d["started"] = False
                d["ok"] = False
                d["why"] = repr(e)
                d["stderr"] = tail(p.log_path, 30)
                return False
            d["started"] = True
            if hook:
                try:
                    hook(q, d)
                except Exception as e:
                    d["hook_error"] = repr(e)
                    d["hook_trace"] = traceback.format_exc()[-1500:]
            d["alive_after_hook"] = p.poll() is None
            with contextlib.suppress(Exception):
                q.cmd("quit", timeout=5)
            return True
        finally:
            if q:
                q.close()
            with contextlib.suppress(subprocess.TimeoutExpired):
                p.wait(5)
            stop_proc(p)
            for h in helpers:
                stop_proc(h)
            d["exit_code"] = p.returncode
            d["stderr"] = tail(p.log_path, 30)
    return False


def hook_base(q, d):
    d["version"] = q.cmd("query-version")
    d["kvm"] = q.try_cmd("query-kvm")
    machines = q.cmd("query-machines")
    for m in machines:
        if m.get("alias") == "q35":
            F["machine"] = m["name"]
    d["machine_resolved"] = F.get("machine")
    d["q35_machines"] = sorted(m["name"] for m in machines if m["name"].startswith("pc-q35"))
    devices = {t["name"] for t in q.cmd("qom-list-types", {"implements": "device", "abstract": False})}
    wanted = ["virtio-vga", "virtio-vga-gl", "virtio-gpu-pci", "virtio-gpu-gl-pci", "vhost-user-fs-pci",
              "virtio-blk-pci", "virtio-net-pci", "virtio-balloon-pci", "virtio-mem-pci", "virtio-sound-pci",
              "virtio-serial-pci", "virtio-rng-pci", "qemu-xhci", "usb-tablet", "usb-host", "ich9-intel-hda",
              "hda-duplex", "tpm-crb", "tpm-tis", "pcie-root-port", "nvme", "e1000e", "ramfb", "bochs-display"]
    d["devices"] = {w: (w in devices) for w in wanted}
    d["migrate_capabilities"] = sorted(c["capability"] for c in q.cmd("query-migrate-capabilities"))
    d["migrate_parameters"] = q.try_cmd("query-migrate-parameters")
    d["chardev_backends"] = sorted(b["name"] for b in q.cmd("query-chardev-backends"))
    F["cap_mapped_ram"] = "mapped-ram" in d["migrate_capabilities"]
    params = d["migrate_parameters"] if isinstance(d["migrate_parameters"], dict) else {}
    F["param_direct_io"] = "direct-io" in params


def hook_migratable(q, d):
    d["query_migrate"] = q.try_cmd("query-migrate")
    target = f"{WORK}/probe-state-{len(REPORT['steps'])}"
    d["migrate_attempt"] = q.try_cmd("migrate", {"uri": f"file:{target}"})
    time.sleep(0.5)
    d["query_migrate_after"] = q.try_cmd("query-migrate")
    with contextlib.suppress(OSError):
        os.unlink(target)


def hook_p2p_client(tag, extras=False, migratable=False, twice=False):
    def hook(q, d):
        d["client"] = run_display_client(tag, qmp=q, extras=extras)
        if twice:
            d["client_reconnect"] = run_display_client(tag + "-again", qmp=q)
        if migratable:
            hook_migratable(q, d)
    return hook


def stage_probes():
    ok = probe("base", hook=hook_base)
    if not ok:
        return False
    log(f"machine type: {F.get('machine')}  mapped-ram: {F.get('cap_mapped_ram')}  direct-io: {F.get('param_direct_io')}")

    F["dbus_p2p"] = probe("display-dbus-p2p", ["-device", "virtio-vga"], display="dbus,p2p=yes",
                          hook=hook_p2p_client("probe-p2p", twice=True))
    probe("display-dbus-p2p-extras", ["-device", "virtio-vga"], display="dbus,p2p=yes",
          hook=hook_p2p_client("probe-p2p-extras", extras=True))

    # Private session bus, for the VM stages that talk to QEMU over a bus.
    with step("dbus:private-bus") as d:
        bus_sock = f"{RUN}/bus.sock"
        addr = f"unix:path={bus_sock}"
        p = spawn("dbus-daemon", ["dbus-daemon", "--session", "--nofork", "--nopidfile", f"--address={addr}"])
        if not wait_for(lambda: os.path.exists(bus_sock) or p.poll() is not None, 5) or p.poll() is not None:
            d["ok"] = False
            d["why"] = tail(p.log_path, 10)
        else:
            F["bus"] = addr
    if F.get("bus"):
        def hook_bus(q, d):
            d["busctl_tree"] = sh(["busctl", f"--address={F['bus']}", "tree", "org.qemu"])
            d["client"] = run_display_client("probe-bus", bus=F["bus"])
        F["dbus_bus"] = probe("display-dbus-bus", ["-device", "virtio-vga"], display=f"dbus,addr={F['bus']}",
                              hook=hook_bus)

    # GPU acceleration: which display configurations come up, and what do they hand a client?
    gl_variants = [("default", "")] + [(os.path.basename(n), f",rendernode={n}")
                                       for n in sorted(glob.glob("/dev/dri/renderD*"))]
    F["gl_working"] = []
    for label, suffix in gl_variants:
        tag = f"probe-gl-{label}"
        with contextlib.suppress(Exception):
            started = probe(f"gl-dbus-{label}", ["-device", "virtio-vga-gl"], display=f"dbus,p2p=yes,gl=on{suffix}",
                            hook=hook_p2p_client(tag, migratable=True))
            if started:
                client = REPORT["steps"][-1]["data"].get("client", {})
                got_frame = any(k.startswith("Scanout") for k in client.get("counts", {}))
                F["gl_working"].append({"label": label, "suffix": suffix, "client_got_scanout": got_frame})
    probe("gl-dbus-extras", ["-device", "virtio-vga-gl"], display="dbus,p2p=yes,gl=on",
          hook=hook_p2p_client("probe-gl-extras", extras=True))
    probe("gl-egl-headless", ["-device", "virtio-vga-gl"], display="egl-headless")
    probe("gl-venus", ["-object", "memory-backend-memfd,id=mem,size=512M,share=on",
                       "-device", "virtio-vga-gl,hostmem=1G,blob=true,venus=true"],
          display="dbus,p2p=yes,gl=on", machine_extra=",memory-backend=mem")

    hda = ["-device", "ich9-intel-hda", "-device", "hda-duplex,audiodev=snd0"]
    F["audio_pipewire"] = probe("audio-pipewire", ["-audiodev", "pipewire,id=snd0", *hda])
    F["audio_dbus"] = probe("audio-dbus", ["-audiodev", "dbus,id=snd0", *hda], display="dbus,p2p=yes,audiodev=snd0")
    F["vdagent"] = probe("clipboard-vdagent",
                         ["-chardev", "qemu-vdagent,id=vdagent,clipboard=on", "-device", "virtio-serial-pci",
                          "-device", "virtserialport,chardev=vdagent,name=com.redhat.spice.0"],
                         display="dbus,p2p=yes", hook=hook_migratable)
    probe("net-user", ["-netdev", "user,id=n0", "-device", "virtio-net-pci,netdev=n0"])
    probe("net-passt", ["-netdev", "passt,id=n0", "-device", "virtio-net-pci,netdev=n0"])
    F["memfd"] = probe("memory-memfd", ["-object", "memory-backend-memfd,id=mem,size=512M,share=on"],
                       machine_extra=",memory-backend=mem")

    fw = find_firmware()
    F["firmware"] = fw
    if fw:
        nvram = f"{WORK}/probe-VARS.fd"
        shutil.copyfile(fw["vars"], nvram)
        F["uefi"] = probe("uefi-firmware",
                          ["-drive", f"if=pflash,format={fw['code_format']},unit=0,readonly=on,file={fw['code']}",
                           "-drive", f"if=pflash,format={fw['vars_format']},unit=1,file={nvram}"])
    else:
        skip("probe:uefi-firmware", "no UEFI firmware found")

    if shutil.which("swtpm"):
        tpm_dir = f"{WORK}/probe-tpm"
        os.makedirs(tpm_dir, exist_ok=True)
        tpm_sock = f"{RUN}/swtpm.sock"
        swtpm = spawn("swtpm-probe", ["swtpm", "socket", "--tpm2", "--tpmstate", f"dir={tpm_dir}",
                                      "--ctrl", f"type=unixio,path={tpm_sock}", "--terminate"])
        wait_for(lambda: os.path.exists(tpm_sock) or swtpm.poll() is not None, 5)
        probe("tpm-swtpm", ["-chardev", f"socket,id=chrtpm,path={tpm_sock}", "-tpmdev", "emulator,id=tpm0,chardev=chrtpm",
                            "-device", "tpm-crb,tpmdev=tpm0"], helpers=[swtpm])
    else:
        skip("probe:tpm-swtpm", "swtpm not installed")

    # Shared folders: which sandbox mode and which option sets does this virtiofsd accept?
    if not F.get("virtiofsd"):
        skip("probe:virtiofs", "virtiofsd not found")
        return True
    share = f"{WORK}/probe-share"
    os.makedirs(share, exist_ok=True)

    def virtiofs_probe(label, sandbox, extra):
        sock = f"{RUN}/pfs-{label}.sock"
        with contextlib.suppress(OSError):
            os.unlink(sock)
        F["virtiofsd_sandbox"] = sandbox
        try:
            fsd = start_virtiofsd(f"virtiofsd-probe-{label}", sock, share, extra)
        except Exception as e:
            with step(f"probe:virtiofs-{label}") as d:
                d["ok"] = False
                d["why"] = str(e)
            return False
        return probe(f"virtiofs-{label}",
                     ["-object", "memory-backend-memfd,id=mem,size=512M,share=on",
                      "-chardev", f"socket,id=fs0,path={sock}", "-device", "vhost-user-fs-pci,chardev=fs0,tag=probe"],
                     machine_extra=",memory-backend=mem", hook=hook_migratable, helpers=[fsd])

    sandbox_ok = None
    for sandbox in ("namespace", "none"):
        if virtiofs_probe(f"sandbox-{sandbox}", sandbox, []):
            sandbox_ok = sandbox
            break
    if not sandbox_ok:
        F["virtiofs"] = False
        return True
    F["virtiofs"] = True
    F["virtiofsd_sandbox"] = sandbox_ok
    F["virtiofs_squash"] = virtiofs_probe("uid-squash", sandbox_ok, uid_squash_args())
    F["virtiofs_map"] = virtiofs_probe("uid-map", sandbox_ok, uid_map_args())
    base = uid_squash_args() if F["virtiofs_squash"] else []
    F["virtiofs_migration_sets"] = []
    for i, flags in enumerate(MIGRATION_FLAG_SETS):
        if virtiofs_probe(f"migration-set{i}", sandbox_ok, base + flags):
            F["virtiofs_migration_sets"].append(flags)
    return True


# ---------------------------------------------------------------- the guest

USER_DATA = """#cloud-config
ssh_pwauth: false
ssh_authorized_keys:
  - {pubkey}
"""

UID_TEST = r"""#!/bin/bash
# Tries ordinary file operations on each mount point as three different guest users.
for m in "$@"; do
  echo "===== $m"
  if ! mountpoint -q "$m"; then echo "NOT MOUNTED"; continue; fi
  echo "--- listing before"; ls -lan "$m"
  for u in root arch tester; do
    echo "--- as $u (uid $(id -u "$u"))"
    t() {
      local name=$1; shift
      if sudo -u "$u" "$@" >/dev/null 2>/tmp/uidtest.err; then echo "$name: ok"
      else echo "$name: FAIL ($(head -c 160 /tmp/uidtest.err | tr '\n' ' '))"; fi
    }
    t read-0644     cat "$m/host-file.txt"
    t read-0600     cat "$m/host-private.txt"
    t create        touch "$m/new-by-$u.txt"
    t write         sh -c "echo hello >> '$m/new-by-$u.txt'"
    t append-host   sh -c "echo from-$u >> '$m/host-file.txt'"
    t mkdir         mkdir "$m/dir-by-$u"
    t create-in-dir touch "$m/dir-by-$u/inner.txt"
    t chmod         chmod 640 "$m/new-by-$u.txt"
    t rename        mv "$m/new-by-$u.txt" "$m/renamed-by-$u.txt"
    t symlink       ln -s host-file.txt "$m/link-by-$u"
    t run-script    sh -c "printf '#!/bin/sh\necho ran\n' > '$m/script-$u.sh' && chmod +x '$m/script-$u.sh' && '$m/script-$u.sh'"
    t delete        rm "$m/script-$u.sh"
  done
  echo "--- root chown to arch"
  if sudo chown arch:arch "$m/renamed-by-root.txt" 2>/tmp/uidtest.err; then echo "chown: ok"
  else echo "chown: FAIL ($(head -c 160 /tmp/uidtest.err))"; fi
  echo "--- listing after"; ls -lan "$m"
done
"""

HOLDER = r"""#!/usr/bin/env python3
# Runs inside the guest across a suspend/restore. Holds random data in memory and
# open files on the shared folder, and logs once per half second what it can see.
import hashlib, json, os, sys, time
share, fill_mb = sys.argv[1], int(sys.argv[2])
log = open('/var/tmp/holder.log', 'a', buffering=1)
def L(**kw):
    kw['t'] = round(time.time(), 3); kw['mono'] = round(time.monotonic(), 3)
    log.write(json.dumps(kw) + '\n')
buf = bytearray()
for _ in range(fill_mb):
    buf += os.urandom(1 << 20)
sha = hashlib.sha256(buf).hexdigest()
L(ev='filled', mb=fill_mb)
files = {}
for name in ('keep.txt', 'modify.txt', 'delete.txt', 'replace.txt'):
    try:
        files[name] = open(os.path.join(share, name), 'rb')
    except Exception as e:
        L(ev='open-fail', name=name, err=repr(e))
try:
    app = open(os.path.join(share, 'append.txt'), 'ab', buffering=0)
except Exception as e:
    app = None; L(ev='open-fail', name='append.txt', err=repr(e))
L(ev='ready')
n = 0
while True:
    n += 1
    rec = {'ev': 'tick', 'n': n}
    if n % 4 == 0:
        for name, f in files.items():
            try:
                f.seek(0); rec[name] = f.read(120).decode(errors='replace')
            except Exception as e:
                rec[name] = 'ERR ' + repr(e)
        try:
            app.write(b'tick %d\n' % n); rec['append'] = 'ok'
        except Exception as e:
            rec['append'] = 'ERR ' + repr(e)
        try:
            rec['listdir'] = sorted(os.listdir(share))
        except Exception as e:
            rec['listdir'] = 'ERR ' + repr(e)
        for name in ('modify.txt', 'replace.txt', 'delete.txt', 'added-while-suspended.txt'):
            try:
                with open(os.path.join(share, name), 'rb') as f:
                    rec['fresh:' + name] = f.read(120).decode(errors='replace')
            except Exception as e:
                rec['fresh:' + name] = 'ERR ' + repr(e)
    if os.path.exists('/var/tmp/verify'):
        os.unlink('/var/tmp/verify')
        rec['memory_intact'] = hashlib.sha256(buf).hexdigest() == sha
    L(**rec)
    time.sleep(0.5)
"""


def stage_image():
    """Fetch the guest image, make a throwaway disk on top of it, and build the cloud-init seed."""
    with step("guest:image") as d:
        CACHE.mkdir(exist_ok=True)
        override = os.environ.get("ROOMY_LAB_IMAGE")
        base = Path(override) if override else CACHE / "Arch-Linux-x86_64-cloudimg.qcow2"
        if not base.exists():
            log(f"downloading {IMAGE_URL} (about 500 MB, once)")
            part = base.with_suffix(".part")
            with urllib.request.urlopen(IMAGE_URL, timeout=60) as r, open(part, "wb") as f:
                total = 0
                next_mark = 100 << 20
                while True:
                    chunk = r.read(1 << 20)
                    if not chunk:
                        break
                    f.write(chunk)
                    total += len(chunk)
                    if total >= next_mark:
                        log(f"  {total >> 20} MB")
                        next_mark += 100 << 20
            try:
                with urllib.request.urlopen(IMAGE_URL + ".SHA256", timeout=30) as r:
                    want = r.read().decode().split()[0]
                h = hashlib.sha256()
                with open(part, "rb") as f:
                    for chunk in iter(lambda: f.read(1 << 20), b""):
                        h.update(chunk)
                d["sha256_ok"] = h.hexdigest() == want
                if not d["sha256_ok"]:
                    raise RuntimeError("downloaded image does not match its published SHA256")
            except urllib.error.URLError as e:
                d["sha256_ok"] = f"could not fetch checksum: {e}"
            os.replace(part, base)
        d["base"] = str(base)
        d["info"] = sh(["qemu-img", "info", "--output=json", str(base)])
        disk = WORK / "disk.qcow2"
        r = sh(["qemu-img", "create", "-f", "qcow2", "-F", "qcow2", "-b", str(base), str(disk), "20G"])
        if r["rc"] != 0:
            raise RuntimeError(f"qemu-img create failed: {r}")
        F["disk"] = str(disk)

        key = f"{RUN}/id_ed25519"
        r = sh(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "roomy-lab", "-f", key])
        if r["rc"] != 0:
            raise RuntimeError(f"ssh-keygen failed: {r}")
        F["ssh_key"] = key
        seed_dir = WORK / "seed"
        seed_dir.mkdir(exist_ok=True)
        (seed_dir / "user-data").write_text(USER_DATA.format(pubkey=open(key + ".pub").read().strip()))
        (seed_dir / "meta-data").write_text(f"instance-id: roomy-lab-{TS}\nlocal-hostname: roomy-lab\n")
        seed = WORK / "seed.iso"
        r = sh(["xorriso", "-as", "mkisofs", "-output", str(seed), "-volid", "CIDATA", "-joliet", "-rock",
                str(seed_dir / "user-data"), str(seed_dir / "meta-data")])
        if r["rc"] != 0:
            raise RuntimeError(f"xorriso failed: {r}")
        F["seed"] = str(seed)
        F["guest_ready"] = True


def make_share(tag, suspend_files=False):
    d = WORK / "shares" / tag
    (d / "sub").mkdir(parents=True, exist_ok=True)
    (d / "host-file.txt").write_text("written on the host\n")
    (d / "host-private.txt").write_text("host private, mode 0600\n")
    os.chmod(d / "host-private.txt", 0o600)
    (d / "sub" / "inner.txt").write_text("inner\n")
    if suspend_files:
        for name in ("keep.txt", "modify.txt", "delete.txt", "replace.txt"):
            (d / name).write_text(f"original content of {name}\n")
    return str(d)


class VM:
    """One guest. launch() may be called repeatedly (cold boot or restore from a state file)."""

    def __init__(self, label, gpu="virtio-vga", display="none", shares=(), vdagent=False, audio=None, smp="4"):
        self.label = label
        self.gpu = gpu
        self.display = display
        self.shares = list(shares)        # dicts: tag, dir, extra
        self.vdagent = vdagent
        self.audio = audio
        self.smp = smp
        self.uefi = bool(F.get("uefi")) and F.get("uefi_boot", True)
        self.port = free_port()
        self.launches = 0
        self.proc = None
        self.qmp = None
        self.helpers = []
        self.nvram = f"{WORK}/{label}-VARS.fd"
        self.qga_sock = None
        self.serial = None

    def argv(self, tag, incoming):
        a = [QEMU, "-nodefaults", "-no-user-config", "-name", f"roomy-lab-{self.label}",
             "-machine", f"{F['machine']},memory-backend=mem", *accel_args(), "-smp", self.smp,
             "-m", str(MEM_MB), "-object", f"memory-backend-memfd,id=mem,size={MEM_MB}M,share=on",
             "-qmp", f"unix:{RUN}/{tag}-qmp.sock,server=on,wait=off", "-rtc", "base=utc",
             "-serial", f"file:{self.serial}", "-display", self.display]
        if self.uefi:
            fw = F["firmware"]
            if not os.path.exists(self.nvram):
                shutil.copyfile(fw["vars"], self.nvram)
            a += ["-drive", f"if=pflash,format={fw['code_format']},unit=0,readonly=on,file={fw['code']}",
                  "-drive", f"if=pflash,format={fw['vars_format']},unit=1,file={self.nvram}"]
        for i in range(1, 5):
            a += ["-device", f"pcie-root-port,id=rp{i},chassis={i},port={0x10 + i}"]
        a += ["-device", self.gpu,
              "-device", "qemu-xhci,id=usb", "-device", "usb-tablet",
              "-blockdev", f"driver=qcow2,node-name=disk0,discard=unmap,file.driver=file,file.filename={F['disk']}",
              "-device", "virtio-blk-pci,drive=disk0,id=vda,bootindex=1",
              "-drive", f"if=none,id=seed,format=raw,media=cdrom,readonly=on,file={F['seed']}",
              "-device", "ide-cd,drive=seed,bus=ide.0",
              "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{self.port}-:22",
              "-device", "virtio-net-pci,netdev=n0,id=nic0",
              "-device", "virtio-rng-pci", "-device", "virtio-balloon-pci,id=balloon0",
              "-device", "virtio-serial-pci,id=vser0",
              "-chardev", f"socket,id=qga0,path={self.qga_sock},server=on,wait=off",
              "-device", "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0"]
        if self.vdagent:
            a += ["-chardev", "qemu-vdagent,id=vdagent,clipboard=on",
                  "-device", "virtserialport,chardev=vdagent,name=com.redhat.spice.0"]
        if self.audio:
            a += ["-audiodev", f"{self.audio},id=snd0", "-device", "ich9-intel-hda", "-device", "hda-duplex,audiodev=snd0"]
        for s in self.shares:
            a += ["-chardev", f"socket,id=fs-{s['tag']},path={s['sock']}",
                  "-device", f"vhost-user-fs-pci,chardev=fs-{s['tag']},tag={s['tag']},id=fsdev-{s['tag']}"]
        if incoming:
            a += ["-incoming", "defer"]
        return a

    def launch(self, incoming=False):
        self.launches += 1
        tag = f"{self.label}-{self.launches}"
        self.serial = LOGS / f"serial-{tag}.log"
        self.qga_sock = f"{RUN}/{tag}-qga.sock"
        self.helpers = []
        for s in self.shares:
            s["sock"] = f"{RUN}/{tag}-fs-{s['tag']}.sock"
            self.helpers.append(start_virtiofsd(f"virtiofsd-{tag}-{s['tag']}", s["sock"], s["dir"], s.get("extra", [])))
        self.proc = spawn(f"qemu-{tag}", self.argv(tag, incoming))
        self.qmp = QMP(f"{RUN}/{tag}-qmp.sock", self.proc, timeout=30 * TM)

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def ssh(self, cmd, timeout=30, input=None):
        argv = ["ssh", "-F", "/dev/null", "-i", F["ssh_key"], "-p", str(self.port),
                "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes",
                "-o", "IdentitiesOnly=yes", "-o", "ConnectTimeout=5", "-o", "LogLevel=ERROR",
                f"{GUEST_USER}@127.0.0.1", cmd]
        return sh(argv, timeout=timeout * TM, input=input)

    def g(self, cmd, timeout=30):
        """Run a guest command and return its combined output as text."""
        r = self.ssh(cmd + " 2>&1", timeout=timeout)
        text = r["out"].rstrip()
        if r["rc"] != 0:
            text += f"\n[rc={r['rc']}{' TIMEOUT' if 'timeout_s' in r else ''}] {r['err'].strip()}"
        return text

    def wait_ssh(self, timeout):
        t0 = time.monotonic()
        while time.monotonic() - t0 < timeout:
            if not self.alive():
                raise RuntimeError(f"QEMU exited with code {self.proc.returncode} while waiting for the guest")
            if self.ssh("true", timeout=8)["rc"] == 0:
                return round(time.monotonic() - t0, 2)
            time.sleep(1)
        raise TimeoutError(f"guest did not answer over SSH within {timeout:.0f}s")

    def reap_helpers(self, d=None):
        """After QEMU is gone, virtiofsd should exit on its own; note whether it did."""
        for h in self.helpers:
            exited = True
            try:
                h.wait(4)
            except subprocess.TimeoutExpired:
                exited = False
                stop_proc(h)
            if d is not None:
                d.setdefault("helpers_exited_on_their_own", {})[h.name] = exited
        self.helpers = []

    def quit(self, d=None):
        if self.alive():
            with contextlib.suppress(Exception):
                self.qmp.cmd("quit", timeout=10)
            with contextlib.suppress(subprocess.TimeoutExpired):
                self.proc.wait(10)
        stop_proc(self.proc)
        if self.qmp:
            self.qmp.close()
        if d is not None and self.proc is not None:
            d["qemu_exit_code"] = self.proc.returncode
        self.reap_helpers(d)

    def powerdown(self, d, timeout=90):
        """Graceful shutdown through the ACPI power button."""
        t0 = time.monotonic()
        self.qmp.cmd("system_powerdown")
        try:
            self.proc.wait(timeout * TM)
            d["graceful_shutdown_s"] = round(time.monotonic() - t0, 2)
        except subprocess.TimeoutExpired:
            d["ok"] = False
            d["why"] = f"guest did not power off within {timeout * TM:.0f}s"
        self.quit(d)

    def failure_context(self):
        ctx = {"qemu_log": tail(self.proc.log_path, 30) if self.proc else "", "serial_tail": tail(self.serial, 50)}
        if self.alive():
            with contextlib.suppress(Exception):
                ctx["status"] = self.qmp.try_cmd("query-status", timeout=5)
        return ctx


def boot_guest(vm, d):
    """Cold boot and wait for SSH. Falls back from UEFI to BIOS once if the guest never shows up."""
    for attempt in (1, 2):
        vm.launch()
        d["uefi"] = vm.uefi
        try:
            d["boot_to_ssh_s"] = vm.wait_ssh(300 * TM)
            break
        except Exception as e:
            fail = {"uefi": vm.uefi, "error": repr(e), **vm.failure_context()}
            d.setdefault("boot_failures", []).append(fail)
            vm.quit()
            if vm.uefi and attempt == 1:
                log("guest did not come up with UEFI firmware; retrying with BIOS")
                vm.uefi = False
                F["uefi_boot"] = False
                continue
            raise
    d["cloud_init"] = vm.g("sudo cloud-init status --wait", timeout=240)
    d["guest"] = vm.g("uname -r; systemd-detect-virt; nproc; free -m | sed -n 2p; cat /proc/cmdline; "
                      "systemctl is-system-running; lsmod | grep -E 'virtio|drm|fuse' | awk '{print $1}' | tr '\\n' ' '")


def screendump(vm, name):
    path = OUT / "screens" / f"{name}.png"
    r = vm.qmp.try_cmd("screendump", {"filename": str(path), "format": "png"}, timeout=20)
    return {"file": str(path.relative_to(OUT)), "result": r, "bytes": path.stat().st_size if path.exists() else 0}


def display_kwargs(vm):
    """How to reach this VM's D-Bus display: over the private bus, or peer-to-peer through QMP."""
    if "addr=" in vm.display:
        return {"bus": F["bus"]}
    return {"qmp": vm.qmp}


def mount_shares(vm):
    out = {}
    for s in vm.shares:
        out[s["tag"]] = vm.g(f"sudo mkdir -p /mnt/{s['tag']} && sudo mount -t virtiofs {s['tag']} /mnt/{s['tag']} "
                             f"&& findmnt -no FSTYPE,OPTIONS /mnt/{s['tag']}")
    return out


# ------------------------------------------------------------ stage: features

def stage_features():
    shares = []
    if F.get("virtiofs"):
        shares.append({"tag": "plain", "dir": make_share("plain"), "extra": []})
        if F.get("virtiofs_squash"):
            shares.append({"tag": "squash", "dir": make_share("squash"), "extra": uid_squash_args()})
        if F.get("virtiofs_map"):
            shares.append({"tag": "map", "dir": make_share("map"), "extra": uid_map_args()})
    if F.get("dbus_bus"):
        display = f"dbus,addr={F['bus']}"
    elif F.get("dbus_p2p"):
        display = "dbus,p2p=yes"
    else:
        display = "none"
    vm = VM("feat", display=display, shares=shares, smp="2,maxcpus=4")
    try:
        with step("features:boot") as d:
            boot_guest(vm, d)
        if not vm.alive():
            skip("features:*", "guest did not boot")
            return
        q = vm.qmp

        if shares:
            with step("features:shared-folder-permissions") as d:
                d["host_ids"] = {"uid": os.getuid(), "gid": os.getgid()}
                d["mounts"] = mount_shares(vm)
                vm.g("sudo useradd -u 4242 -m tester")
                vm.ssh("cat > /var/tmp/uidtest.sh", input=UID_TEST)
                mounts = " ".join(f"/mnt/{s['tag']}" for s in shares)
                text = vm.g(f"bash /var/tmp/uidtest.sh {mounts}", timeout=120)
                (OUT / "uid-test-guest.txt").write_text(text)
                host_view = "".join(f"===== {s['tag']}\n{sh(['ls', '-lanR', s['dir']])['out']}\n" for s in shares)
                (OUT / "uid-test-host.txt").write_text(host_view)
                d["files"] = ["uid-test-guest.txt", "uid-test-host.txt"]
                d["fail_lines"] = [ln for ln in text.splitlines() if "FAIL" in ln or "NOT MOUNTED" in ln][:80]

        if display != "none":
            with step("features:display") as d:
                modes = "for f in /sys/class/drm/card*-Virtual-1/modes; do head -n 3 $f; done; cat /sys/class/graphics/fb0/virtual_size"
                d["guest_modes_before"] = vm.g(modes)
                d["client"] = run_display_client("feat-core", seconds=5, resize="1600x900", **display_kwargs(vm))
                time.sleep(1)
                d["guest_modes_after_resize_request"] = vm.g(modes)
                d["client_extras"] = run_display_client("feat-extras", seconds=3, extras=True, **display_kwargs(vm))
                d["client_reconnect"] = run_display_client("feat-reconnect", seconds=2, **display_kwargs(vm))
                d["screenshot"] = screendump(vm, "features")
                if "addr=" in display:
                    d["busctl_tree"] = sh(["busctl", f"--address={F['bus']}", "tree", "org.qemu"])["out"]

        with step("features:guest-agent") as d:
            d["ping"] = qga(vm.qga_sock, "guest-ping")
            info = qga(vm.qga_sock, "guest-info").get("return", {})
            d["version"] = info.get("version")
            enabled = {c["name"] for c in info.get("supported_commands", []) if c.get("enabled")}
            d["commands"] = {c: (c in enabled) for c in ("guest-shutdown", "guest-set-time", "guest-get-time",
                                                         "guest-fsfreeze-freeze", "guest-exec", "guest-get-osinfo",
                                                         "guest-network-get-interfaces", "guest-suspend-disk")}
            d["osinfo"] = qga(vm.qga_sock, "guest-get-osinfo").get("return")

        with step("features:memory-balloon") as d:
            d["before"] = {"qmp": q.try_cmd("query-balloon"), "guest": vm.g("free -m | sed -n 2p")}
            q.cmd("balloon", {"value": (MEM_MB // 2) << 20})
            time.sleep(5)
            d["after_shrink_to_half"] = {"qmp": q.try_cmd("query-balloon"), "guest": vm.g("free -m | sed -n 2p")}
            q.cmd("balloon", {"value": MEM_MB << 20})
            time.sleep(3)
            d["after_restore"] = {"qmp": q.try_cmd("query-balloon"), "guest": vm.g("free -m | sed -n 2p")}

        with step("features:disk-grow-live") as d:
            d["before"] = vm.g("lsblk -bdno SIZE /dev/vda")
            q.cmd("block_resize", {"node-name": "disk0", "size": 24 << 30})
            time.sleep(2)
            d["after"] = vm.g("lsblk -bdno SIZE /dev/vda; sudo dmesg | grep -i 'vda' | tail -n 3")
            d["ok"] = d["after"].splitlines()[0].strip() == str(24 << 30)

        with step("features:cpu-hotplug") as d:
            cpus = "nproc; cat /sys/devices/system/cpu/online /sys/devices/system/cpu/present"
            d["before"] = vm.g(cpus)
            slots = q.cmd("query-hotpluggable-cpus")
            free = [s for s in slots if "qom-path" not in s]
            d["free_slots"] = len(free)
            if free:
                d["device_add"] = q.try_cmd("device_add", {"driver": free[0]["type"], "id": "cpu-hot1", **free[0]["props"]})
                time.sleep(3)
                d["after"] = vm.g(cpus)

        if F.get("virtiofs"):
            with step("features:shared-folder-hotplug") as d:
                sock = f"{RUN}/hot-fs.sock"
                fsd = start_virtiofsd("virtiofsd-hotplug", sock, make_share("hot"), uid_squash_args() if F.get("virtiofs_squash") else [])
                vm.helpers.append(fsd)
                d["chardev_add"] = q.try_cmd("chardev-add", {"id": "fs-hot", "backend": {"type": "socket", "data": {
                    "addr": {"type": "unix", "data": {"path": sock}}, "server": False}}})
                d["device_add"] = q.try_cmd("device_add", {"driver": "vhost-user-fs-pci", "id": "fsdev-hot",
                                                           "chardev": "fs-hot", "tag": "hot", "bus": "rp1"})
                time.sleep(3)
                d["guest_mount"] = vm.g("sudo mkdir -p /mnt/hot && sudo mount -t virtiofs hot /mnt/hot && ls -la /mnt/hot "
                                        "&& echo from-guest | sudo tee /mnt/hot/hotplug-write.txt")
                d["guest_umount"] = vm.g("sudo umount /mnt/hot && echo unmounted")
                seen = len(q.events)
                d["device_del"] = q.try_cmd("device_del", {"id": "fsdev-hot"})
                try:
                    q.wait_event("DEVICE_DELETED", 20 * TM, since=seen)
                    d["device_deleted_event"] = True
                except TimeoutError:
                    d["device_deleted_event"] = False
                d["chardev_remove"] = q.try_cmd("chardev-remove", {"id": "fs-hot"})
                d["ok"] = "from-guest" in d["guest_mount"] and d["device_deleted_event"]

        with step("features:network-toggle") as d:
            iface = "$(ls /sys/class/net | grep -v lo | head -n 1)"
            carrier = f"cat /sys/class/net/{iface}/carrier /sys/class/net/{iface}/carrier_changes"
            d["before"] = vm.g(carrier)
            q.cmd("set_link", {"name": "nic0", "up": False})
            time.sleep(2)
            d["ssh_while_down_rc"] = vm.ssh("true", timeout=6)["rc"]
            q.cmd("set_link", {"name": "nic0", "up": True})
            d["back_after_s"] = vm.wait_ssh(60 * TM)
            d["after"] = vm.g(carrier)
            d["ok"] = d["ssh_while_down_rc"] != 0

        with step("features:pause-resume") as d:
            d["before"] = vm.g("date +%s.%N; cat /proc/uptime")
            q.cmd("stop")
            d["status_paused"] = q.cmd("query-status")
            d["ssh_while_paused_rc"] = vm.ssh("true", timeout=4)["rc"]
            time.sleep(4)
            q.cmd("cont")
            d["status_resumed"] = q.cmd("query-status")
            d["after"] = vm.g("date +%s.%N; cat /proc/uptime")
            d["host_time"] = time.time()

        with step("features:restart-via-ctrl-alt-del") as d:
            seen = len(q.events)
            t0 = time.monotonic()
            q.cmd("send-key", {"keys": [{"type": "qcode", "data": k} for k in ("ctrl", "alt", "delete")]})
            q.wait_event("RESET", 90 * TM, since=seen)
            d["reset_event_after_s"] = round(time.monotonic() - t0, 2)
            time.sleep(3)
            d["ssh_back_after_s"] = vm.wait_ssh(180 * TM)
            d["uptime"] = vm.g("cat /proc/uptime; sudo journalctl -b -1 -n 5 --no-pager -o cat")

        with step("features:graceful-shutdown") as d:
            vm.powerdown(d)
            d["events"] = [e["event"] for e in q.events][-12:]
    finally:
        vm.quit()


# ------------------------------------------------------------- stage: suspend

def migration_setup(q, variant):
    caps = {"mapped-ram": "mapped-ram" in variant, "multifd": "multifd" in variant}
    q.cmd("migrate-set-capabilities", {"capabilities": [{"capability": k, "state": v} for k, v in caps.items()]})
    params = {"max-bandwidth": 1 << 40}
    if caps["multifd"]:
        params["multifd-channels"] = 4
    if "direct-io" in variant:
        params["direct-io"] = True
    q.cmd("migrate-set-parameters", params)


def wait_migration(q, timeout):
    deadline = time.monotonic() + timeout
    last = {}
    while time.monotonic() < deadline:
        last = q.cmd("query-migrate")
        if last.get("status") in ("completed", "failed", "cancelled"):
            return last
        time.sleep(0.1)
    raise TimeoutError(f"migration still '{last.get('status')}' after {timeout:.0f}s")


def suspend_to_file(vm, state, variant, d):
    """Stop the guest, write its state to a file, and end the QEMU process."""
    q = vm.qmp
    d["variant"] = variant
    with contextlib.suppress(OSError):
        os.unlink(state)
    t0 = time.monotonic()
    q.cmd("stop")
    try:
        migration_setup(q, variant)
        d["blocked_reasons"] = q.cmd("query-migrate").get("blocked-reasons")
        q.cmd("migrate", {"uri": f"file:{state}"})
        result = wait_migration(q, 600 * TM)
    except Exception:
        with contextlib.suppress(Exception):
            q.cmd("cont")
        raise
    d["migrate_result"] = {k: result.get(k) for k in ("status", "total-time", "downtime", "setup-time", "error-desc")}
    d["ram"] = {k: result.get("ram", {}).get(k) for k in ("transferred", "total", "mbps", "pages-per-second")}
    if result.get("status") != "completed":
        q.cmd("cont")
        raise RuntimeError(f"saving state failed: {result.get('error-desc', result.get('status'))}")
    st = os.stat(state)
    d["state_file"] = {"apparent_mb": st.st_size >> 20, "on_disk_mb": (st.st_blocks * 512) >> 20}
    vm.quit(d)
    d["suspend_s"] = round(time.monotonic() - t0, 2)


def restore_from_file(vm, state, variant, d):
    """Start a fresh QEMU (and fresh virtiofsd), load the state file, and continue the guest."""
    t0 = time.monotonic()
    vm.launch(incoming=True)
    q = vm.qmp
    migration_setup(q, variant)
    q.cmd("migrate-incoming", {"uri": f"file:{state}"})
    result = wait_migration(q, 600 * TM)
    d["incoming_result"] = {k: result.get(k) for k in ("status", "error-desc")}
    if result.get("status") != "completed":
        raise RuntimeError(f"loading state failed: {result}")
    d["status_after_load"] = q.cmd("query-status")
    q.cmd("cont")
    d["restore_to_running_s"] = round(time.monotonic() - t0, 2)
    vm.wait_ssh(120 * TM)
    d["restore_to_ssh_s"] = round(time.monotonic() - t0, 2)


def holder_summary(text):
    recs = []
    for line in text.splitlines():
        with contextlib.suppress(ValueError):
            recs.append(json.loads(line))
    ticks = [r for r in recs if r.get("ev") == "tick"]
    s = {"records": len(recs), "ticks": len(ticks), "non_tick": [r for r in recs if r.get("ev") != "tick"][:10]}
    if len(ticks) > 1:
        wall = max((b["t"] - a["t"], a["n"]) for a, b in zip(ticks, ticks[1:]))
        mono = max((b["mono"] - a["mono"], a["n"]) for a, b in zip(ticks, ticks[1:]))
        s["largest_wall_clock_gap"] = {"secs": round(wall[0], 2), "after_tick": wall[1]}
        s["largest_monotonic_gap"] = {"secs": round(mono[0], 2), "after_tick": mono[1]}
        s["tick_numbers_contiguous"] = all(b["n"] == a["n"] + 1 for a, b in zip(ticks, ticks[1:]))
    s["memory_intact_checks"] = [r["memory_intact"] for r in ticks if "memory_intact" in r]
    detailed = [r for r in ticks if "listdir" in r]
    if detailed:
        s["last_file_view"] = detailed[-1]
    return s


def check_after_restore(vm, d, name):
    vm.g("sudo touch /var/tmp/verify")
    time.sleep(6)
    text = vm.ssh("cat /var/tmp/holder.log", timeout=30)["out"]
    (OUT / f"holder-{name}.log").write_text(text)
    d["holder"] = holder_summary(text)
    d["holder_unit"] = vm.g("systemctl is-active roomy-holder")
    host_now = time.time()
    guest_now = vm.g("date +%s.%N")
    with contextlib.suppress(ValueError):
        d["guest_clock_behind_host_s"] = round(host_now - float(guest_now.split()[0]), 2)
    d["guest"] = vm.g("uptime; systemctl is-system-running; ls -la /mnt/share0; "
                      "sudo dmesg | tail -n 25; sudo journalctl -p err -b --no-pager -n 15 -o cat")
    checks = d["holder"]["memory_intact_checks"]
    d["ok"] = bool(checks) and checks[-1] is True and d["holder"].get("tick_numbers_contiguous") is True


def suspend_scenario(index, fs_flags, mutate):
    """Boot, suspend to disk, optionally change the shared folder while suspended, restore, verify.

    Returns True when the guest came back intact."""
    name = f"suspend{index}"
    shares = []
    if F.get("virtiofs"):
        extra = (uid_squash_args() if F.get("virtiofs_squash") else []) + fs_flags
        shares.append({"tag": "share0", "dir": make_share(f"{name}-share0", suspend_files=True), "extra": extra})
    display = "dbus,p2p=yes" if F.get("dbus_p2p") else "none"
    audio = "dbus" if (F.get("audio_dbus") and display != "none") else None
    if audio:
        display += ",audiodev=snd0"
    vm = VM(name, display=display, shares=shares, vdagent=bool(F.get("vdagent")), audio=audio)
    variant1 = "mapped-ram+multifd" if F.get("cap_mapped_ram") else "plain"
    succeeded = False
    try:
        with step(f"{name}:boot") as d:
            d["virtiofsd_flags"] = fs_flags
            d["share_changed_while_suspended"] = mutate
            d["devices"] = {"display": display, "audio": audio, "clipboard_agent": vm.vdagent, "shares": len(shares)}
            boot_guest(vm, d)
            d["mounts"] = mount_shares(vm)
            vm.ssh("cat > /var/tmp/holder.py", input=HOLDER)
            fill = FILL_MB if F.get("kvm") else min(FILL_MB, 256)
            d["holder_start"] = vm.g(f"sudo rm -f /var/tmp/holder.log; sudo systemd-run --unit=roomy-holder --collect "
                                     f"python3 /var/tmp/holder.py /mnt/share0 {fill}")
            ready = wait_for(lambda: vm.ssh("grep -c '\"ev\": \"ready\"' /var/tmp/holder.log",
                                            timeout=10)["out"].strip() not in ("", "0"), 180 * TM, interval=2)
            d["holder_ready"] = ready
            time.sleep(5)
        if not vm.alive():
            return False
        state = WORK / f"{name}.state"

        with step(f"{name}:suspend-1") as d:
            try:
                suspend_to_file(vm, state, variant1, d)
            except Exception as e:
                if variant1 == "plain" or not vm.alive():
                    raise
                d["first_attempt_error"] = repr(e)
                log(f"saving with {variant1} failed, retrying with a plain state file")
                variant1 = "plain"
                suspend_to_file(vm, state, variant1, d)
        if vm.alive():      # suspend failed and the guest was continued
            with step(f"{name}:shutdown-after-failed-suspend") as d:
                vm.powerdown(d)
            return False

        with step(f"{name}:while-suspended") as d:
            if shares and mutate:
                sd = Path(shares[0]["dir"])
                (sd / "modify.txt").write_text("MODIFIED on the host while the guest was suspended\n")
                os.unlink(sd / "delete.txt")
                (sd / "replace.tmp").write_text("REPLACED by a different file while the guest was suspended\n")
                os.replace(sd / "replace.tmp", sd / "replace.txt")
                (sd / "added-while-suspended.txt").write_text("new file created while the guest was suspended\n")
                d["host_listing"] = sh(["ls", "-la", str(sd)])["out"]
            time.sleep(8)

        with step(f"{name}:restore-1") as d:
            restore_from_file(vm, state, variant1, d)
            check_after_restore(vm, d, f"{name}-1")
            d["virtiofsd_log"] = "\n".join(tail(h.log_path, 25) for h in vm.helpers)
            succeeded = d.get("ok", True)
        if not vm.alive():
            return False

        with step(f"{name}:clock-fix-via-guest-agent") as d:
            d["before"] = {"guest": vm.g("date +%s.%N"), "host": time.time()}
            d["guest_set_time"] = qga(vm.qga_sock, "guest-set-time")
            d["after"] = {"guest": vm.g("date +%s.%N"), "host": time.time()}

        variant2 = variant1 + "+direct-io" if (F.get("param_direct_io") and "multifd" in variant1) else variant1
        with step(f"{name}:suspend-2") as d:
            suspend_to_file(vm, state, variant2, d)
        if vm.alive():
            with step(f"{name}:shutdown-after-failed-suspend-2") as d:
                vm.powerdown(d)
            return succeeded
        with step(f"{name}:restore-2") as d:
            restore_from_file(vm, state, variant2, d)
            check_after_restore(vm, d, f"{name}-2")

        if vm.alive():
            with step(f"{name}:graceful-shutdown") as d:
                vm.powerdown(d)
        return succeeded
    finally:
        vm.quit()
        with contextlib.suppress(OSError):
            os.unlink(WORK / f"{name}.state")


def stage_suspend():
    """Best shared-folder options first, with the share changed while suspended. If that fails, repeat
    without changing the share (is it the change that breaks it?), then try the next option set."""
    flag_sets = F.get("virtiofs_migration_sets") or [[]]
    attempts = [(flag_sets[0], True), (flag_sets[0], False)] + [(f, True) for f in flag_sets[1:2]]
    F["suspend_attempts"] = []
    for i, (flags, mutate) in enumerate(attempts, start=1):
        ok = suspend_scenario(i, flags, mutate)
        F["suspend_attempts"].append({"virtiofsd_flags": flags, "share_changed": mutate, "ok": ok})
        if ok and mutate:
            return
        log("suspend scenario did not fully succeed; trying the next variation")


# ------------------------------------------------------------ stage: followup

def timed(fn):
    t0 = time.monotonic()
    try:
        result = fn()
    except Exception as e:
        result = {"error": repr(e)}
    return {"secs": round(time.monotonic() - t0, 2), "result": result}


def clock_lag(vm):
    """Seconds the guest wall clock is behind the host (includes about 0.1s of SSH latency)."""
    host = time.time()
    try:
        return round(host - float(vm.g("date +%s.%N").split()[0]), 2)
    except (ValueError, IndexError):
        return None


def agent_checks(vm, d):
    """Exercise the guest agent and the guest's hardware clock, timing every call."""
    d["clock_lag_before_s"] = clock_lag(vm)
    d["agent_ping"] = timed(lambda: qga(vm.qga_sock, "guest-ping", timeout=20))
    d["hwclock_show"] = timed(lambda: vm.g("sudo hwclock --show --verbose | tail -n 6", timeout=40))
    d["agent_set_time_explicit"] = timed(
        lambda: qga(vm.qga_sock, "guest-set-time", {"time": int(time.time() * 1e9)}, timeout=40))
    d["clock_lag_after_explicit_s"] = clock_lag(vm)
    d["agent_set_time_from_rtc"] = timed(lambda: qga(vm.qga_sock, "guest-set-time", timeout=40))
    d["clock_lag_after_rtc_s"] = clock_lag(vm)
    d["agent_ping_again"] = timed(lambda: qga(vm.qga_sock, "guest-ping", timeout=20))
    d["agent_unit"] = vm.g("systemctl is-active qemu-guest-agent; "
                           "sudo journalctl -u qemu-guest-agent -b --no-pager -n 12 -o cat")


def stage_followup():
    """Second-round questions: why the guest agent stalled after a restore, what a pause does to the
    guest clock, which display modes a resize request offers, and what the guest waits for at boot."""
    display = f"dbus,addr={F['bus']}" if F.get("dbus_bus") else ("dbus,p2p=yes" if F.get("dbus_p2p") else "none")
    variant = "mapped-ram+multifd" if F.get("cap_mapped_ram") else "plain"

    vm = VM("follow", display=display, vdagent=bool(F.get("vdagent")))
    state = WORK / "follow.state"
    try:
        with step("followup:boot") as d:
            boot_guest(vm, d)
            d["boot_analysis"] = vm.g("systemd-analyze; systemd-analyze blame | head -n 12; "
                                      "systemd-analyze critical-chain sshd.service multi-user.target | head -n 40; "
                                      "ip -br addr; networkctl status --no-pager | head -n 25", timeout=60)
        if not vm.alive():
            return
        if display != "none":
            with step("followup:resize-modes") as d:
                modes = ("for c in /sys/class/drm/card*-Virtual-1; do echo \"$c $(cat $c/status)\"; cat $c/modes | head -n 40; done; "
                         "cat /sys/class/graphics/fb0/virtual_size")
                d["modes_before"] = vm.g(modes)
                for size in ("1600x900", "1920x1080", "1024x768"):
                    client = run_display_client(f"follow-resize-{size}", seconds=3, resize=size, **display_kwargs(vm))
                    d[f"after_{size}"] = {"console": client.get("console_after"), "resize": client.get("resize"),
                                          "errors": client.get("errors"), "guest": vm.g(modes)}
        with step("followup:pause-and-clock") as d:
            d["lag_before_s"] = clock_lag(vm)
            vm.qmp.cmd("stop")
            time.sleep(6)
            vm.qmp.cmd("cont")
            d["lag_after_6s_pause_s"] = clock_lag(vm)
            d["agent_set_time_explicit"] = timed(
                lambda: qga(vm.qga_sock, "guest-set-time", {"time": int(time.time() * 1e9)}, timeout=40))
            d["lag_after_fix_s"] = clock_lag(vm)
        with step("followup:agent-before-suspend") as d:
            agent_checks(vm, d)
        with step("followup:suspend") as d:
            suspend_to_file(vm, state, variant, d)
        if vm.alive():
            return
        time.sleep(8)
        with step("followup:restore") as d:
            restore_from_file(vm, state, variant, d)
        with step("followup:agent-after-restore") as d:
            agent_checks(vm, d)
            d["agent_restart"] = timed(lambda: vm.g("sudo systemctl restart qemu-guest-agent && echo restarted", timeout=120))
            d["agent_ping_after_restart"] = timed(lambda: qga(vm.qga_sock, "guest-ping", timeout=20))
        with step("followup:shutdown-after-agent-use") as d:
            vm.powerdown(d, timeout=150)
            d["serial_tail"] = tail(vm.serial, 12)
    finally:
        vm.quit()

    # Same again, but never talking to the guest agent: is shutdown after a restore fast then?
    vm = VM("follow2", display=display, vdagent=bool(F.get("vdagent")))
    try:
        with step("followup2:boot") as d:
            boot_guest(vm, d)
        if not vm.alive():
            return
        with step("followup2:suspend") as d:
            suspend_to_file(vm, state, variant, d)
        if vm.alive():
            return
        time.sleep(8)
        with step("followup2:restore") as d:
            restore_from_file(vm, state, variant, d)
            d["clock_lag_s"] = clock_lag(vm)
            time.sleep(20)
            d["clock_lag_20s_later_s"] = clock_lag(vm)
            d["time_sync_services"] = vm.g("timedatectl show -p NTP -p NTPSynchronized; systemctl is-active systemd-timesyncd")
        with step("followup2:shutdown-without-agent-use") as d:
            vm.powerdown(d, timeout=150)
            d["serial_tail"] = tail(vm.serial, 12)
    finally:
        vm.quit()
        with contextlib.suppress(OSError):
            os.unlink(state)


# ----------------------------------------------------------------- stage: gpu

def stage_gpu():
    working = F.get("gl_working") or []
    if not working:
        skip("gpu:*", "no GPU-accelerated display configuration started in the probes")
        return
    choice = next((w for w in working if w["client_got_scanout"]), working[0])
    if F.get("dbus_bus"):
        display = f"dbus,addr={F['bus']},gl=on{choice['suffix']}"
    else:
        display = f"dbus,p2p=yes,gl=on{choice['suffix']}"
    vm = VM("gpu", gpu="virtio-vga-gl", display=display)
    try:
        with step("gpu:boot") as d:
            d["display"] = display
            boot_guest(vm, d)
            d["guest_drm"] = vm.g("sudo dmesg | grep -iE 'drm|virtio_gpu|virgl' | tail -n 20; ls -la /dev/dri; "
                                  "sudo cat /sys/kernel/debug/dri/*/virtio-gpu-features")
        if not vm.alive():
            return
        with step("gpu:display") as d:
            d["client"] = run_display_client("gpu-core", seconds=4, **display_kwargs(vm))
            d["client_extras"] = run_display_client("gpu-extras", seconds=3, extras=True, **display_kwargs(vm))
            d["screenshot"] = screendump(vm, "gpu")
        with step("gpu:suspend-is-refused") as d:
            d["query_migrate"] = vm.qmp.try_cmd("query-migrate")
            d["migrate_attempt"] = vm.qmp.try_cmd("migrate", {"uri": f"file:{WORK}/gpu.state"})
            time.sleep(1)
            d["query_migrate_after"] = vm.qmp.try_cmd("query-migrate")
            d["still_running"] = vm.qmp.cmd("query-status")
        with step("gpu:guest-rendering") as d:
            d["install"] = vm.g("sudo pacman -Sy --noconfirm --needed mesa mesa-utils | tail -n 5", timeout=600)
            d["eglinfo"] = vm.g("sudo eglinfo -B | head -n 120", timeout=90)
            d["renderer_lines"] = [ln.strip() for ln in d["eglinfo"].splitlines() if "renderer" in ln.lower()]
            d["host_qemu_log"] = tail(vm.proc.log_path, 20)
            d["ok"] = any("virgl" in ln.lower() for ln in d["renderer_lines"])
        with step("gpu:force-off") as d:
            t0 = time.monotonic()
            vm.quit(d)
            d["force_off_s"] = round(time.monotonic() - t0, 2)
    finally:
        vm.quit()


# ------------------------------------------------------------- stage: environment

def stage_env():
    with step("env:host") as d:
        ok, why = kvm_usable()
        F["kvm"] = ok
        d["kvm_usable"] = ok
        d["kvm_error"] = why
        d["dev_kvm"] = sh("ls -l /dev/kvm")["out"].strip()
        d["uname"] = sh("uname -a")["out"].strip()
        d["os_release"] = sh("grep -E '^(PRETTY_NAME|ID|VERSION_ID)=' /etc/os-release")["out"]
        d["host_os_release"] = sh("grep -E '^(PRETTY_NAME|ID)=' /run/host/etc/os-release")["out"]
        d["id"] = sh("id")["out"].strip()
        d["cpu"] = sh("grep -m1 'model name' /proc/cpuinfo; nproc; grep -m1 -oE 'svm|vmx' /proc/cpuinfo")["out"]
        d["memory"] = sh("grep -E 'MemTotal|MemAvailable' /proc/meminfo")["out"]
        d["env"] = {k: os.environ.get(k) for k in ("XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP", "WAYLAND_DISPLAY", "DISPLAY",
                                                   "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "CONTAINER_ID")}
        d["work_dir_filesystem"] = sh(["df", "-hT", str(WORK)])["out"]
        d["nofile_limit"] = sh("ulimit -n; ulimit -Hn")["out"].split()
        d["userns"] = sh("unshare -Ur true && echo ok")
        d["pipewire_socket"] = os.path.exists(os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/nonexistent"), "pipewire-0"))
        d["systemd_user"] = sh("systemctl --user is-system-running")
        d["systemd_run_user"] = sh("systemd-run --user --wait --collect --quiet /bin/sh -c 'echo ran-in-unit; cat /etc/os-release | grep ^ID='", timeout=20)
    with step("env:software") as d:
        d["qemu"] = sh([QEMU, "--version"])
        d["qemu_img"] = sh(["qemu-img", "--version"])["out"].splitlines()[:1]
        F["virtiofsd"] = find_virtiofsd()
        d["virtiofsd_path"] = F["virtiofsd"]
        if F["virtiofsd"]:
            d["virtiofsd_version"] = sh([F["virtiofsd"], "--version"])["out"].strip()
            (OUT / "virtiofsd-help.txt").write_text(sh([F["virtiofsd"], "--help"])["out"])
        d["tools"] = {t: shutil.which(t) for t in ("swtpm", "passt", "xorriso", "ssh", "ssh-keygen", "dbus-daemon",
                                                   "busctl", "systemd-run", "weston", "cargo", "rustc", "pkg-config")}
        d["versions"] = sh("swtpm --version; passt --version 2>&1 | head -n 1; rustc --version; "
                           "pkg-config --modversion gtk4 libadwaita-1; python3 -c 'import gi; print(\"pygobject\", gi.__version__)'")["out"]
        d["packages"] = sh("pacman -Q qemu-base qemu-desktop qemu-img qemu-ui-dbus qemu-audio-dbus qemu-audio-pipewire "
                           "qemu-hw-display-virtio-vga-gl virglrenderer virtiofsd edk2-ovmf swtpm passt mesa gtk4 "
                           "libadwaita pipewire glib2 2>&1")["out"]
        for name, arg in (("display", "-display"), ("audiodev", "-audiodev"), ("machine", "-machine"),
                          ("device", "-device"), ("netdev", "-netdev"), ("chardev", "-chardev")):
            (OUT / f"qemu-help-{name}.txt").write_text(sh([QEMU, arg, "help"])["out"])
        d["qemu_modules"] = sorted(os.path.basename(p) for p in glob.glob("/usr/lib/qemu/*.so"))
        d["ok"] = d["qemu"]["rc"] == 0
    with step("env:gpu") as d:
        d["render_nodes"] = sh("ls -l /dev/dri/")["out"]
        cards = {}
        for dev in sorted(glob.glob("/sys/class/drm/card[0-9]") + glob.glob("/sys/class/drm/renderD*")):
            info = {}
            for attr in ("vendor", "device"):
                with contextlib.suppress(OSError):
                    info[attr] = open(f"{dev}/device/{attr}").read().strip()
            with contextlib.suppress(OSError):
                info["driver"] = os.path.basename(os.readlink(f"{dev}/device/driver"))
            cards[os.path.basename(dev)] = info
        d["drm_devices"] = cards
        d["lspci"] = sh("lspci -nnk | grep -A3 -E 'VGA|3D|Display'")["out"]
        d["nvidia"] = sh("cat /proc/driver/nvidia/version; nvidia-smi --query-gpu=name,driver_version --format=csv,noheader")["out"]
        d["eglinfo"] = sh("eglinfo -B | head -n 80", timeout=40)["out"]
        d["vulkan"] = sh("vulkaninfo --summary | head -n 60", timeout=40)["out"]
    return shutil.which(QEMU) is not None


# --------------------------------------------------------------------- main

def write_summary():
    lines = [f"roomy lab run {TS}", f"KVM: {F.get('kvm')}   machine: {F.get('machine')}   timeout multiplier: {TM}", ""]
    counts = {"ok": 0, "FAILED": 0, "skipped": 0}
    for s in REPORT["steps"]:
        verdict = "skipped" if "skipped" in s else {True: "ok", False: "FAILED"}.get(s["ok"], "interrupted")
        counts[verdict] = counts.get(verdict, 0) + 1
        note = s.get("skipped") or s.get("error") or s.get("data", {}).get("why") or ""
        lines.append(f"{verdict:11} {s.get('secs', ''):>7}  {s['name']}  {str(note)[:140]}")
    lines += ["", "  ".join(f"{k}: {v}" for k, v in counts.items())]
    (OUT / "SUMMARY.txt").write_text("\n".join(lines) + "\n")
    return lines


def main():
    global TM, RUN, _run_log
    wanted = [a for a in sys.argv[1:] if not a.startswith("-")]
    unknown = [a for a in wanted if a not in VM_STAGES + EXTRA_STAGES]
    if unknown or "-h" in sys.argv or "--help" in sys.argv:
        print(__doc__)
        return 2 if unknown else 0
    stages = wanted or list(VM_STAGES)

    for p in (OUT, LOGS, OUT / "dbus", OUT / "screens", WORK):
        p.mkdir(parents=True, exist_ok=True)
    _run_log = open(OUT / "run.log", "a")
    base = os.environ.get("XDG_RUNTIME_DIR")
    RUN = tempfile.mkdtemp(prefix="roomy-lab-", dir=base if base and os.access(base, os.W_OK) else "/tmp")
    atexit.register(cleanup)
    latest = DATA / "results" / "latest"
    with contextlib.suppress(OSError):
        if latest.is_symlink():
            latest.unlink()
        latest.symlink_to(TS)
    log(f"results: {OUT}")

    try:
        software_ok = stage_env()
        TM = float(os.environ.get("ROOMY_LAB_TM") or (1 if F.get("kvm") else 6))
        if not F.get("kvm"):
            log("WARNING: /dev/kvm is not usable here. Continuing with slow software emulation; "
                "timings will be meaningless.")
        if not software_ok:
            log("QEMU is not installed or not runnable; nothing more can be tested.")
        elif stage_probes():
            if not F.get("memfd"):
                skip("guest:*", "shared guest memory (memfd) did not work; VM stages need it")
            else:
                stage_image()
                if F.get("guest_ready"):
                    if "features" in stages:
                        stage_features()
                    if "suspend" in stages:
                        stage_suspend()
                    if "gpu" in stages:
                        stage_gpu()
                    if "followup" in stages:
                        stage_followup()
    except KeyboardInterrupt:
        log("interrupted")
        REPORT["interrupted"] = True
    finally:
        REPORT["facts"] = F
        REPORT["finished"] = time.strftime("%Y%m%d-%H%M%S")
        save_report()
        summary = write_summary()
        print("\n" + "\n".join(summary))
        log(f"done. Everything is in {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
