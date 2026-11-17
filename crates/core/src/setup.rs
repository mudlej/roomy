//! Installing what roomy needs from the distribution's packages.

use crate::MIN_QEMU;
use crate::env::{Env, roomy_command, shell_word};
use crate::host::{OsRelease, Version};
use crate::process::run_capture;
use std::fs::File;
use std::io::Write as _;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// What must be present on any distribution, in plain words. Shown when
/// roomy cannot install packages itself. The versions are [`MIN_QEMU`] and
/// [`crate::shares::MIN_VIRTIOFSD`].
pub const REQUIRED_COMPONENTS: &[&str] = &[
    "QEMU 10.1 or newer for x86-64 (qemu-system-x86_64) with KVM support",
    "QEMU modules: D-Bus display, D-Bus audio, virtio-gpu and virtio-vga including their OpenGL variants",
    "qemu-img (disk image tool)",
    "virtiofsd 1.13 or newer (shared folders)",
    "UEFI firmware for virtual machines (OVMF / edk2) with its QEMU firmware descriptor",
    "GTK 4.14 or newer and libadwaita 1.5 or newer (for roomy's window)",
    "The PulseAudio client library, libpulse (for sound; PipeWire systems provide it too)",
];

/// One command of an installation, to be run as root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The package manager and its arguments. It may ask its own questions.
    pub command: Vec<String>,
    /// The same for when the user has already said yes: nothing may stop to ask. A separate
    /// command line because package managers disagree on where such a switch may stand.
    pub unattended: Vec<String>,
}

impl Step {
    fn new(start: &[&str], unattended_start: &[&str], rest: &[&str]) -> Step {
        let line = |start: &[&str]| start.iter().chain(rest).map(|word| (*word).to_owned()).collect();
        Step { command: line(start), unattended: line(unattended_start) }
    }
}

/// A package installation `roomy setup` can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// Run in order; a step that fails ends the installation.
    pub steps: Vec<Step>,
    /// Things worth knowing before running it on this distribution.
    pub notes: Vec<String>,
    /// What usually is the matter when the package manager refuses, and what to do then.
    pub if_it_fails: Option<String>,
}

impl InstallPlan {
    fn new(steps: Vec<Step>) -> InstallPlan {
        InstallPlan { steps, notes: Vec::new(), if_it_fails: None }
    }

    fn note(mut self, note: impl Into<String>) -> InstallPlan {
        self.notes.push(note.into());
        self
    }

    /// The commands as a user would type them, one per line.
    pub fn command_lines(&self) -> Vec<String> {
        self.steps.iter().map(|step| step.command.join(" ")).collect()
    }
}

/// What `roomy setup` can do about missing packages on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advice {
    /// Run this.
    Install(InstallPlan),
    /// roomy knows the system, but there is no installation it should run by itself there:
    /// the system takes in packages by being rebuilt and restarted, or it has no QEMU new enough.
    /// What to do instead.
    ByHand(String),
    /// A distribution roomy has no package list for.
    Unknown,
}

const ARCH: &[&str] = &["qemu-desktop", "qemu-img", "virtiofsd", "edk2-ovmf", "gtk4", "libadwaita", "libpulse"];

/// Fedora splits QEMU into one package per module. Two of these are needed by others in the list
/// without being declared as such there (`qemu-ui-opengl` by the D-Bus display,
/// `qemu-device-display-virtio-gpu-gl` by the accelerated graphics card), so both are named.
const FEDORA: &[&str] = &[
    "qemu-system-x86-core",
    "qemu-img",
    "qemu-ui-dbus",
    "qemu-ui-opengl",
    "qemu-audio-dbus",
    "qemu-device-display-virtio-vga",
    "qemu-device-display-virtio-vga-gl",
    "qemu-device-display-virtio-gpu-gl",
    "virtiofsd",
    "edk2-ovmf",
    "gtk4",
    "libadwaita",
    "pulseaudio-libs",
];

/// openSUSE: nothing pulls in the D-Bus display and audio modules, and the display module needs
/// `qemu-ui-opengl` without saying so.
const SUSE: &[&str] = &[
    "qemu-x86",
    "qemu-ui-dbus",
    "qemu-ui-opengl",
    "qemu-audio-dbus",
    "qemu-hw-display-virtio-vga",
    "qemu-hw-display-virtio-gpu",
    "qemu-img",
    "virtiofsd",
    "qemu-ovmf-x86_64",
    "libgtk-4-1",
    "libadwaita-1-0",
    "libpulse0",
];

