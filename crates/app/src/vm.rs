//! `roomy vm ...`: virtual machines from the command line.

use crate::{EXIT_PROBLEMS, EXIT_USAGE, usage_error};
use roomy_core::control::{self, Request, State};
use roomy_core::env::{Env, roomy_command};
use roomy_core::settings::{self, Applies, Changes};
use roomy_core::vm::{self, Features, Folder, NewVm, PortForward, SharedFolder};
use roomy_core::{Error, Result, err, library, supervisor};
use std::fs::File;
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

pub fn run(args: &[String]) -> ExitCode {
    let Some((name, rest)) = args.split_first() else {
        return usage_error(&format!(
            "'roomy vm' needs an action, for example: {} vm status <vm-folder>",
            roomy_command()
        ));
    };
    match name.as_str() {
        "create" => return report(create(rest)),
        "list" if rest.is_empty() => return report(list()),
        "list" => return usage_error("'roomy vm list' takes no arguments"),
        "delete" => return report(delete(rest)),
        "set" => return report(set(rest)),
        _ => {}
    }
    let Some(action) = Action::named(name) else {
        return usage_error(&format!("unknown action '{name}' for 'roomy vm'"));
    };
    let [folder] = rest else {
        return usage_error(&format!("'roomy vm {name}' takes exactly one argument: the VM's folder"));
    };
    report(Folder::open(Path::new(folder)).and_then(|folder| act(&Env::system(), &folder, action)))
}

/// What `roomy vm <action> <vm-folder>` does.
enum Action {
    Status,
    Start,
    DiscardState,
    Add,
    Forget,
    /// Something only the VM's supervisor can do.
    Ask(Request),
}

impl Action {
    fn named(name: &str) -> Option<Action> {
        Some(match name {
            "status" => Action::Status,
            "start" => Action::Start,
            "discard-state" => Action::DiscardState,
            "add" => Action::Add,
            "forget" => Action::Forget,
            "pause" => Action::Ask(Request::Pause),
            "resume" => Action::Ask(Request::Resume),
            "suspend" => Action::Ask(Request::Suspend),
            "shutdown" => Action::Ask(Request::Shutdown),
            "restart" => Action::Ask(Request::Restart),
            "force-off" => Action::Ask(Request::ForceOff),
            "force-restart" => Action::Ask(Request::ForceRestart),
            _ => return None,
        })
    }
}

/// Carries out one action and says how the VM is now.
fn act(env: &Env, folder: &Folder, action: Action) -> Result<String> {
    let line = match action {
        Action::Status => control::status(env, folder).word(),
        Action::Start => {
            let was_suspended = folder.is_suspended();
            let exe = roomy_core::env::own_program().map_err(|e| err!("cannot locate the roomy program: {e}"))?;
            control::start(env, folder, &exe)?;
            if was_suspended { "running (continued from saved state)" } else { "running" }
        }
        Action::DiscardState => {
            if control::status(env, folder) != State::Suspended {
                return Err(err!("the virtual machine is not suspended; there is no saved state to discard"));
            }
            folder.discard_saved_state()?;
            "off (saved state discarded)"
        }
        Action::Add => {
            let added = library::add(&library::default_path()?, folder.dir())?;
            if added { "added to the library" } else { "already in the library" }
        }
        Action::Forget => {
            let listed = library::forget(&library::default_path()?, folder.dir())?;
            if listed { "removed from the library; its files were not touched" } else { "not in the library" }
        }
        Action::Ask(request) => {
            let reply = control::request(env, folder, request)?;
            return Ok(match reply.note {
                Some(note) => format!("{}\nNote: {note}", reply.state.word()),
                None => reply.state.word().to_owned(),
            });
        }
    };
    Ok(line.to_owned())
}

