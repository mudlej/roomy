//! Running helper programs with a time limit.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub struct Captured {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a program to completion and captures its output. The program is
/// killed and an error returned if it runs longer than `limit`.
///
/// Meant for short commands with small output (version queries and the like).
pub fn run_capture(program: &Path, args: &[&str], limit: Duration) -> io::Result<Captured> {
    let mut child =
        Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let Some(status) = wait_with_limit(&mut child, limit)? else {
        kill_and_reap(&mut child);
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{} did not finish within {limit:?}", program.display()),
        ));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let mut bytes = Vec::new();
        out.read_to_end(&mut bytes)?;
        stdout = String::from_utf8_lossy(&bytes).into_owned();
    }
    if let Some(mut err) = child.stderr.take() {
        let mut bytes = Vec::new();
        err.read_to_end(&mut bytes)?;
        stderr = String::from_utf8_lossy(&bytes).into_owned();
    }
    Ok(Captured { status, stdout, stderr })
}

/// Waits for a child to exit. Returns `None` if it is still running after `limit`.
pub fn wait_with_limit(child: &mut Child, limit: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Starts a long-running helper with its output appended to a log file.
///
/// The helper is tied to the calling thread: if that thread (in roomy, the
/// VM supervisor's main thread) dies, the kernel sends the helper SIGTERM so
/// it can never be left running without an owner.
pub fn spawn_logged(program: &Path, args: &[String], log_path: &Path) -> io::Result<Child> {
    let log = File::options().create(true).append(true).open(log_path)?;
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: prctl is async-signal-safe and touches no memory shared with the parent.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    command.spawn()
}

/// A child process that is killed when this value is dropped.
pub struct KillOnDrop(pub Child);

impl KillOnDrop {
    pub fn running(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        kill_and_reap(&mut self.0);
    }
}

/// Polls `done` until it returns true or `limit` passes. Returns whether it became true.
pub fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Makes room for a new log at `path`: what the previous run wrote is kept
/// once, as `<name>.1`, so the log of a run that went wrong survives the next start.
pub fn start_fresh_log(path: &Path) {
    let has_content = std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);
    if has_content {
        let mut previous = path.as_os_str().to_owned();
        previous.push(".1");
        if std::fs::rename(path, &previous).is_ok() {
            return;
        }
    }
    let _ = std::fs::remove_file(path);
}

/// The last `lines` lines of a text file; empty if it cannot be read.
pub fn read_tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