/// Debian and Ubuntu, the QEMU part. `qemu-system-modules-opengl` is where the D-Bus display,
/// D-Bus audio and the accelerated graphics card live. Ubuntu also offers these four in a
/// second, newer edition whose names end in `-hwe`; the two editions exclude each other.
const APT_QEMU: &[&str] = &["qemu-system-x86", "qemu-system-modules-opengl", "qemu-utils", "ovmf"];
const APT_QEMU_FROM_BACKPORTS: &[&str] = &["qemu-system-x86", "qemu-system-modules-opengl", "qemu-utils"];

/// Debian and Ubuntu, everything else. Recommended packages are left out to keep QEMU small, so
/// what the window needs beyond its libraries is named: EGL for accelerated graphics, and the
/// icons, their SVG loader and the desktop settings a GTK program expects to find.
const APT_OTHER: &[&str] = &[
    "virtiofsd",
    "libgtk-4-1",
    "libadwaita-1-0",
    "libpulse0",
    "libegl1",
    "adwaita-icon-theme",
    "librsvg2-common",
    "gsettings-desktop-schemas",
];

/// Never remove anything to make room (`--no-remove`), leave installed packages at their version
/// (`--no-upgrade`), and wait for the software centre or automatic updates to let go of the
/// package database rather than fail at once.
const APT_INSTALL: &[&str] =
    &["apt-get", "install", "--no-install-recommends", "--no-remove", "-o", "DPkg::Lock::Timeout=120"];
const APT_INSTALL_UNATTENDED: &[&str] = &[
    "env",
    "DEBIAN_FRONTEND=noninteractive",
    "apt-get",
    "install",
    "-y",
    "--no-install-recommends",
    "--no-remove",
    "-o",
    "DPkg::Lock::Timeout=120",
];

/// Where to find a QEMU new enough, for a Debian or Ubuntu release that has none.
const NEW_ENOUGH_RELEASES: &str = "Ubuntu has a new enough QEMU from 26.04 on, Debian from 13 with backports.";

/// Runs a program of this computer and gives back what it printed, or `None` if that failed.
/// Only programs that change nothing are run through it.
pub type Ask<'a> = &'a dyn Fn(&str, &[&str]) -> Option<String>;

/// What to do about missing packages on the distribution described by `os`.
///
/// The package lists come from each distribution's own package definitions; `roomy doctor`
/// afterwards is what says whether an installation worked.
pub fn advice(env: &Env, os: &OsRelease, ask: Ask) -> Advice {
    if os.is_like("arch") {
        // Always together with a full update (-Syu). A rolling distribution's package sources only
        // hold the newest packages, built against each other: plain -S asks for whatever this
        // system last heard of, which the sources have usually dropped by then (the download fails),
        // and refreshing without updating (-Sy) puts new packages on top of libraries too old for them.
        let mut plan = InstallPlan::new(vec![Step::new(
            &["pacman", "-Syu", "--needed"],
            &["pacman", "-Syu", "--needed", "--noconfirm"],
            ARCH,
        )])
        .note(
            "On an Arch-based system, packages can only be installed reliably together with a full update, so this \
             also updates everything else that is installed. If that brings a new kernel, restart the computer \
             afterwards.",
        );
        plan.if_it_fails = Some(format!(
            "If pacman stopped at a question, it needs an answer that cannot be given from here. Run it in a \
             terminal to answer it:\n  sudo {}",
            plan.command_lines().join(" ")
        ));
        return Advice::Install(plan);
    }
    // Red Hat Enterprise Linux and its rebuilds call themselves "like fedora", but package QEMU
    // quite differently (other names, no `qemu-system-x86_64` on the PATH, no accelerated graphics).
    if os.is_like("fedora") && !os.is_like("rhel") {
        // Atomic editions (Silverblue, Kinoite and the images built on them) keep the system
        // read-only: packages are added to the next system image, which needs a restart.
        if env.path("/run/ostree-booted").exists() {
            return Advice::ByHand(format!(
                "This is an image-based system: packages are added to the system image and become available after \
                 a restart.\n  sudo rpm-ostree install --idempotent {}\nThen restart the computer and run '{} \
                 doctor'. Alternatively, use roomy inside a toolbox or distrobox container, where it can install \
                 what it needs itself.",
                FEDORA.join(" "),
                roomy_command()
            ));
        }
        return Advice::Install(
            InstallPlan::new(vec![Step::new(&["dnf", "install"], &["dnf", "install", "-y"], FEDORA)]).note(
                "This also updates QEMU packages that are already installed but older; shut down running virtual \
                 machines first.",
            ),
        );
    }
    if os.is_like("suse") {
        let transactional =
            os.variant_id == "transactional" || ["opensuse-microos", "microos"].iter().any(|family| os.is_like(family));
        if transactional {
            return Advice::ByHand(format!(
                "This is a transactional system: packages are added to a new snapshot and become available after a \
                 restart.\n  sudo transactional-update pkg install {}\nThen restart the computer and run '{} \
                 doctor'. Alternatively, use roomy inside a distrobox container, where it can install what it needs \
                 itself.",
                SUSE.join(" "),
                roomy_command()
            ));
        }
        // zypper stops reading switches at the first package name, so "do not ask" has to come first.
        return Advice::Install(
            InstallPlan::new(vec![Step::new(
                &["zypper", "install"],
                &["zypper", "--non-interactive", "install"],
                SUSE,
            )])
            .note(
                "Bring the system up to date first (on Tumbleweed: sudo zypper dup); installing onto an \
                     outdated system updates only these packages.",
            )
            .note(
                "If zypper says the system management is locked, the desktop's update service is busy: wait a \
                     minute and try again.",
            ),
        );
    }
    if os.is_like("debian") || os.is_like("ubuntu") {
        return apt_advice(env, os, ask);
    }
    Advice::Unknown
}

