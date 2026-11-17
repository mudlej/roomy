//! The `roomy` command.

mod vm;

use roomy_core::doctor::{self, Report};
use roomy_core::env::Env;
use roomy_core::{host, setup};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

const HELP: &str = "\
roomy - simple, reliable virtual machines

Usage:
  roomy
      Open the window.
  roomy doctor [--json] [--report-file <path>]
      Check that this computer can run roomy's virtual machines. Prints a
      report you can copy when something is wrong.
  roomy setup [--yes]
      Install what roomy needs from your distribution's packages, then run
      the checks again. Asks before changing anything unless --yes is given.

  roomy vm create --name <name> [--in <folder>] [--os <id>] [--cpus <n>]
                  [--memory <MB>] [--disk <GB>] [--iso <path>] [--from-disk <qcow2>]
                  [--gpu] [--no-network] [--share <name>=<folder>]... [--forward <host>:<guest>]...
      Create a virtual machine in its own folder (default location: ~/VMs).
  roomy vm list                       The virtual machines in your library
  roomy vm set <vm-folder> [--name <name>] [--cpus <n>] [--memory <MB>] [--disk <GB>]
               [--gpu on|off] [--network on|off] [--clipboard on|off] [--microphone on|off]
               [--share <name>=<folder>]... [--unshare <name>]... [--forward <host>:<guest>]... [--unforward <host>]...
      Change settings. Each change is reported as in effect already or waiting for the next boot.
  roomy vm status <vm-folder>
  roomy vm start <vm-folder>          Boot, or continue a suspended VM
  roomy vm pause <vm-folder>          Freeze in memory; instant, lost if this computer powers off
  roomy vm resume <vm-folder>
  roomy vm suspend <vm-folder>        Save to disk and stop; survives a reboot of this computer
  roomy vm shutdown <vm-folder>       Ask the guest to shut down
  roomy vm restart <vm-folder>        Ask the guest to restart
  roomy vm force-off <vm-folder>      Stop immediately, like pulling the plug
  roomy vm force-restart <vm-folder>  Reset immediately
  roomy vm discard-state <vm-folder>  Throw away a suspended VM's saved state
  roomy vm add <vm-folder>            Add an existing VM folder to your library
  roomy vm forget <vm-folder>         Take a VM off the list, keeping its files
  roomy vm delete <vm-folder> --yes   Delete a VM and its disk permanently

  roomy --version
  roomy --help
";

/// Exit codes: 0 fine, 1 the checks found a blocking problem, 2 the command could not be carried out.
pub(crate) const EXIT_PROBLEMS: u8 = 1;
pub(crate) const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    // Die quietly when output is piped into something that stops reading (e.g. `| head`).
    // SAFETY: restoring the default SIGPIPE action before any other thread exists.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => return open_window(),
    };
    match command {
        "doctor" => run_doctor(rest),
        "setup" => run_setup(rest),
        "vm" => vm::run(rest),
        "supervise" => vm::supervise(rest),
        "--version" | "-V" => {
            println!("roomy {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        other => usage_error(&format!("unknown command '{other}'")),
    }
}

/// The program that shows roomy's window: named by `ROOMY_WINDOW_PROGRAM`
/// (for roomy's own tests), else installed next to this one, else on the PATH.
fn window_program(env: &Env) -> Option<PathBuf> {
    let named = std::env::var_os("ROOMY_WINDOW_PROGRAM").map(PathBuf::from);
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.to_path_buf()))
        .map(|dir| [dir.join(doctor::WINDOW_PROGRAM), dir.join("../libexec").join(doctor::WINDOW_PROGRAM)]);
    named
        .into_iter()
        .chain(beside.into_iter().flatten())
        .find(|p| roomy_core::env::is_executable(p))
        .or_else(|| env.find_program(doctor::WINDOW_PROGRAM))
}

/// `roomy` without arguments: open the window. The window is a separate
/// program so that a computer without the graphical libraries still gets an
/// explanation, and working `doctor` and `setup` commands, instead of a loader error.
fn open_window() -> ExitCode {
    use std::os::unix::process::CommandExt;
    let env = Env::system();
    let check = doctor::check_window(window_program(&env).as_deref());
    let Some(program) = window_program(&env).filter(|_| check.status == doctor::Status::Ok) else {
        eprintln!("roomy: {}.", check.summary);
        if let Some(log) = &check.log {
            eprintln!("{}", log.trim_end());
        }
        eprintln!("\nRun 'roomy doctor' to see what is missing, or 'roomy setup' to install it.");
        eprintln!("Virtual machines can be used without the window: see 'roomy --help'.");
        return ExitCode::from(EXIT_PROBLEMS);
    };
    let mut command = Command::new(&program);
    if let Ok(this) = std::env::current_exe() {
        command.env("ROOMY_PROGRAM", this);
    }
    // Replaces this process; only returns if that failed.
    let error = command.exec();
    eprintln!("roomy: cannot start {}: {error}", program.display());
    ExitCode::from(EXIT_PROBLEMS)
}

