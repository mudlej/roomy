//! The `roomy` program end to end on a fake host: the real binary, a real
//! detached supervisor process and control socket, with stand-in QEMU tools.

#[path = "../../core/tests/common/mod.rs"]
mod common;

use common::FakeHost;
use roomy_core::process::wait_until;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

struct Cli<'a> {
    host: &'a FakeHost,
}

impl Cli<'_> {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_roomy"))
            .args(args)
            // The stand-ins come first; the system directories are only there for their `python3`.
            .env("PATH", format!("{}:/usr/bin:/bin", self.host.base.join("bin").display()))
            .env("ROOMY_HOST_ROOT", self.host.base.join("root"))
            .env("XDG_RUNTIME_DIR", self.host.base.join("run"))
            .env("HOME", self.host.base.join("home"))
            .env_remove("XDG_CONFIG_HOME")
            // Never start a real window from a test, even when one is installed next to the binary.
            .env("ROOMY_WINDOW_PROGRAM", self.host.base.join("bin/roomy-gui"))
            .output()
            .unwrap()
    }

    /// Runs a command that must succeed and returns what it printed.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "roomy {args:?} failed:\n{stdout}\n{stderr}");
        stdout.trim().to_owned()
    }

    /// Runs a command that must fail and returns its error message.
    fn err(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert_eq!(out.status.code(), Some(1), "roomy {args:?} should have failed with status 1: {out:?}");
        String::from_utf8_lossy(&out.stderr).trim().to_owned()
    }

    fn wait_for_status(&self, vm: &str, wanted: &str) {
        let became = wait_until(Duration::from_secs(15), || self.ok(&["vm", "status", vm]) == wanted);
        assert!(became, "the VM never became {wanted}");
    }
}

#[test]
fn a_vm_goes_through_its_whole_life_from_the_command_line() {
    let host = FakeHost::healthy("cli");
    let cli = Cli { host: &host };
    let home_vms = host.base.join("home/Virtual-Machines");

    // Default location is ~/Virtual-Machines/<name>.
    let created = cli.ok(&["vm", "create", "--name", "Test VM", "--disk", "10", "--forward", "2222:22"]);
    let vm_dir = home_vms.join("Test VM").canonicalize().unwrap();
    assert_eq!(created, format!("Created {}", vm_dir.display()));
    let vm = vm_dir.to_str().unwrap();
    assert_eq!(cli.ok(&["vm", "status", vm]), "off");

    // `start` returns once the guest runs; the supervisor it launched lives on.
    assert_eq!(cli.ok(&["vm", "start", vm]), "running");
    assert_eq!(cli.ok(&["vm", "status", vm]), "running");
    assert_eq!(cli.err(&["vm", "start", vm]), "roomy: the virtual machine is already running");

    assert_eq!(cli.ok(&["vm", "pause", vm]), "paused");
    let resumed = cli.ok(&["vm", "resume", vm]);
    assert!(resumed.starts_with("running\nNote: The guest's clock may be behind"), "{resumed}");

    assert_eq!(cli.ok(&["vm", "suspend", vm]), "suspended");
    cli.wait_for_status(vm, "suspended");
    assert_eq!(cli.err(&["vm", "pause", vm]), "roomy: the virtual machine is not running");
    assert_eq!(cli.ok(&["vm", "start", vm]), "running (continued from saved state)");

    assert_eq!(cli.ok(&["vm", "shutdown", vm]), "stopping");
    cli.wait_for_status(vm, "off");
    assert!(Path::new(vm).join("logs/supervisor.log").is_file());

    // Suspend, then give up on the saved state.
    assert_eq!(cli.ok(&["vm", "start", vm]), "running");
    assert_eq!(cli.ok(&["vm", "suspend", vm]), "suspended");
    cli.wait_for_status(vm, "suspended");
    assert_eq!(cli.ok(&["vm", "discard-state", vm]), "off (saved state discarded)");
    assert_eq!(
        cli.err(&["vm", "discard-state", vm]),
        "roomy: the virtual machine is not suspended; there is no saved state to discard"
    );

    assert_eq!(cli.ok(&["vm", "start", vm]), "running");
    assert_eq!(cli.ok(&["vm", "force-off", vm]), "off");
    cli.wait_for_status(vm, "off");
}