/// Debian, Ubuntu and what is built on them. The package names are the same on all of them, but
/// many releases still in use carry a QEMU older than roomy needs, so the package manager is
/// asked which version it would install before anything is proposed.
fn apt_advice(env: &Env, os: &OsRelease, ask: Ask) -> Advice {
    let update = Step::new(&["apt-get", "update"], &["apt-get", "update"], &[]);
    // Ubuntu's newer edition of the virtualization packages, where the user has opted into it:
    // asking for the ordinary names there would mean removing it.
    let newer_edition = env.path("/var/lib/dpkg/info/ubuntu-virt-hwe.list").exists();
    let qemu: Vec<String> =
        APT_QEMU.iter().map(|name| if newer_edition { format!("{name}-hwe") } else { (*name).to_owned() }).collect();
    let all: Vec<&str> = qemu.iter().map(String::as_str).chain(APT_OTHER.iter().copied()).collect();
    let ordinary = InstallPlan::new(vec![update.clone(), apt_install(&all, &["--no-upgrade"])]);

    let policy = ask("apt-cache", &["policy", if newer_edition { "qemu-system-x86-hwe" } else { "qemu-system-x86" }]);
    let Some(offered) = policy.as_deref().and_then(apt_candidate) else {
        // The package manager knows nothing yet (its lists were never fetched, say). For the
        // releases known to be too old, say so; otherwise install and let the doctor judge.
        let release = Version::find_in(&format!("{}.0", os.version_id));
        let oldest = match os.id.as_str() {
            "debian" => Some(Version { major: 13, minor: 0, micro: 0 }),
            "ubuntu" => Some(Version { major: 26, minor: 4, micro: 0 }),
            _ => None,
        };
        return match (release, oldest) {
            (Some(release), Some(oldest)) if release < oldest => Advice::ByHand(format!(
                "{} has a QEMU older than roomy needs ({MIN_QEMU} or newer), so installing it would not help.\n\
                 {NEW_ENOUGH_RELEASES}",
                os.pretty_name
            )),
            _ => Advice::Install(ordinary),
        };
    };
    if offered >= MIN_QEMU {
        return Advice::Install(ordinary);
    }
    if let Some(suite) = policy.as_deref().and_then(apt_newer_suite) {
        // Only QEMU comes from the backports; `-t` lets the parts of QEMU that must match come along.
        let other: Vec<&str> = std::iter::once("ovmf").chain(APT_OTHER.iter().copied()).collect();
        return Advice::Install(
            InstallPlan::new(vec![
                update,
                apt_install(APT_QEMU_FROM_BACKPORTS, &["-t", &suite]),
                apt_install(&other, &["--no-upgrade"]),
            ])
            .note(format!(
                "{} itself has QEMU {offered}; roomy needs {MIN_QEMU} or newer and takes it from {suite}. This \
                 replaces QEMU for every program that uses it (GNOME Boxes, virt-manager); shut down running \
                 virtual machines first.",
                os.pretty_name
            )),
        );
    }
    let mut text = format!(
        "{} offers QEMU {offered}; roomy needs {MIN_QEMU} or newer, so installing it would not help.",
        os.pretty_name
    );
    if os.id == "debian" && !os.version_codename.is_empty() {
        text.push_str(&format!(
            "\nA newer QEMU is published in {0}-backports. Enable that source (https://backports.debian.org/Instructions/), \
             run 'sudo apt-get update', then run '{1} setup' again.",
            os.version_codename,
            roomy_command()
        ));
    } else {
        text.push('\n');
        text.push_str(NEW_ENOUGH_RELEASES);
    }
    Advice::ByHand(text)
}