fn create(args: &[String]) -> Result<String> {
    let mut new = NewVm {
        name: String::new(),
        os: "linux".to_owned(),
        cpus: 2,
        memory_mb: 4096,
        disk_gb: 64,
        install_image: None,
        existing_disk: None,
        features: Features::default(),
        shared_folders: Vec::new(),
        port_forwards: Vec::new(),
    };
    let mut parent: Option<PathBuf> = None;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let mut value = || args.next().cloned().ok_or_else(|| err!("{flag} needs a value"));
        match flag.as_str() {
            "--name" => new.name = value()?,
            "--in" => parent = Some(PathBuf::from(value()?)),
            "--os" => new.os = value()?,
            "--cpus" => new.cpus = number(flag, &value()?)?,
            "--memory" => new.memory_mb = number(flag, &value()?)?,
            "--disk" => new.disk_gb = number(flag, &value()?)?,
            "--iso" => new.install_image = Some(absolute(&value()?)?),
            "--from-disk" => new.existing_disk = Some(absolute(&value()?)?),
            "--gpu" => new.features.gpu_acceleration = true,
            "--no-network" => new.features.network = false,
            "--share" => new.shared_folders.push(parse_share(&value()?)?),
            "--forward" => new.port_forwards.push(parse_forward(flag, &value()?)?),
            other => return Err(err!("unknown option '{other}' for 'roomy vm create'")),
        }
    }
    if new.name.trim().is_empty() {
        return Err(err!("'roomy vm create' needs --name <name>"));
    }
    let parent = match parent {
        Some(parent) => parent,
        None => default_vm_parent()?,
    };
    let folder = vm::create(&Env::system(), &parent, new)?;
    // The VM exists either way; a library that cannot be updated is worth a warning, not a failure.
    if let Err(e) = library::default_path().and_then(|library| library::add(&library, folder.dir())) {
        eprintln!("roomy: the virtual machine was created but could not be added to the library: {e}");
    }
    Ok(format!("Created {}", folder.dir().display()))
}

/// One line per VM in the library: state, name and folder.
fn list() -> Result<String> {
    let env = Env::system();
    let mut lines = Vec::new();
    for dir in library::folders(&library::default_path()?)? {
        let line = match Folder::open(&dir).and_then(|folder| Ok((control::status(&env, &folder), folder.load()?))) {
            Ok((state, config)) => format!("{:<10} {}  ({})", state.word(), config.name, dir.display()),
            Err(e) => format!("{:<10} {}  ({e})", "missing", dir.display()),
        };
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(format!("No virtual machines yet. Create one with: {} vm create --name <name>", roomy_command()));
    }
    Ok(lines.join("\n"))
}

fn delete(args: &[String]) -> Result<String> {
    let (folders, flags): (Vec<&String>, Vec<&String>) = args.iter().partition(|a| !a.starts_with("--"));
    let ([dir], true) = (folders.as_slice(), flags.iter().all(|f| *f == "--yes")) else {
        return Err(err!("usage: {} vm delete <vm-folder> --yes", roomy_command()));
    };
    let folder = Folder::open(Path::new(dir))?;
    if flags.is_empty() {
        return Err(err!(
            "this permanently deletes the virtual machine in {}, including its disk.\nTo go ahead: {roomy} vm delete \
             {dir} --yes\nTo only take it off the list: {roomy} vm forget {dir}",
            folder.dir().display(),
            roomy = roomy_command()
        ));
    }
    let left = library::delete(&Env::system(), &library::default_path()?, &folder)?;
    Ok(if left.is_empty() {
        "deleted".to_owned()
    } else {
        format!("deleted; {} other item(s) in {} were left alone", left.len(), folder.dir().display())
    })
}

