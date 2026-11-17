//! Talking to a running VM.
//!
//! Every running VM has one supervisor process (see [`crate::supervisor`])
//! that owns QEMU. Clients, whether the command line or the window, ask it
//! to do things over a unix socket: one JSON request line, one JSON reply line.
//! A VM with no supervisor is either off or suspended, which the VM folder
//! itself tells.

use crate::env::Env;
use crate::error::Context;
use crate::vm::{Folder, remove_if_present};
use crate::{Result, err};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Powered down.
    Off,
    /// Guest memory is saved on disk; Start continues where it left off.
    Suspended,
    Starting,
    Running,
    /// Frozen in memory; resumes instantly, lost if this computer powers off.
    Paused,
    /// Saving guest memory to disk.
    Suspending,
    /// Shutdown was requested and the guest has not powered off yet.
    Stopping,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Off => "off",
            State::Suspended => "suspended",
            State::Starting => "starting",
            State::Running => "running",
            State::Paused => "paused",
            State::Suspending => "suspending",
            State::Stopping => "stopping",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Request {
    Status,
    Pause,
    Resume,
    /// Save guest memory to disk and stop.
    Suspend,
    /// Ask the guest to shut down, as its power button would.
    Shutdown,
    /// Stop immediately, like pulling the plug.
    ForceOff,
    /// Ask the guest to restart.
    Restart,
    /// Reset immediately, like a reset button.
    ForceRestart,
    /// Open a private connection to the guest's screen, sound and clipboard.
    /// The connection arrives attached to the reply; use [`open_display`].
    Display,
    /// Plug or unplug the guest's network cable.
    SetNetwork {
        on: bool,
    },
    /// Make the running guest's disk larger.
    GrowDisk {
        size_gb: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    /// The VM's state after the request.
    pub state: State,
    /// Why the request failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Something worth telling the user even though the request worked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Whether the guest's helper program (the QEMU guest agent) is answering.
    #[serde(default)]
    pub guest_agent: bool,
}

/// What a running VM was started with, as its supervisor recorded it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Running {
    /// The graphics card its accelerated screen is drawn on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_node: Option<PathBuf>,
    /// The settings in effect, which differ from the settings file when
    /// something was changed that only applies at the next boot.
    pub config: crate::vm::Config,
}

fn running_file(env: &Env, folder: &Folder) -> PathBuf {
    runtime_dir(env, folder).join("running.toml")
}

/// What the VM is running with, or `None` if it is not running.
pub fn running(env: &Env, folder: &Folder) -> Option<Running> {
    toml::from_str(&fs::read_to_string(running_file(env, folder)).ok()?).ok()
}

pub(crate) fn record_running(env: &Env, folder: &Folder, running: &Running) -> Result<()> {
    let text = toml::to_string(running).context("cannot describe the running virtual machine")?;
    crate::vm::write_atomically(&running_file(env, folder), text.as_bytes())
}

/// Where a VM's sockets live while it runs: a private per-user directory
/// named after the VM folder's location.
pub fn runtime_dir(env: &Env, folder: &Folder) -> PathBuf {
    // FNV-1a: stable across roomy versions, unlike std's hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in folder.dir().as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    env.runtime_dir.join("roomy").join(format!("{hash:016x}"))
}

pub fn control_socket(env: &Env, folder: &Folder) -> PathBuf {
    runtime_dir(env, folder).join("control.sock")
}

