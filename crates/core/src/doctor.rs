//! `roomy doctor`: checks that this computer can run roomy's virtual machines.
//!
//! The checks do not just look for files. Wherever possible they start a
//! real, empty, paused virtual machine for a moment with exactly the devices
//! roomy uses, so "ok" means the feature works here and a failure carries
//! QEMU's own error message.

use crate::env::{Env, roomy_command};
use crate::host::{self, CpuVirtualization, Firmware, KvmAccess, Version};
use crate::process::{KillOnDrop, read_tail, run_capture, spawn_logged, wait_until, wait_with_limit};
use crate::qmp::Qmp;
use crate::{MIN_QEMU, shares};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// An optional feature is unavailable; virtual machines still run.
    Warning,
    /// roomy cannot run virtual machines until this is fixed.
    Failed,
    /// Not checked because something it depends on failed.
    Skipped,
}

impl Status {
    pub fn tag(self) -> &'static str {
        match self {
            Status::Ok => " OK ",
            Status::Warning => "WARN",
            Status::Failed => "FAIL",
            Status::Skipped => "SKIP",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub id: &'static str,
    pub title: &'static str,
    pub status: Status,
    /// One line: what was found.
    pub summary: String,
    /// What the user can do about it, when there is something to do.
    pub fix: Option<String>,
    /// Installing roomy's packages (`roomy setup`) is that something.
    pub setup_helps: bool,
    /// Raw output from the failing program, for the report.
    pub log: Option<String>,
}

impl Check {
    fn new(id: &'static str, title: &'static str, status: Status, summary: impl Into<String>) -> Check {
        Check { id, title, status, summary: summary.into(), fix: None, setup_helps: false, log: None }
    }

    fn fix(mut self, fix: impl Into<String>) -> Check {
        self.fix = Some(fix.into());
        self
    }

    /// The fix is to install packages: `lead` says which or why, and the command to run follows.
    fn fix_by_setup(mut self, lead: &str) -> Check {
        let run = format!("Run: {} setup", roomy_command());
        self.fix = Some(if lead.is_empty() { run } else { format!("{lead} {run}") });
        self.setup_helps = true;
        self
    }

    fn log(mut self, log: impl Into<String>) -> Check {
        let log = log.into();
        if !log.trim().is_empty() {
            self.log = Some(log);
        }
        self
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub generated_utc: String,
    /// Name and value pairs describing the system, in display order.
    pub facts: Vec<(&'static str, String)>,
    pub checks: Vec<Check>,
}

impl Report {
    pub fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }

    /// True when installing roomy's packages is the fix for something found here.
    pub fn setup_helps(&self) -> bool {
        self.checks.iter().any(|c| c.setup_helps)
    }

    /// True when nothing prevents roomy from running virtual machines.
    pub fn can_run_vms(&self) -> bool {
        self.count(Status::Failed) == 0 && self.count(Status::Skipped) == 0
    }

    pub fn verdict(&self) -> String {
        let failed = self.count(Status::Failed);
        let warnings = self.count(Status::Warning);
        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        match (failed, warnings) {
            (0, 0) => "Everything roomy needs is working.".to_owned(),
            (0, w) => format!("roomy can run virtual machines. {} unavailable.", plural(w, "optional feature")),
            (f, 0) => format!("roomy cannot run virtual machines yet: {} to fix.", plural(f, "problem")),
            (f, w) => format!(
                "roomy cannot run virtual machines yet: {} to fix. {} also unavailable.",
                plural(f, "problem"),
                plural(w, "optional feature")
            ),
        }
    }

    /// The whole report as plain text, made to be pasted into a bug report or a search engine.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "roomy doctor report");
        let _ = writeln!(out, "Generated: {} UTC", self.generated_utc);
        let width = self.facts.iter().map(|(name, _)| name.len()).max().unwrap_or(0) + 1;
        for (name, value) in &self.facts {
            let _ = writeln!(out, "{:<width$} {value}", format!("{name}:"));
        }
        let _ = writeln!(out);
        for check in &self.checks {
            let _ = writeln!(out, "[{}] {}: {}", check.status.tag(), check.title, check.summary);
            if let Some(fix) = &check.fix {
                for (i, line) in fix.lines().enumerate() {
                    let _ = writeln!(out, "       {}{line}", if i == 0 { "Fix: " } else { "     " });
                }
            }
            if let Some(log) = &check.log {
                let _ = writeln!(out, "       Output:");
                for line in log.lines() {
                    let _ = writeln!(out, "         | {line}");
                }
            }
        }
        let _ = writeln!(out);
        let _ = writeln!(out, "{}", self.verdict());
        out
    }