/// `roomy vm set <vm-folder> [options]`: change settings, saying for each when it takes effect.
fn set(args: &[String]) -> Result<String> {
    let Some((dir, options)) = args.split_first().filter(|(dir, _)| !dir.starts_with("--")) else {
        return Err(err!(
            "usage: {} vm set <vm-folder> [--name <name>] [--cpus <n>] [--memory <MB>] ...",
            roomy_command()
        ));
    };
    let folder = Folder::open(Path::new(dir))?;
    let config = folder.load()?;
    let mut changes = Changes::default();
    let (mut shares, mut forwards) = (config.shared_folders.clone(), config.port_forwards.clone());
    let switch = |flag: &str, value: &str| match value {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(err!("{flag} takes 'on' or 'off', not '{other}'")),
    };
    let mut options = options.iter();
    while let Some(flag) = options.next() {
        let value = options.next().cloned().ok_or_else(|| err!("{flag} needs a value"))?;
        match flag.as_str() {
            "--name" => changes.name = Some(value),
            "--os" => changes.os = Some(value),
            "--cpus" => changes.cpus = Some(number(flag, &value)?),
            "--memory" => changes.memory_mb = Some(number(flag, &value)?),
            "--disk" => changes.disk_gb = Some(number(flag, &value)?),
            "--gpu" => changes.gpu_acceleration = Some(switch(flag, &value)?),
            "--network" => changes.network = Some(switch(flag, &value)?),
            "--clipboard" => changes.clipboard = Some(switch(flag, &value)?),
            "--microphone" => changes.microphone = Some(switch(flag, &value)?),
            "--share" => {
                let share = parse_share(&value)?;
                shares.retain(|existing| existing.name != share.name);
                shares.push(share);
            }
            "--unshare" => {
                let before = shares.len();
                shares.retain(|share| share.name != value);
                if shares.len() == before {
                    return Err(err!("there is no shared folder named '{value}'"));
                }
            }
            "--forward" => {
                let forward = parse_forward(flag, &value)?;
                forwards.retain(|existing| existing.host != forward.host);
                forwards.push(forward);
            }
            "--unforward" => {
                let host: u16 = number(flag, &value)?;
                let before = forwards.len();
                forwards.retain(|forward| forward.host != host);
                if forwards.len() == before {
                    return Err(err!("port {host} of this computer is not forwarded"));
                }
            }
            other => return Err(err!("unknown option '{other}' for 'roomy vm set'")),
        }
    }
    changes.shared_folders = Some(shares);
    changes.port_forwards = Some(forwards);

    let applied = settings::apply(&Env::system(), &folder, &changes)?;
    if applied.is_empty() {
        return Ok("nothing to change".to_owned());
    }
    let lines: Vec<String> = applied
        .iter()
        .map(|change| {
            let when = match change.applies {
                Applies::Now => "changed",
                Applies::NextBoot => "saved; takes effect the next time the virtual machine is shut down and started",
            };
            match &change.note {
                Some(note) => format!("{}: {when}\n  {note}", change.setting),
                None => format!("{}: {when}", change.setting),
            }
        })
        .collect();
    Ok(lines.join("\n"))
}

fn default_vm_parent() -> Result<PathBuf> {
    library::default_vm_parent().map_err(|e| err!("{e}; pass --in <folder>"))
}

/// The value of `--share`: `<name>=<folder>`.
fn parse_share(value: &str) -> Result<SharedFolder> {
    let (name, path) = value
        .split_once('=')
        .ok_or_else(|| err!("--share takes <name>=<folder>, for example --share projects=/home/me/projects"))?;
    Ok(SharedFolder { name: name.to_owned(), path: absolute(path)? })
}

/// The value of `--forward`: `<host port>:<guest port>`.
fn parse_forward(flag: &str, value: &str) -> Result<PortForward> {
    let (host, guest) = value
        .split_once(':')
        .ok_or_else(|| err!("--forward takes <host port>:<guest port>, for example --forward 2222:22"))?;
    Ok(PortForward { host: number(flag, host)?, guest: number(flag, guest)? })
}

fn number<T: std::str::FromStr>(flag: &str, text: &str) -> Result<T> {
    text.parse().map_err(|_| err!("{flag}: '{text}' is not a valid number"))
}

/// Settings store absolute paths, so the VM works from any directory.
fn absolute(path: &str) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|e| err!("cannot resolve {path}: {e}"))
}

fn report(outcome: Result<String>) -> ExitCode {
    match outcome {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn fail(error: &Error) -> ExitCode {
    eprintln!("roomy: {error}");
    ExitCode::from(EXIT_PROBLEMS)
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// `roomy supervise <vm-folder> [--ready-fd <n>]`: the process that owns a
/// running VM. Started by `roomy vm start` and by the window; not meant to
/// be run by hand.
pub fn supervise(args: &[String]) -> ExitCode {
    let (folder, ready) = match args {
        [folder] => (folder, None),
        [folder, flag, fd] if flag == "--ready-fd" => match fd.parse::<i32>() {
            // SAFETY: the launcher passes this descriptor for us to own; nothing else uses it.
            Ok(fd) if fd > 2 => (folder, Some(unsafe { File::from_raw_fd(fd) })),
            _ => return usage_error("--ready-fd needs a descriptor number"),
        },
        _ => return usage_error("usage: roomy supervise <vm-folder> [--ready-fd <n>]"),
    };
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(signal, on_stop_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
    }
    match supervisor::run(&Env::system(), Path::new(folder), ready, &STOP) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("roomy supervisor: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}