/// The doctor's report, including whether the window can start.
fn full_report(env: &Env) -> Report {
    let mut report = doctor::run(env);
    report.checks.push(doctor::check_window(window_program(env).as_deref()));
    report.checks.push(doctor::check_sound());
    report
}

pub(crate) fn usage_error(message: &str) -> ExitCode {
    eprintln!("roomy: {message}\n\n{HELP}");
    ExitCode::from(EXIT_USAGE)
}

fn run_doctor(args: &[String]) -> ExitCode {
    let mut as_json = false;
    let mut report_file: Option<PathBuf> = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => as_json = true,
            "--report-file" => match args.next() {
                Some(path) => report_file = Some(PathBuf::from(path)),
                None => return usage_error("--report-file needs a path"),
            },
            other => return usage_error(&format!("unknown option '{other}' for doctor")),
        }
    }

    let report = full_report(&Env::system());
    if as_json {
        println!("{:#}", report.to_json());
    } else {
        print_report(&report);
    }
    if let Some(path) = report_file {
        match std::fs::write(&path, report.to_text()) {
            Ok(()) => eprintln!("Report saved to {}", path.display()),
            Err(e) => {
                eprintln!("roomy: could not save the report to {}: {e}", path.display());
                return ExitCode::from(EXIT_USAGE);
            }
        }
    } else if !as_json && !report.can_run_vms() {
        println!("\nThe text above is the full report: copy it when asking for help,");
        println!("or save it with: roomy doctor --report-file roomy-doctor.txt");
    }
    exit_code_for(&report)
}

fn exit_code_for(report: &Report) -> ExitCode {
    if report.can_run_vms() { ExitCode::SUCCESS } else { ExitCode::from(EXIT_PROBLEMS) }
}

/// Prints the plain-text report, colouring the status tags on a terminal.
fn print_report(report: &Report) {
    let colour = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in report.to_text().lines() {
        let painted = colour
            .then(|| {
                [("[ OK ]", "32"), ("[WARN]", "33"), ("[FAIL]", "31"), ("[SKIP]", "90")].iter().find_map(
                    |(tag, code)| line.strip_prefix(tag).map(|rest| format!("\x1b[{code}m{tag}\x1b[0m{rest}")),
                )
            })
            .flatten();
        let _ = writeln!(out, "{}", painted.as_deref().unwrap_or(line));
    }
}

fn run_setup(args: &[String]) -> ExitCode {
    let mut assume_yes = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => assume_yes = true,
            other => return usage_error(&format!("unknown option '{other}' for setup")),
        }
    }

    let env = Env::system();
    println!("Checking this computer...\n");
    let before = full_report(&env);
    print_report(&before);
    if before.can_run_vms() && before.count(doctor::Status::Warning) == 0 {
        return ExitCode::SUCCESS;
    }

    let os = host::os_release(&env);
    let Some(plan) = setup::install_plan(&os) else {
        println!("\nroomy does not know how to install packages on {} yet.", os.pretty_name);
        println!("Install these with your package manager, then run 'roomy doctor' again:");
        for component in setup::REQUIRED_COMPONENTS {
            println!("  - {component}");
        }
        return ExitCode::from(EXIT_USAGE);
    };

    println!("\nroomy can install the required packages with this command (run as administrator):");
    println!("  {}", plan.command_line());
    println!("Problems that are not about missing packages (for example KVM access) are not changed by this.");
    if !assume_yes && !confirm("Run it now?") {
        println!("Nothing was changed.");
        return ExitCode::from(EXIT_USAGE);
    }

    // Having been told yes already, the package manager must not stop to ask again.
    let to_run = if assume_yes { plan.unattended_command() } else { plan.command.clone() };
    let mut command = match privileged(&env, &to_run) {
        Some(command) => command,
        None => {
            println!("No way to become administrator was found (sudo or pkexec). Run the command above as root.");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            println!("\nThe package manager did not finish successfully ({status}). Its messages are above.");
            return ExitCode::from(EXIT_USAGE);
        }
        Err(e) => {
            println!("\nCould not run the package manager: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    }

    println!("\nChecking again...\n");
    let after = full_report(&env);
    print_report(&after);
    exit_code_for(&after)
}

/// Asks a yes/no question. Anything but an explicit yes, including no
/// terminal to ask on, counts as no.
fn confirm(question: &str) -> bool {
    if !io::stdin().is_terminal() {
        println!("{question} Not asking because input is not a terminal; pass --yes to proceed.");
        return false;
    }
    print!("{question} [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    if io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Wraps a command so that it runs as root.
fn privileged(env: &Env, command: &[&str]) -> Option<Command> {
    // SAFETY: geteuid cannot fail.
    let is_root = unsafe { libc::geteuid() } == 0;
    let (program, args) = command.split_first()?;
    if is_root {
        let mut direct = Command::new(program);
        direct.args(args);
        return Some(direct);
    }
    let elevator = env.find_program("sudo").or_else(|| env.find_program("pkexec"))?;
    let mut elevated = Command::new(elevator);
    elevated.arg(program).args(args);
    Some(elevated)
}