#[test]
fn a_start_that_fails_reports_qemus_error_to_the_caller() {
    let host = FakeHost::healthy("clifail");
    let cli = Cli { host: &host };
    let vms = host.base.join("vms");
    cli.ok(&["vm", "create", "--name", "broken", "--in", vms.to_str().unwrap()]);
    let vm = vms.join("broken");
    host.configure_qemu(r#"{"reject": ["virtio-vga"]}"#);
    let error = cli.err(&["vm", "start", vm.to_str().unwrap()]);
    assert!(error.contains("QEMU did not start"), "{error}");
    assert!(error.contains("fake refusal of 'virtio-vga'"), "{error}");
    assert_eq!(cli.ok(&["vm", "status", vm.to_str().unwrap()]), "off");
}

#[test]
fn mistakes_on_the_command_line_are_explained() {
    let host = FakeHost::healthy("cliusage");
    let cli = Cli { host: &host };
    assert_eq!(cli.err(&["vm", "create"]), "roomy: 'roomy vm create' needs --name <name>");
    assert_eq!(
        cli.err(&["vm", "create", "--name", "x", "--cpus", "many"]),
        "roomy: --cpus: 'many' is not a valid number"
    );
    let not_a_vm = cli.err(&["vm", "status", host.base.to_str().unwrap()]);
    assert!(not_a_vm.contains("is not a roomy virtual machine"), "{not_a_vm}");
    let unknown = cli.run(&["vm", "frobnicate", "/tmp"]);
    assert_eq!(unknown.status.code(), Some(2), "an unknown action is reported before the folder is looked at");
    assert!(String::from_utf8_lossy(&unknown.stderr).starts_with("roomy: unknown action 'frobnicate' for 'roomy vm'"));
}

#[test]
fn the_library_lists_vms_and_deleting_removes_only_roomys_files() {
    let host = FakeHost::healthy("clilib");
    let cli = Cli { host: &host };
    assert!(cli.ok(&["vm", "list"]).starts_with("No virtual machines yet"));

    let elsewhere = host.base.join("elsewhere");
    cli.ok(&["vm", "create", "--name", "one"]);
    cli.ok(&["vm", "create", "--name", "two", "--in", elsewhere.to_str().unwrap()]);
    let one = host.base.join("home/Virtual-Machines/one").canonicalize().unwrap();
    let two = elsewhere.join("two").canonicalize().unwrap();
    let (one_arg, two_arg) = (one.to_str().unwrap(), two.to_str().unwrap());
    let listed = cli.ok(&["vm", "list"]);
    let lines: Vec<&str> = listed.lines().collect();
    assert_eq!(lines.len(), 2, "{listed}");
    assert!(lines[0].starts_with("off") && lines[0].contains("one") && lines[0].contains(one_arg), "{listed}");
    assert!(lines[1].contains(two_arg), "{listed}");

    // Forgetting keeps the files; adding brings the VM back.
    assert_eq!(cli.ok(&["vm", "forget", two_arg]), "removed from the library; its files were not touched");
    assert!(two.join("disk.qcow2").is_file());
    assert_eq!(cli.ok(&["vm", "list"]).lines().count(), 1);
    assert_eq!(cli.ok(&["vm", "add", two_arg]), "added to the library");
    assert_eq!(cli.ok(&["vm", "add", two_arg]), "already in the library");

    // Deleting needs an explicit yes, refuses while the VM runs, and leaves the user's own files alone.
    assert!(cli.err(&["vm", "delete", two_arg]).contains("permanently deletes"));
    assert!(two.join("disk.qcow2").is_file());
    cli.ok(&["vm", "start", two_arg]);
    assert!(cli.err(&["vm", "delete", two_arg, "--yes"]).contains("turn it off before deleting"));
    cli.ok(&["vm", "force-off", two_arg]);
    cli.wait_for_status(two_arg, "off");
    std::fs::write(two.join("my-notes.txt"), "mine").unwrap();
    assert!(cli.ok(&["vm", "delete", two_arg, "--yes"]).contains("1 other item(s)"));
    assert!(two.join("my-notes.txt").is_file() && !two.join("disk.qcow2").exists() && !two.join("roomy.toml").exists());

    // The small picture of its screen that the window keeps is roomy's own file too.
    std::fs::write(one.join("thumbnail.png"), "picture").unwrap();
    assert_eq!(cli.ok(&["vm", "delete", one_arg, "--yes"]), "deleted");
    assert!(!one.exists());
    assert!(cli.ok(&["vm", "list"]).starts_with("No virtual machines yet"));

    // A VM whose folder went away stays listed as missing instead of silently vanishing.
    cli.ok(&["vm", "create", "--name", "three"]);
    std::fs::remove_dir_all(host.base.join("home/Virtual-Machines/three")).unwrap();
    assert!(cli.ok(&["vm", "list"]).starts_with("missing"));
}

#[test]
fn without_arguments_the_window_opens_or_its_absence_is_explained() {
    let host = FakeHost::healthy("cliwin");
    let cli = Cli { host: &host };
    let window = host.base.join("bin/roomy-gui");

    // The fake host has no KVM, so the doctor's exit status is not what is being looked at here.
    let doctor = || String::from_utf8_lossy(&cli.run(&["doctor"]).stdout).into_owned();

    // Not installed: explained, and the checks say the same without blocking VMs.
    let missing = cli.err(&[]);
    assert!(missing.contains("roomy-gui is not installed next to roomy"), "{missing}");
    assert!(missing.contains("roomy doctor"), "{missing}");
    let report = doctor();
    assert!(report.contains("[WARN] roomy's window: roomy-gui is not installed"), "{report}");

    // Installed but unable to load its libraries: the loader's message is passed on.
    host.install("fake-window-broken", &window);
    let broken = cli.err(&[]);
    assert!(broken.contains("the window cannot start on this computer"), "{broken}");
    assert!(broken.contains("libadwaita-1.so.0"), "{broken}");
    assert!(doctor().contains("Install GTK 4"));

    // Working: roomy becomes the window program and tells it where roomy is.
    host.install("fake-window", &window);
    assert_eq!(cli.ok(&[]), format!("window opened by {}", env!("CARGO_BIN_EXE_roomy")));
    assert!(doctor().contains("[ OK ] roomy's window: the window can start"));
}

#[test]
fn settings_change_now_or_at_the_next_boot_and_say_which() {
    let host = FakeHost::healthy("cliset");
    let cli = Cli { host: &host };
    cli.ok(&["vm", "create", "--name", "box", "--disk", "10"]);
    let dir = host.base.join("home/Virtual-Machines/box").canonicalize().unwrap();
    let vm = dir.to_str().unwrap();
    let shared = host.base.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    let config = || std::fs::read_to_string(dir.join("roomy.toml")).unwrap();

    // Off: everything is simply changed.
    let off =
        cli.ok(&["vm", "set", vm, "--cpus", "6", "--share", &format!("docs={}", shared.display()), "--disk", "20"]);
    assert!(off.contains("processors: changed") && off.contains("shared folders: changed"), "{off}");
    assert!(off.contains("disk size: changed") && off.contains("grow its partition"), "{off}");
    assert_eq!(std::fs::read_to_string(dir.join("disk.qcow2")).unwrap(), format!("qcow2 {}", 20u64 << 30));
    assert_eq!(cli.ok(&["vm", "set", vm, "--cpus", "6"]), "nothing to change");

    // Mistakes change nothing.
    assert!(cli.err(&["vm", "set", vm, "--disk", "5"]).contains("cannot be made smaller"));
    assert!(cli.err(&["vm", "set", vm, "--cpus", "0", "--name", "renamed"]).contains("at least 1 processor"));
    assert!(cli.err(&["vm", "set", vm, "--share", "gone=/nonexistent/folder"]).contains("does not exist"));
    assert!(cli.err(&["vm", "set", vm, "--unshare", "nope"]).contains("no shared folder named"));
    assert!(cli.err(&["vm", "set", vm, "--gpu", "maybe"]).contains("'on' or 'off'"));
    assert!(config().contains("name = \"box\"") && config().contains("cpus = 6"));

    // Running: the network switch and the disk act at once, hardware waits for the next boot.
    cli.ok(&["vm", "start", vm]);
    let running =
        cli.ok(&["vm", "set", vm, "--network", "off", "--memory", "8192", "--disk", "30", "--microphone", "on"]);
    assert!(running.contains("network: changed") && running.contains("microphone: changed"), "{running}");
    assert!(running.contains("memory: saved; takes effect the next time"), "{running}");
    assert!(running.contains("disk size: changed"), "{running}");
    let asked: Vec<String> = host
        .qemu_log()
        .into_iter()
        .filter(|l| l.starts_with("qmp set_link") || l.starts_with("qmp block_resize"))
        .collect();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert!(asked[0].contains("\"up\": false") && asked[1].contains(&(30u64 << 30).to_string()), "{asked:?}");
    assert!(config().contains("memory_mb = 8192") && config().contains("network = false"));
    cli.ok(&["vm", "force-off", vm]);
    cli.wait_for_status(vm, "off");
}

#[test]
fn the_systems_roomy_can_download_are_listed_and_unreachable_ones_explained() {
    let host = FakeHost::healthy("cliimage");
    let cli = Cli { host: &host };
    let listed = cli.ok(&["image", "list"]);
    assert!(listed.starts_with("Good first choices:\n  ubuntu        Ubuntu: "), "{listed}");
    assert!(listed.contains("\n  cachyos       CachyOS: "), "{listed}");

    assert_eq!(
        cli.err(&["image", "get", "beos"]),
        "roomy: roomy does not know a system called 'beos' (see: roomy image list)"
    );
    assert_eq!(
        cli.err(&["image", "get", "fedora", "vista"]),
        "roomy: Fedora has no edition called 'vista' (see: roomy image list fedora)"
    );
    assert_eq!(
        cli.err(&["image", "get", "zorin"]),
        "roomy: roomy cannot download Zorin OS Core itself; get it from https://zorin.com/os/download/"
    );
    // The stand-in curl answers every address as missing: the project's server is named.
    let offline = cli.err(&["image", "get", "archlinux"]);
    assert!(
        offline.starts_with("roomy: cannot read https://geo.mirror.pkgbuild.com/iso/latest/sha256sums.txt: "),
        "{offline}"
    );
    assert!(!host.base.join("home/Downloads").exists(), "nothing is saved when nothing was found");

    assert_eq!(cli.run(&["image"]).status.code(), Some(2));
}