/// Creates (or empties) a VM's runtime directory, readable by this user only.
pub fn prepare_runtime_dir(env: &Env, folder: &Folder) -> Result<PathBuf> {
    let dir = runtime_dir(env, folder);
    remove_if_present(&dir).context(format!("cannot clear {}", dir.display()))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .context(format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

/// The VM's current state.
pub fn status(env: &Env, folder: &Folder) -> State {
    match send(env, folder, Request::Status) {
        Ok(reply) => reply.state,
        Err(_) if folder.is_suspended() => State::Suspended,
        Err(_) => State::Off,
    }
}

/// Sends a request to the VM's supervisor. Fails if the VM is not running.
pub fn send(env: &Env, folder: &Folder, request: Request) -> Result<Reply> {
    // Saving a large guest to a slow disk can take minutes; everything else is quick.
    let limit = if request == Request::Suspend { Duration::from_secs(30 * 60) } else { Duration::from_secs(60) };
    let stream = connect_and_send(env, folder, request, limit)?;
    let mut reply = String::new();
    match BufReader::new(stream).read_line(&mut reply) {
        Ok(0) => Err(err!("the virtual machine stopped before answering")),
        Ok(_) => serde_json::from_str(&reply).context("unexpected answer from the virtual machine's supervisor"),
        Err(e) => Err(err!("no answer from the virtual machine's supervisor: {e}")),
    }
}

/// Opens a private connection to a running VM's screen, sound and clipboard
/// (QEMU's D-Bus display protocol, spoken directly over the returned socket).
pub fn open_display(env: &Env, folder: &Folder) -> Result<OwnedFd> {
    let stream = connect_and_send(env, folder, Request::Display, Duration::from_secs(60))?;
    let (mut text, mut connection, mut buf) = (Vec::new(), None, [0u8; 4096]);
    while !text.contains(&b'\n') {
        let (count, fd) = crate::fdpass::recv_with_fd(&stream, &mut buf)
            .map_err(|e| err!("no answer from the virtual machine's supervisor: {e}"))?;
        connection = connection.or(fd);
        if count == 0 {
            break;
        }
        text.extend_from_slice(&buf[..count]);
    }
    if text.is_empty() {
        return Err(err!("the virtual machine stopped before answering"));
    }
    let reply: Reply =
        serde_json::from_slice(&text).context("unexpected answer from the virtual machine's supervisor")?;
    match (reply.ok, connection) {
        (true, Some(connection)) => Ok(connection),
        (true, None) => Err(err!("the supervisor did not hand over the display connection")),
        (false, _) => Err(err!("{}", reply.error.unwrap_or_else(|| "the display could not be opened".into()))),
    }
}

/// Sends one request line to the VM's supervisor and leaves the connection waiting at most
/// `limit` for each read of the reply.
fn connect_and_send(env: &Env, folder: &Folder, request: Request, limit: Duration) -> Result<UnixStream> {
    let not_running = || err!("the virtual machine is not running");
    let mut stream = UnixStream::connect(control_socket(env, folder)).map_err(|_| not_running())?;
    stream.set_read_timeout(Some(limit)).context("cannot configure the control connection")?;
    let line = serde_json::to_string(&request).expect("requests always serialize");
    stream.write_all(format!("{line}\n").as_bytes()).map_err(|_| not_running())?;
    Ok(stream)
}

/// Like [`send`], but a refusal by the supervisor becomes an error.
pub fn request(env: &Env, folder: &Folder, request: Request) -> Result<Reply> {
    let reply = send(env, folder, request)?;
    match &reply.error {
        Some(error) if !reply.ok => Err(err!("{error}")),
        _ => Ok(reply),
    }
}

/// Starts the VM: boots it, or continues it if it is suspended.
///
/// Launches a detached supervisor (`<roomy> supervise <folder>`) that keeps
/// running after the caller exits, and waits until the guest is running or
/// the start has failed.
pub fn start(env: &Env, folder: &Folder, roomy_exe: &Path) -> Result<()> {
    if send(env, folder, Request::Status).is_ok() {
        return Err(err!("the virtual machine is already running"));
    }
    folder.ensure_dirs()?;
    let log_path = folder.log_dir().join("supervisor.log");
    crate::process::start_fresh_log(&log_path);
    let log = File::create(&log_path).context(format!("cannot write {}", log_path.display()))?;
    let (ready_read, ready_write) = pipe()?;

    const READY_FD: i32 = 3;
    let write_fd = ready_write.as_raw_fd();
    let mut command = Command::new(roomy_exe);
    command
        .arg("supervise")
        .arg(folder.dir())
        .arg("--ready-fd")
        .arg(READY_FD.to_string())
        .stdin(Stdio::null())
        .stdout(log.try_clone().context("cannot duplicate the log handle")?)
        .stderr(log);
    if let Some(node) = &env.render_node {
        command.env("ROOMY_RENDER_NODE", node);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            // Own session: the supervisor must outlive the terminal or window that started it.
            libc::setsid();
            let failed = if write_fd == READY_FD {
                libc::fcntl(READY_FD, libc::F_SETFD, 0) < 0
            } else {
                libc::dup2(write_fd, READY_FD) < 0
            };
            if failed { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        });
    }
    let mut child = command.spawn().context(format!("cannot run {}", roomy_exe.display()))?;
    drop(ready_write);
    // Reap the supervisor when it eventually exits, so a long-lived caller does not collect zombies.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    match read_ready(ready_read, Duration::from_secs(10 * 60)) {
        Ready::Ok => Ok(()),
        Ready::Failed(error) => Err(err!("{error}")),
        Ready::Vanished => Err(err!(
            "the virtual machine's supervisor stopped unexpectedly; see {}\n{}",
            log_path.display(),
            crate::process::read_tail(&log_path, 10)
        )),
        Ready::TimedOut => Err(err!("the virtual machine did not start within 10 minutes; see {}", log_path.display())),
    }
}

/// What the supervisor writes on its ready pipe once startup has finished.
#[derive(Serialize, Deserialize)]
pub(crate) struct ReadyMessage {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

enum Ready {
    Ok,
    Failed(String),
    Vanished,
    TimedOut,
}

fn read_ready(pipe: OwnedFd, limit: Duration) -> Ready {
    let deadline = Instant::now() + limit;
    let mut file = File::from(pipe);
    let mut data = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ready::TimedOut;
        }
        let mut poll = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let wait_ms = remaining.as_millis().min(1_000) as i32;
        // SAFETY: `poll` points to one valid pollfd for the duration of the call.
        if unsafe { libc::poll(&mut poll, 1, wait_ms) } <= 0 {
            continue;
        }
        let mut chunk = [0u8; 4096];
        match file.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
        }
        if data.contains(&b'\n') {
            break;
        }
    }
    match serde_json::from_slice::<ReadyMessage>(&data) {
        Ok(ReadyMessage { ok: true, .. }) => Ready::Ok,
        Ok(ReadyMessage { error, .. }) => Ready::Failed(error.unwrap_or_else(|| "the start failed".to_owned())),
        Err(_) => Ready::Vanished,
    }
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a valid array of two ints; on success both are fresh descriptors we own.
    unsafe {
        if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
            return Err(err!("cannot create a pipe: {}", std::io::Error::last_os_error()));
        }
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}