/// An `apt-get install` of `packages`, with `more` switches.
fn apt_install(packages: &[&str], more: &[&str]) -> Step {
    let rest: Vec<&str> = more.iter().chain(packages).copied().collect();
    Step::new(APT_INSTALL, APT_INSTALL_UNATTENDED, &rest)
}

/// The version `apt-cache policy` says would be installed.
fn apt_candidate(policy: &str) -> Option<Version> {
    let line = policy.lines().find_map(|line| line.trim().strip_prefix("Candidate:"))?;
    Version::find_in(line.rsplit(':').next()?)
}

/// A backports source in `apt-cache policy` output that holds a QEMU new enough.
fn apt_newer_suite(policy: &str) -> Option<String> {
    let mut version = None;
    for line in policy.lines().map(str::trim) {
        // The version table lists a version, then the sources that have it.
        let mut words = line.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (Some(priority), Some(_url), Some(suite)) if priority.parse::<u32>().is_ok() => {
                let suite = suite.split('/').next().unwrap_or(suite);
                if suite.ends_with("-backports") && version.is_some_and(|version| version >= MIN_QEMU) {
                    return Some(suite.to_owned());
                }
            }
            (Some(first), _, _) => {
                let first = if first == "***" { line.split_whitespace().nth(1).unwrap_or("") } else { first };
                if let Some(found) = Version::find_in(first.rsplit(':').next().unwrap_or(first)) {
                    version = Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// Asks the host something by running a program that changes nothing. No answer when the
/// program is absent, fails or does not finish.
pub fn query(env: &Env, program: &str, args: &[&str]) -> Option<String> {
    let answer = run_capture(&env.find_program(program)?, args, Duration::from_secs(30)).ok()?;
    answer.status.success().then_some(answer.stdout)
}

/// Who is at hand when an installation has to become administrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// The user has already said yes (`--yes`, or a button in the window): nothing may stop to ask.
    pub unattended: bool,
    /// The user is at a terminal, where sudo can ask for a password.
    pub terminal: bool,
    /// Already administrator.
    pub root: bool,
}

impl Session {
    /// For this process. Whether the user is at a terminal is the caller's to say: a window
    /// started from a terminal has one, and nobody is looking at it.
    pub fn of_this_process(unattended: bool, terminal: bool) -> Session {
        // SAFETY: geteuid cannot fail.
        Session { unattended, terminal, root: unsafe { libc::geteuid() } == 0 }
    }
}

/// The command lines that carry out `plan` as administrator, to be run in order until one
/// fails. `None` when there is no way to become administrator from this session.
pub fn elevated(plan: &InstallPlan, session: Session, env: &Env, ask: Ask) -> Option<Vec<Vec<String>>> {
    let steps = || plan.steps.iter().map(|step| if session.unattended { &step.unattended } else { &step.command });
    let behind = |program: &Path, step: &Vec<String>| {
        std::iter::once(program.display().to_string()).chain(step.iter().cloned()).collect::<Vec<String>>()
    };
    if session.root {
        return Some(steps().cloned().collect());
    }
    // sudo asks for the password on the terminal. Without one it is only of use where it asks nothing.
    let sudo = env.find_program("sudo").filter(|_| session.terminal || ask("sudo", &["-n", "true"]).is_some());
    if let Some(sudo) = sudo {
        return Some(steps().map(|step| behind(&sudo, step)).collect());
    }
    // pkexec asks through the desktop's own password dialog, once for every command it is given:
    // several steps go to it as one.
    let pkexec = env.find_program("pkexec")?;
    let steps: Vec<&Vec<String>> = steps().collect();
    Some(match steps.as_slice() {
        [only] => vec![behind(&pkexec, only)],
        several => {
            let line = |step: &&Vec<String>| step.iter().map(|word| shell_word(word)).collect::<Vec<_>>().join(" ");
            let script = several.iter().map(line).collect::<Vec<_>>().join(" && ");
            vec![behind(&pkexec, &vec!["/bin/sh".to_owned(), "-c".to_owned(), script])]
        }
    })
}

/// Runs `commands` (from [`elevated`]) in order, with everything they print added to the file
/// `log`, and stops at the first that fails. The error says what happened in the user's words.
///
/// For a caller without a terminal. The commands take no input, and they are left alone if the
/// caller goes away or its terminal is interrupted: a package manager stopped half-way leaves a
/// system in a worse state than one that finishes unwatched.
pub fn run_logged(commands: &[Vec<String>], log: &Path) -> Result<(), String> {
    for command in commands {
        let Some((program, args)) = command.split_first() else { continue };
        let cannot_log = |e: std::io::Error| format!("Cannot write to {}: {e}", log.display());
        let mut file = File::options().create(true).append(true).open(log).map_err(cannot_log)?;
        writeln!(file, "$ {}", command.join(" ")).map_err(cannot_log)?;
        let output = file.try_clone().map_err(cannot_log)?;
        let status = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(output)
            .stderr(file)
            .process_group(0)
            .status()
            .map_err(|e| format!("Could not run {program}: {e}"))?;
        if status.success() {
            continue;
        }
        let through_pkexec = Path::new(program).file_name().is_some_and(|name| name == "pkexec");
        return Err(match (through_pkexec, status.code()) {
            // pkexec's own two ways of not running the command at all.
            (true, Some(126)) => "Not authorised: the password dialog was dismissed. Nothing was changed.".to_owned(),
            (true, Some(127)) => format!(
                "The administrator's password could not be asked for here; nothing was changed. Run this in a \
                 terminal instead: {} setup",
                roomy_command()
            ),
            (_, Some(code)) => format!("The package manager did not finish successfully (exit status {code})."),
            (_, None) => {
                format!("The package manager was stopped by signal {}.", status.signal().unwrap_or_default())
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    const NOBODY: Ask<'static> = &|_, _| None;

    /// A host on which the named files exist, empty.
    fn host(files: &[&str]) -> (Env, TempDir) {
        let root = TempDir::new("setup");
        for file in files {
            let path = root.join(file.trim_start_matches('/'));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        (Env::rooted(&*root, Vec::new(), root.join("run")), root)
    }

    fn advice_for(os_release: &str, files: &[&str], ask: Ask) -> Advice {
        let (env, _root) = host(files);
        advice(&env, &OsRelease::parse(os_release), ask)
    }

    /// A host on whose search path the named programs exist.
    fn host_with_programs(names: &[&str]) -> (Env, TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let (_, root) = host(&[]);
        for name in names {
            std::fs::write(root.join(name), "").unwrap();
            std::fs::set_permissions(root.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (Env::rooted(&*root, vec![root.to_path_buf()], root.join("run")), root)
    }

    #[test]
    fn becoming_administrator_depends_on_who_can_be_asked() {
        let update = Step::new(&["apt-get", "update"], &["apt-get", "update"], &[]);
        let add = Step::new(&["apt-get", "install"], &["apt-get", "install", "-y"], &["it's"]);
        let plan = InstallPlan::new(vec![update.clone(), add]);
        let lines = |commands: Option<Vec<Vec<String>>>| -> Option<Vec<String>> {
            commands.map(|commands| commands.iter().map(|command| command.join(" ")).collect())
        };
        let session = |unattended, terminal, root| Session { unattended, terminal, root };
        let passwordless: Ask = &|program, args| (program == "sudo" && args == ["-n", "true"]).then(String::new);

        let (both, root) = host_with_programs(&["sudo", "pkexec"]);
        let (sudo, pkexec) = (root.join("sudo").display().to_string(), root.join("pkexec").display().to_string());
        // Already administrator: as they are.
        assert_eq!(
            lines(elevated(&plan, session(true, false, true), &both, NOBODY)).unwrap(),
            ["apt-get update", "apt-get install -y it's"]
        );
        // At a terminal, sudo asks there, and the package manager may ask its own questions.
        assert_eq!(
            lines(elevated(&plan, session(false, true, false), &both, NOBODY)).unwrap(),
            [format!("{sudo} apt-get update"), format!("{sudo} apt-get install it's")]
        );
        // From the window sudo could not ask: the desktop's password dialog does, once for everything.
        assert_eq!(
            lines(elevated(&plan, session(true, false, false), &both, NOBODY)).unwrap(),
            [format!(r"{pkexec} /bin/sh -c apt-get update && apt-get install -y 'it'\''s'")]
        );
        // A single command is handed over as itself, so that the dialog can name it.
        assert_eq!(
            lines(elevated(&InstallPlan::new(vec![update]), session(true, false, false), &both, NOBODY)).unwrap(),
            [format!("{pkexec} apt-get update")]
        );
        // Unless sudo needs no password here.
        assert_eq!(
            lines(elevated(&plan, session(true, false, false), &both, passwordless)).unwrap(),
            [format!("{sudo} apt-get update"), format!("{sudo} apt-get install -y it's")]
        );

        let (only_sudo, _root) = host_with_programs(&["sudo"]);
        assert_eq!(elevated(&plan, session(true, false, false), &only_sudo, NOBODY), None);
    }

    #[test]
    fn an_installation_stops_at_the_first_failure_and_keeps_what_was_printed() {
        let root = TempDir::new("setup");
        let log = root.join("setup.log");
        let shell = |script: &str| vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()];

        assert_eq!(run_logged(&[shell("echo one"), shell("echo two >&2")], &log), Ok(()));
        let failure = run_logged(&[shell("echo three; exit 3"), shell("echo never")], &log).unwrap_err();
        assert_eq!(failure, "The package manager did not finish successfully (exit status 3).");
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "$ /bin/sh -c echo one\none\n$ /bin/sh -c echo two >&2\ntwo\n$ /bin/sh -c echo three; exit 3\nthree\n"
        );
    }

    fn install(advice: Advice) -> InstallPlan {
        match advice {
            Advice::Install(plan) => plan,
            other => panic!("expected an installation, got {other:?}"),
        }
    }

    fn by_hand(advice: Advice) -> String {
        match advice {
            Advice::ByHand(text) => text,
            other => panic!("expected instructions, got {other:?}"),
        }
    }

    #[test]
    fn arch_and_its_derivatives_use_pacman() {
        let plan = install(advice_for("ID=cachyos\nID_LIKE=arch\n", &[], NOBODY));
        assert_eq!(
            plan.command_lines(),
            ["pacman -Syu --needed qemu-desktop qemu-img virtiofsd edk2-ovmf gtk4 libadwaita libpulse"]
        );
        let step = &plan.steps[0];
        assert_eq!(step.unattended[..4], ["pacman", "-Syu", "--needed", "--noconfirm"]);
        assert_eq!(step.unattended[4..], step.command[3..]);
        // A full update is more than was asked for: said before anything runs.
        assert!(plan.notes[0].contains("also updates everything else"), "{:?}", plan.notes);
    }

    #[test]
    fn fedora_uses_dnf_but_its_image_based_editions_and_red_hat_do_not() {
        let plan = install(advice_for("ID=fedora\nVERSION_ID=45\n", &[], NOBODY));
        assert!(
            plan.command_lines()[0]
                .starts_with("dnf install qemu-system-x86-core qemu-img qemu-ui-dbus qemu-ui-opengl")
        );
        assert_eq!(plan.steps[0].unattended[..3], ["dnf", "install", "-y"]);
        // A remix that uses Fedora's packages.
        install(advice_for("ID=nobara\nID_LIKE=fedora\n", &[], NOBODY));

        let text = by_hand(advice_for("ID=fedora\nVARIANT_ID=silverblue\n", &["/run/ostree-booted"], NOBODY));
        assert!(text.contains("rpm-ostree install --idempotent qemu-system-x86-core"), "{text}");
        assert_eq!(advice_for("ID=\"rhel\"\nID_LIKE=\"fedora\"\n", &[], NOBODY), Advice::Unknown);
        assert_eq!(advice_for("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\n", &[], NOBODY), Advice::Unknown);
    }

    #[test]
    fn opensuse_uses_zypper_with_the_do_not_ask_switch_ahead_of_the_packages() {
        let plan = install(advice_for("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n", &[], NOBODY));
        assert!(plan.command_lines()[0].starts_with("zypper install qemu-x86 qemu-ui-dbus"));
        let unattended = &plan.steps[0].unattended;
        assert_eq!(unattended[..3], ["zypper", "--non-interactive", "install"]);
        // Everything after the first package name is taken for a package by zypper.
        assert!(unattended[3..].iter().all(|word| !word.starts_with('-')), "{unattended:?}");
        assert_eq!(plan.notes.len(), 2);

        for os_release in [
            "ID=\"opensuse-microos\"\nID_LIKE=\"suse opensuse opensuse-tumbleweed microos sl-micro\"\n",
            "ID=\"aeon\"\nID_LIKE=\"suse opensuse opensuse-tumbleweed opensuse-microos opensuse-aeon microos\"\n",
            "ID=\"opensuse-leap\"\nID_LIKE=\"suse opensuse\"\nVARIANT_ID=\"transactional\"\n",
        ] {
            let text = by_hand(advice_for(os_release, &[], NOBODY));
            assert!(text.contains("transactional-update pkg install qemu-x86"), "{os_release:?}: {text}");
        }
    }

    const UBUNTU_26_04: &str = "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"26.04\"\nVERSION_CODENAME=resolute\nPRETTY_NAME=\"Ubuntu 26.04 LTS\"\n";
    const DEBIAN_13: &str =
        "ID=debian\nVERSION_ID=\"13\"\nVERSION_CODENAME=trixie\nPRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\n";

    fn policy(text: &'static str) -> impl Fn(&str, &[&str]) -> Option<String> {
        move |program, args| {
            assert_eq!((program, args.first()), ("apt-cache", Some(&"policy")));
            Some(text.to_owned())
        }
    }

    #[test]
    fn ubuntu_with_a_new_enough_qemu_updates_the_lists_and_installs_without_removing_anything() {
        let ask = policy(
            "qemu-system-x86:\n  Installed: (none)\n  Candidate: 1:10.2.1+ds-1ubuntu4.4\n  Version table:\n     \
             1:10.2.1+ds-1ubuntu4.4 500\n        500 http://archive.ubuntu.com/ubuntu resolute-updates/main amd64 Packages\n",
        );
        let plan = install(advice_for(UBUNTU_26_04, &[], &ask));
        let lines = plan.command_lines();
        assert_eq!(lines[0], "apt-get update");
        assert_eq!(
            lines[1],
            "apt-get install --no-install-recommends --no-remove -o DPkg::Lock::Timeout=120 --no-upgrade qemu-system-x86 \
             qemu-system-modules-opengl qemu-utils ovmf virtiofsd libgtk-4-1 libadwaita-1-0 libpulse0 libegl1 \
             adwaita-icon-theme librsvg2-common gsettings-desktop-schemas"
        );
        let unattended = plan.steps[1].unattended.join(" ");
        assert!(unattended.starts_with("env DEBIAN_FRONTEND=noninteractive apt-get install -y "), "{unattended}");
        assert!(unattended.contains("--no-remove"), "{unattended}");
        assert!(plan.notes.is_empty());

        // Where the user has opted into Ubuntu's newer edition of these packages, that edition is kept.
        let plan = install(advice_for(UBUNTU_26_04, &["/var/lib/dpkg/info/ubuntu-virt-hwe.list"], &ask));
        let install = &plan.command_lines()[1];
        assert!(
            install.contains(" qemu-system-x86-hwe qemu-system-modules-opengl-hwe qemu-utils-hwe ovmf-hwe virtiofsd "),
            "{install}"
        );
    }

    #[test]
    fn a_release_whose_qemu_is_too_old_is_not_installed_onto() {
        let ask = policy(
            "qemu-system-x86:\n  Installed: (none)\n  Candidate: 1:8.2.2+ds-0ubuntu1.10\n  Version table:\n     \
             1:8.2.2+ds-0ubuntu1.10 500\n        500 http://archive.ubuntu.com/ubuntu noble-updates/main amd64 Packages\n",
        );
        // Linux Mint 22 is built on Ubuntu 24.04: its own version number says nothing, the offered QEMU does.
        let text = by_hand(advice_for(
            "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_ID=\"22\"\nPRETTY_NAME=\"Linux Mint 22\"\n",
            &[],
            &ask,
        ));
        assert!(text.starts_with("Linux Mint 22 offers QEMU 8.2.2; roomy needs 10.1.0 or newer"), "{text}");

        let ask = policy(
            "qemu-system-x86:\n  Installed: (none)\n  Candidate: 1:10.0.13+ds-0+deb13u1\n  Version table:\n     \
             1:10.0.13+ds-0+deb13u1 500\n        500 http://deb.debian.org/debian trixie/main amd64 Packages\n",
        );
        let text = by_hand(advice_for(DEBIAN_13, &[], &ask));
        assert!(text.contains("published in trixie-backports"), "{text}");
    }

    #[test]
    fn debian_takes_qemu_alone_from_backports_once_they_are_enabled() {
        let ask = policy(
            "qemu-system-x86:\n  Installed: 1:10.0.13+ds-0+deb13u1\n  Candidate: 1:10.0.13+ds-0+deb13u1\n  Version table:\n     \
             1:11.1.2+ds-1~bpo13+1 100\n        100 http://deb.debian.org/debian trixie-backports/main amd64 Packages\n \
             *** 1:10.0.13+ds-0+deb13u1 500\n        500 http://deb.debian.org/debian trixie/main amd64 Packages\n        \
             100 /var/lib/dpkg/status\n",
        );
        let plan = install(advice_for(DEBIAN_13, &[], &ask));
        let lines = plan.command_lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "apt-get update");
        assert!(
            lines[1].ends_with("-t trixie-backports qemu-system-x86 qemu-system-modules-opengl qemu-utils"),
            "{}",
            lines[1]
        );
        assert!(!lines[1].contains("--no-upgrade"), "QEMU is meant to be upgraded here: {}", lines[1]);
        assert!(lines[2].contains("--no-upgrade ovmf virtiofsd libgtk-4-1"), "{}", lines[2]);
        assert!(plan.notes[0].contains("takes it from trixie-backports"), "{:?}", plan.notes);
    }

    #[test]
    fn without_an_answer_from_the_package_manager_the_ordinary_installation_is_offered() {
        let plan = install(advice_for(DEBIAN_13, &[], NOBODY));
        assert_eq!(plan.steps.len(), 2);
        // Unless the release is one known to be too old whatever its lists say.
        let text = by_hand(advice_for(
            "ID=debian\nVERSION_ID=\"12\"\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n",
            &[],
            NOBODY,
        ));
        assert_eq!(
            text,
            "Debian GNU/Linux 12 (bookworm) has a QEMU older than roomy needs (10.1.0 or newer), so installing it \
             would not help.\nUbuntu has a new enough QEMU from 26.04 on, Debian from 13 with backports."
        );
        by_hand(advice_for("ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n", &[], NOBODY));
        install(advice_for("ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"26.10\"\n", &[], NOBODY));
        install(advice_for("ID=debian\nVERSION_CODENAME=forky\n", &[], NOBODY));
        let none_yet = policy("qemu-system-x86:\n  Installed: (none)\n  Candidate: (none)\n  Version table:\n");
        assert_eq!(install(advice_for(DEBIAN_13, &[], &none_yet)).steps.len(), 2);
    }

    #[test]
    fn the_plain_list_of_requirements_names_the_versions_roomy_checks_for() {
        let short = |version: Version| format!("{}.{} or newer", version.major, version.minor);
        let names = |program: &str| REQUIRED_COMPONENTS.iter().find(|line| line.starts_with(program)).unwrap();
        assert!(names("QEMU ").contains(&short(MIN_QEMU)), "{}", names("QEMU "));
        assert!(names("virtiofsd").contains(&short(crate::shares::MIN_VIRTIOFSD)), "{}", names("virtiofsd"));
    }

    #[test]
    fn an_unfamiliar_distribution_gets_no_guess() {
        assert_eq!(advice_for("ID=void\n", &[], NOBODY), Advice::Unknown);
        assert_eq!(advice_for("", &[], NOBODY), Advice::Unknown);
    }
}