    pub fn to_json(&self) -> Value {
        json!({
            "generated_utc": self.generated_utc,
            "can_run_vms": self.can_run_vms(),
            "verdict": self.verdict(),
            "facts": self.facts.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect::<Vec<_>>(),
            "checks": self.checks.iter().map(|c| json!({
                "id": c.id,
                "title": c.title,
                "status": c.status.tag().trim().to_ascii_lowercase(),
                "summary": c.summary,
                "fix": c.fix,
                "output": c.log,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Runs every check. Takes a few seconds; never panics on a broken host.
pub fn run(env: &Env) -> Report {
    let os = host::os_release(env);
    let mut facts = vec![
        ("roomy", env!("CARGO_PKG_VERSION").to_owned()),
        ("System", os.pretty_name.clone()),
        ("Kernel", host::kernel_release(env)),
    ];
    let mut checks = Vec::new();

    let cpu = host::cpu_virtualization(env);
    checks.push(check_cpu(cpu));
    checks.push(check_kvm(env, cpu));

    let (qemu_check, qemu) = check_qemu(env);
    checks.push(qemu_check);
    if let Some((path, version)) = &qemu {
        facts.push(("QEMU", format!("{version} ({})", path.display())));
    }
    checks.push(check_qemu_img(env));

    let firmware = host::find_firmware(env);
    let virtiofsd = host::find_virtiofsd(env);
    match (&qemu, Scratch::create(&env.runtime_dir)) {
        (Some((qemu, _)), Ok(scratch)) => {
            let trial = Trial { qemu, scratch: &scratch };
            let (start, machine) = check_vm_start(&trial);
            if let Some(machine) = machine {
                facts.push(("Machine type", machine));
            }
            let started = start.status == Status::Ok;
            checks.push(start);
            if started {
                let devices = check_devices(&trial);
                let display_works = devices.status == Status::Ok;
                checks.push(devices);
                checks.push(check_firmware(&trial, firmware.as_ref()));
                checks.push(check_shared_folders(&trial, virtiofsd.as_deref()));
                let render_node = env.render_node.clone().or_else(|| host::display_gpu(env).map(|gpu| gpu.render_node));
                if let Some(node) = &render_node {
                    facts.push(("Graphics card for VMs", node.display().to_string()));
                }
                // Accelerated graphics go through the same display: without it there is nothing to learn here,
                // and QEMU's complaint would only be the one already reported.
                checks.push(if display_works {
                    check_gpu(&trial, render_node.as_deref())
                } else {
                    let (id, title) = TRIAL_CHECKS[3];
                    Check::new(id, title, Status::Skipped, "not checked: the display has to work first (see above)")
                });
            } else {
                checks.extend(skipped_trials("starting a virtual machine failed"));
            }
        }
        (Some(_), Err(e)) => {
            checks.push(
                Check::new("vm-start", "Starting a virtual machine", Status::Failed,
                    format!("cannot create a working directory in {}: {e}", env.runtime_dir.display()))
                .fix("roomy keeps control sockets in your session's runtime directory (XDG_RUNTIME_DIR). Make sure you are in a normal login session and that directory is writable."),
            );
            checks.extend(skipped_trials("no working directory"));
        }
        (None, _) => {
            checks.push(Check::new(
                "vm-start",
                "Starting a virtual machine",
                Status::Skipped,
                "not checked: no usable QEMU",
            ));
            checks.extend(skipped_trials("no usable QEMU"));
        }
    }

    Report { generated_utc: utc_timestamp(SystemTime::now()), facts, checks }
}

/// Name of the program that shows roomy's window. It is separate from the
/// `roomy` command so that the checks and `roomy setup` still run on a
/// computer where the graphical libraries are missing.
pub const WINDOW_PROGRAM: &str = "roomy-gui";

/// Checks that roomy's window can start: that its program is installed and
/// that the graphical libraries it needs load. Never blocks running VMs,
/// which work from the command line regardless.
pub fn check_window(program: Option<&Path>) -> Check {
    const TITLE: &str = "roomy's window";
    let Some(program) = program else {
        return Check::new(
            "window",
            TITLE,
            Status::Warning,
            format!("{WINDOW_PROGRAM} is not installed next to roomy"),
        )
        .fix("Reinstall roomy; virtual machines can still be used with 'roomy vm' commands meanwhile.");
    };
    match run_capture(program, &["--probe"], Duration::from_secs(15)) {
        Ok(out) if out.status.success() => Check::new("window", TITLE, Status::Ok, "the window can start"),
        Ok(out) => Check::new("window", TITLE, Status::Warning, "the window cannot start on this computer")
            .fix_by_setup("Install GTK 4 (4.14 or newer) and libadwaita (1.5 or newer).")
            .log(format!("{}{}", out.stdout, out.stderr)),
        Err(e) => Check::new("window", TITLE, Status::Warning, format!("{} could not be run: {e}", program.display()))
            .fix("Reinstall roomy; virtual machines can still be used with 'roomy vm' commands meanwhile."),
    }
}

/// Checks that the library roomy's window plays sound through can be loaded.
/// Without it everything works, silently.
pub fn check_sound() -> Check {
    const TITLE: &str = "Sound";
    // SAFETY: dlopen with a valid name; closing a handle we just opened.
    let found = unsafe {
        let handle = libc::dlopen(c"libpulse-simple.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if !handle.is_null() {
            libc::dlclose(handle);
        }
        !handle.is_null()
    };
    if found {
        Check::new("sound", TITLE, Status::Ok, "the sound library is installed")
    } else {
        Check::new(
            "sound",
            TITLE,
            Status::Warning,
            "the sound library (libpulse) is not installed; virtual machines will be silent",
        )
        .fix_by_setup("Install the PulseAudio client library (libpulse).")
    }
}

const TRIAL_CHECKS: [(&str, &str); 4] = [
    ("devices", "Display, sound and clipboard devices"),
    ("firmware", "UEFI firmware"),
    ("shared-folders", "Shared folders"),
    ("gpu", "GPU acceleration"),
];

/// The part of QEMU its refusal says is not installed. Distributions package QEMU's displays,
/// sound outputs and devices separately, and QEMU names the first one it misses.
fn missing_piece(log: &str) -> Option<String> {
    if log.contains("Display 'dbus' is not available") {
        return Some("its D-Bus display module, which roomy shows the screen through".to_owned());
    }
    if log.contains("audio driver") && log.contains("dbus") {
        return Some("its D-Bus audio module, which roomy plays sound through".to_owned());
    }
    let device = log.lines().find_map(|line| {
        let before = line.strip_suffix("' is not a valid device model name")?;
        before.rsplit_once('\'').map(|(_, name)| name)
    })?;
    Some(format!("the '{device}' device"))
}

fn skipped_trials(why: &str) -> Vec<Check> {
    TRIAL_CHECKS
        .iter()
        .map(|(id, title)| Check::new(id, title, Status::Skipped, format!("not checked: {why}")))
        .collect()
}

fn check_cpu(cpu: Option<CpuVirtualization>) -> Check {
    const TITLE: &str = "CPU virtualization";
    match cpu {
        Some(kind) => Check::new("cpu", TITLE, Status::Ok, format!("{} is available", kind.name())),
        None => Check::new("cpu", TITLE, Status::Failed, "the processor does not report AMD-V or Intel VT-x").fix(
            "Enable virtualization (often called SVM, AMD-V, VT-x or Intel Virtualization Technology) in your \
             computer's firmware setup (BIOS/UEFI).\nIf this system is itself a virtual machine, enable nested \
             virtualization on its host.",
        ),
    }
}

fn check_kvm(env: &Env, cpu: Option<CpuVirtualization>) -> Check {
    const TITLE: &str = "KVM access";
    let module = cpu.map_or("kvm_amd or kvm_intel", CpuVirtualization::kvm_module);
    match host::kvm_access(env) {
        KvmAccess::Ready => Check::new("kvm", TITLE, Status::Ok, "/dev/kvm is usable"),
        KvmAccess::Missing => Check::new("kvm", TITLE, Status::Failed, "/dev/kvm does not exist").fix(format!(
            "The KVM kernel module is not loaded. Try: sudo modprobe {module}\nIf that fails, virtualization is \
             probably disabled in your computer's firmware setup. Inside a container, /dev/kvm must be passed in."
        )),
        KvmAccess::Denied => {
            Check::new("kvm", TITLE, Status::Failed, "/dev/kvm exists but you are not allowed to use it")
                .fix("Add your user to the kvm group, then log out and back in:\n  sudo usermod -aG kvm \"$USER\"")
        }
        KvmAccess::Broken(why) => {
            Check::new("kvm", TITLE, Status::Failed, format!("/dev/kvm does not respond like KVM: {why}"))
        }
    }
}

fn check_qemu(env: &Env) -> (Check, Option<(PathBuf, Version)>) {
    const TITLE: &str = "QEMU";
    let Some(path) = env.find_program(host::QEMU_PROGRAM) else {
        let check = Check::new("qemu", TITLE, Status::Failed, format!("{} is not installed", host::QEMU_PROGRAM))
            .fix_by_setup("");
        return (check, None);
    };
    match host::tool_version(&path) {
        Ok(version) if version >= MIN_QEMU => {
            (Check::new("qemu", TITLE, Status::Ok, format!("version {version}")), Some((path, version)))
        }
        Ok(version) => {
            let check = Check::new(
                "qemu",
                TITLE,
                Status::Failed,
                format!("version {version} is too old; roomy needs {MIN_QEMU} or newer"),
            )
            .fix("Update your system. If your distribution does not offer a newer QEMU, roomy cannot run on it yet.");
            (check, None)
        }
        Err(why) => {
            (Check::new("qemu", TITLE, Status::Failed, format!("{} could not be run: {why}", path.display())), None)
        }
    }
}

fn check_qemu_img(env: &Env) -> Check {
    const TITLE: &str = "Disk image tool";
    match env.find_program("qemu-img") {
        Some(path) => Check::new("qemu-img", TITLE, Status::Ok, format!("qemu-img found at {}", path.display())),
        None => Check::new("qemu-img", TITLE, Status::Failed, "qemu-img is not installed").fix_by_setup(""),
    }
}

fn check_vm_start(trial: &Trial) -> (Check, Option<String>) {
    const TITLE: &str = "Starting a virtual machine";
    let outcome = trial.run("start", &["-display", "none"], &["query-kvm", "query-machines"]);
    if !outcome.started {
        let log = outcome.log.to_ascii_lowercase();
        let mut check = Check::new("vm-start", TITLE, Status::Failed, "QEMU could not start an empty virtual machine")
            .log(outcome.log);
        if log.contains("kvm") {
            check = check.fix("QEMU could not use KVM. See the KVM access check above.");
        }
        return (check, None);
    }
    let kvm_enabled = outcome.reply("query-kvm").is_some_and(|r| r["enabled"] == true);
    let machine = outcome.reply("query-machines").and_then(|machines| {
        machines.as_array()?.iter().find(|m| m["alias"] == "q35")?["name"].as_str().map(str::to_owned)
    });
    if !kvm_enabled {
        let check = Check::new("vm-start", TITLE, Status::Failed, "QEMU started, but without hardware acceleration")
            .log(outcome.log);
        return (check, machine);
    }
    (Check::new("vm-start", TITLE, Status::Ok, "an empty virtual machine starts with hardware acceleration"), machine)
}

fn check_devices(trial: &Trial) -> Check {
    let (id, title) = TRIAL_CHECKS[0];
    let outcome = trial.run(
        "devices",
        &[
            "-display",
            "dbus,p2p=yes,audiodev=snd0",
            "-audiodev",
            "dbus,id=snd0",
            "-device",
            "virtio-vga",
            "-device",
            "ich9-intel-hda",
            "-device",
            "hda-duplex,audiodev=snd0",
            "-device",
            "virtio-serial-pci",
            "-chardev",
            "qemu-vdagent,id=vdagent,clipboard=on,mouse=off",
            "-device",
            "virtserialport,chardev=vdagent,name=com.redhat.spice.0",
        ],
        &[],
    );
    if outcome.started {
        Check::new(id, title, Status::Ok, "QEMU accepts roomy's display, audio and clipboard configuration")
    } else if let Some(piece) = missing_piece(&outcome.log) {
        Check::new(id, title, Status::Failed, format!("QEMU is installed without {piece}"))
            .fix_by_setup("That is a package of its own on this system.")
            .log(outcome.log)
    } else {
        Check::new(id, title, Status::Failed, "QEMU rejected roomy's display, audio or clipboard configuration")
            .fix_by_setup("QEMU's D-Bus display and audio modules or its virtio graphics devices are probably missing.")
            .log(outcome.log)
    }
}

fn check_firmware(trial: &Trial, firmware: Option<&Firmware>) -> Check {
    let (id, title) = TRIAL_CHECKS[1];
    let Some(firmware) = firmware else {
        return Check::new(id, title, Status::Failed, "no UEFI firmware for virtual machines was found")
            .fix_by_setup("Install the OVMF / edk2 firmware package for x86-64.");
    };
    let vars = trial.scratch.dir.join("firmware-vars");
    if let Err(e) = fs::copy(&firmware.vars_template, &vars) {
        return Check::new(id, title, Status::Failed, format!("cannot copy {}: {e}", firmware.vars_template.display()));
    }
    let code = format!("if=pflash,unit=0,readonly=on,format={},file={}", firmware.code_format, firmware.code.display());
    let vars = format!("if=pflash,unit=1,format={},file={}", firmware.vars_format, vars.display());
    let outcome = trial.run("firmware", &["-display", "none", "-drive", &code, "-drive", &vars], &[]);
    if outcome.started {
        Check::new(id, title, Status::Ok, format!("{}", firmware.code.display()))
    } else {
        Check::new(id, title, Status::Failed, format!("QEMU could not load {}", firmware.code.display()))
            .log(outcome.log)
    }
}

fn check_shared_folders(trial: &Trial, virtiofsd: Option<&Path>) -> Check {
    let (id, title) = TRIAL_CHECKS[2];
    let Some(virtiofsd) = virtiofsd else {
        return Check::new(id, title, Status::Warning, "virtiofsd is not installed; shared folders are unavailable")
            .fix_by_setup("");
    };
    let unavailable = |what: &str, log: String| {
        Check::new(id, title, Status::Warning, format!("{what}; shared folders are unavailable")).log(log)
    };
    let shared = trial.scratch.dir.join("shared");
    let socket = trial.scratch.dir.join("fs.sock");
    let log_path = trial.scratch.dir.join("virtiofsd.log");
    if let Err(e) = fs::create_dir_all(&shared) {
        return unavailable("cannot create a test folder", e.to_string());
    }
    // SAFETY: getuid and getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let args = shares::virtiofsd_args(&socket, &shared, uid, gid, shares::DEFAULT_GUEST_UID);
    let mut daemon = match spawn_logged(virtiofsd, &args, &log_path) {
        Ok(child) => KillOnDrop(child),
        Err(e) => return unavailable("virtiofsd could not be started", e.to_string()),
    };
    let came_up = wait_until(Duration::from_secs(8), || socket.exists() || !daemon.running());
    if !came_up || !daemon.running() {
        return unavailable("virtiofsd did not accept roomy's configuration", read_tail(&log_path, 15));
    }
    let chardev = format!("socket,id=fs0,path={}", socket.display());
    let outcome = trial.run(
        "shared-folders",
        &["-display", "none", "-chardev", &chardev, "-device", "vhost-user-fs-pci,chardev=fs0,tag=roomy-doctor"],
        &[],
    );
    if outcome.started {
        Check::new(id, title, Status::Ok, format!("virtiofsd works without root ({})", virtiofsd.display()))
    } else {
        let log = format!("{}\n{}", outcome.log, read_tail(&log_path, 15));
        unavailable("QEMU could not attach a shared folder", log)
    }
}

fn check_gpu(trial: &Trial, render_node: Option<&Path>) -> Check {
    let (id, title) = TRIAL_CHECKS[3];
    // On the same graphics card a VM would use.
    let display = match render_node {
        Some(node) => format!("dbus,p2p=yes,gl=on,rendernode={}", node.display()),
        None => "dbus,p2p=yes,gl=on".to_owned(),
    };
    let outcome = trial.run("gpu", &["-display", &display, "-device", "virtio-vga-gl"], &[]);
    if outcome.started {
        Check::new(id, title, Status::Ok, "QEMU can render with this computer's graphics driver")
    } else if let Some(piece) = missing_piece(&outcome.log) {
        let summary = format!("QEMU is installed without {piece}; the GPU acceleration switch will be unavailable");
        Check::new(id, title, Status::Warning, summary)
            .fix_by_setup("Virtual machines still work without it. To get it:")
            .log(outcome.log)
    } else {
        Check::new(
            id,
            title,
            Status::Warning,
            "QEMU could not set up accelerated graphics; the GPU acceleration switch will be unavailable",
        )
        .fix_by_setup(
            "Virtual machines still work without it. Check that your graphics driver and QEMU's OpenGL modules are \
             installed.",
        )
        .log(outcome.log)
    }
}

/// A private directory for sockets and logs, removed when dropped.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn create(parent: &Path) -> std::io::Result<Scratch> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let dir = parent.join(format!("roomy-doctor-{}-{nanos}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Scratch { dir })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Trial<'a> {
    qemu: &'a Path,
    scratch: &'a Scratch,
}

struct Outcome {
    /// QEMU came up with this configuration and answered on its control socket.
    started: bool,
    /// What QEMU printed.
    log: String,
    replies: Vec<(String, Value)>,
}

impl Outcome {
    fn reply(&self, query: &str) -> Option<&Value> {
        self.replies.iter().find(|(name, _)| name == query).map(|(_, value)| value)
    }
}

impl Trial<'_> {
    /// Starts a paused virtual machine without disks, using roomy's machine
    /// setup plus `extra` arguments, asks it `queries`, and shuts it down.
    fn run(&self, name: &str, extra: &[&str], queries: &[&str]) -> Outcome {
        let socket = self.scratch.dir.join(format!("{name}.qmp"));
        let log_path = self.scratch.dir.join(format!("{name}.log"));
        let qmp_arg = format!("unix:{},server=on,wait=off", socket.display());
        let mut args: Vec<String> = [
            "-nodefaults",
            "-no-user-config",
            "-S",
            "-name",
            "roomy-doctor",
            "-machine",
            "q35,memory-backend=mem,vmport=off",
            "-accel",
            "kvm",
            "-cpu",
            "host",
            "-m",
            "256",
            "-object",
            "memory-backend-memfd,id=mem,size=256M,share=on",
            "-qmp",
            &qmp_arg,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(extra.iter().map(|s| s.to_string()));

        let mut qemu = match spawn_logged(self.qemu, &args, &log_path) {
            Ok(child) => KillOnDrop(child),
            Err(e) => {
                return Outcome {
                    started: false,
                    log: format!("could not run {}: {e}", self.qemu.display()),
                    replies: vec![],
                };
            }
        };
        let mut replies = Vec::new();
        let started = match Qmp::connect(&socket, Duration::from_secs(20), || qemu.running()) {
            Ok(mut qmp) => {
                qmp.command_timeout = Duration::from_secs(10);
                for query in queries {
                    if let Ok(value) = qmp.execute(query, None) {
                        replies.push((query.to_string(), value));
                    }
                }
                let _ = qmp.execute("quit", None);
                true
            }
            Err(_) => false,
        };
        let _ = wait_with_limit(&mut qemu.0, Duration::from_secs(5));
        drop(qemu);
        Outcome { started, log: read_tail(&log_path, 20), replies }
    }
}

/// Formats a time as `YYYY-MM-DD HH:MM` in UTC.
/// UTC date and time to the second, for log lines.
pub(crate) fn utc_timestamp_secs(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{}:{:02}", utc_timestamp(time), secs % 60)
}

fn utc_timestamp(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3_600, rem % 3_600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_calendar_dates() {
        let at = |secs| utc_timestamp(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01 00:00");
        assert_eq!(at(951_782_400), "2000-02-29 00:00");
        assert_eq!(at(1_794_866_290), "2026-11-16 21:58");
        assert_eq!(at(1_798_761_599), "2026-12-31 23:59");
    }

    fn report(statuses: &[Status]) -> Report {
        Report {
            generated_utc: "2026-11-16 21:58".into(),
            facts: vec![("roomy", "0.1.0".into()), ("System", "Test Linux".into())],
            checks: statuses.iter().map(|s| Check::new("x", "Example", *s, "summary")).collect(),
        }
    }

    #[test]
    fn verdict_separates_blocking_problems_from_optional_features() {
        assert_eq!(report(&[Status::Ok]).verdict(), "Everything roomy needs is working.");
        assert!(report(&[Status::Ok]).can_run_vms());
        let warned = report(&[Status::Ok, Status::Warning]);
        assert!(warned.can_run_vms());
        assert_eq!(warned.verdict(), "roomy can run virtual machines. 1 optional feature unavailable.");
        let failed = report(&[Status::Failed, Status::Failed, Status::Skipped, Status::Warning]);
        assert!(!failed.can_run_vms());
        assert_eq!(
            failed.verdict(),
            "roomy cannot run virtual machines yet: 2 problems to fix. 1 optional feature also unavailable."
        );
    }

    #[test]
    fn text_report_shows_fix_and_program_output() {
        let mut r = report(&[]);
        r.checks.push(
            Check::new("kvm", "KVM access", Status::Failed, "/dev/kvm does not exist")
                .fix("first line\nsecond line")
                .log("qemu: boom\n"),
        );
        let text = r.to_text();
        assert!(text.contains("roomy:  0.1.0\nSystem: Test Linux\n"), "{text}");
        assert!(text.contains("[FAIL] KVM access: /dev/kvm does not exist\n       Fix: first line\n            second line\n       Output:\n         | qemu: boom\n"), "{text}");
        assert!(text.ends_with("roomy cannot run virtual machines yet: 1 problem to fix.\n"), "{text}");
    }
}
