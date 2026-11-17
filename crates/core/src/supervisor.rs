//! The per-VM supervisor: the one process that owns a running VM.
//!
//! It starts QEMU and the shared-folder daemons, carries out lifecycle
//! requests from clients (see [`crate::control`]) one at a time, and ends
//! when QEMU ends. Having a single owner is what keeps the on-disk state
//! consistent:
//!
//! - **Suspend** writes guest memory to a `.partial` file and renames it into
//!   place only after it is complete and flushed to disk. A crash before the
//!   rename leaves a VM that is simply off; QEMU is told to quit only after.
//! - **Restore** renames the saved state away *before* the guest continues,
//!   so a state can never be loaded twice. Loading it a second time, after
//!   the disk has moved on, would corrupt the guest's file systems.
//! - A lock on the VM folder guarantees there is never a second supervisor.

use crate::agent::{self, Agent, ClockSync};
use crate::control::{self, ReadyMessage, Reply, Request, State};
use crate::env::Env;
use crate::error::Context;
use crate::host::{self, Firmware};
use crate::process::{KillOnDrop, read_tail, spawn_logged, start_fresh_log, wait_until, wait_with_limit};
use crate::qemu::{self, Launch, Sockets, ids};
use crate::qmp::{Qmp, QmpError};
use crate::shares::{self, DaemonFailure};
use crate::time::utc_timestamp_secs;
use crate::vm::{self, Config, Folder};
use crate::{Result, err, fdpass};
use serde_json::json;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// How long a guest gets to shut down when the supervisor itself is told to stop.
const STOP_GRACE: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Runs a VM until it powers off or is suspended.
///
/// Boots the VM, or continues it if the folder holds a saved state. Once the
/// guest is running (or the start has failed) one JSON line is written to
/// `ready`, which is how [`control::start`] learns the outcome. Setting
/// `stop` asks for a graceful shutdown.
pub fn run(env: &Env, dir: &Path, ready: Option<File>, stop: &AtomicBool) -> Result<()> {
    let started = Supervisor::start(env, dir);
    if let Some(mut ready) = ready {
        let message = match &started {
            Ok(_) => ReadyMessage { ok: true, error: None },
            Err(e) => ReadyMessage { ok: false, error: Some(e.to_string()) },
        };
        let line = serde_json::to_string(&message).expect("ready messages always serialize");
        let _ = writeln!(ready, "{line}");
    }
    started?.serve(stop)
}

struct Supervisor {
    env: Env,
    folder: Folder,
    /// The settings the guest is running with.
    config: Config,
    state: State,
    /// Why the guest is paused when roomy did not pause it (e.g. a full disk).
    trouble: Option<String>,
    /// When a stop requested through the `stop` flag turns into force-off.
    stop_deadline: Option<Instant>,
    agent: Agent,
    qmp: Qmp,
    qemu_log: PathBuf,
    /// The graphics card accelerated graphics are drawn on.
    render_node: Option<PathBuf>,
    /// Whether the guest's agent answered the last time it was looked for, and when that was.
    agent_seen: bool,
    agent_checked: Option<Instant>,
    /// How many display connections have been handed out; names QEMU's side of each.
    displays_opened: u64,
    /// A display connection made for the request being answered, sent with its reply.
    handoff: Option<OwnedFd>,
    // Drop order matters from here on: QEMU first, then its helpers, then the
    // sockets directory, and the folder lock last.
    qemu: KillOnDrop,
    helpers: Vec<KillOnDrop>,
    listener: UnixListener,
    _runtime: RuntimeDir,
    _lock: File,
}

impl Supervisor {
    fn start(env: &Env, dir: &Path) -> Result<Supervisor> {
        let folder = Folder::open(dir)?;
        let lock = folder.lock_after_supervisor()?.ok_or_else(|| err!("the virtual machine is already running"))?;
        folder.ensure_dirs()?;
        folder.clean_interrupted_state()?;

        let restoring = folder.is_suspended();
        let current = folder.load()?;
        let mut config = if restoring {
            Folder::load_from(&folder.saved_config())
                .context("the suspended virtual machine's saved settings cannot be read")?
        } else {
            current.clone()
        };

        let qemu_path = env.require(host::QEMU_PROGRAM)?;
        let firmware = host::require_firmware(env)?;
        check_files(&folder, &mut config, &firmware, restoring)?;

        let runtime = RuntimeDir(control::prepare_runtime_dir(env, &folder)?);
        let sockets = Sockets::in_dir(&runtime.0);
        let listener = UnixListener::bind(control::control_socket(env, &folder))
            .and_then(|l| l.set_nonblocking(true).map(|()| l))
            .context(format!("cannot open the control socket {}", control::control_socket(env, &folder).display()))?;

        let helpers = start_share_daemons(env, &folder, &config, &sockets)?;

        let qemu_log = folder.log_dir().join("qemu.log");
        start_fresh_log(&qemu_log);
        // The card the window is drawn with if a window started this VM, else the one with the monitor.
        let render_node = env.vm_render_node().filter(|_| config.features.gpu_acceleration);
        if let Some(node) = &render_node {
            log(&format!("accelerated graphics on {}", node.display()));
        }
        let mut args = qemu::args(&Launch {
            config: &config,
            folder: &folder,
            sockets: &sockets,
            firmware: &firmware,
            restoring,
            render_node: render_node.as_deref(),
        });
        // For finding faults in QEMU's own behaviour: ROOMY_QEMU_TRACE="vdagent_* dbus_clipboard_*" makes
        // QEMU write those of its trace events to this VM's qemu.log.
        for pattern in std::env::var("ROOMY_QEMU_TRACE").unwrap_or_default().split_whitespace() {
            args.extend(["-trace".to_owned(), pattern.to_owned()]);
        }
        let mut qemu = KillOnDrop(
            spawn_logged(&qemu_path, &args, &qemu_log).context(format!("cannot run {}", qemu_path.display()))?,
        );
        let qmp = Qmp::connect(&sockets.qmp(), Duration::from_secs(30), || qemu.running())
            .map_err(|e| with_log(err!("QEMU did not start ({e})"), &qemu_log))?;

        let mut supervisor = Supervisor {
            env: env.clone(),
            agent: Agent::new(sockets.agent()),
            folder,
            config,
            state: State::Starting,
            trouble: None,
            stop_deadline: None,
            render_node,
            agent_seen: false,
            agent_checked: None,
            displays_opened: 0,
            handoff: None,
            qmp,
            qemu_log,
            qemu,
            helpers,
            listener,
            _runtime: runtime,
            _lock: lock,
        };
        if restoring {
            supervisor.load_saved_state()?;
        }
        // Switches that take effect immediately follow the current settings,
        // even when the hardware layout comes from the suspended guest's.
        if !current.features.network {
            supervisor.command("set_link", json!({ "name": ids::NIC, "up": false }))?;
        }
        supervisor.command("cont", json!({}))?;
        supervisor.state = State::Running;
        supervisor.record_running();
        if restoring {
            // The guest is running again: the saved state is spent for good.
            let _ = fs::remove_file(supervisor.folder.saved_state_consumed());
            let _ = fs::remove_file(supervisor.folder.saved_config());
            let note = supervisor.sync_clock();
            log(&format!("restored from saved state{}", note.map(|n| format!(" ({n})")).unwrap_or_default()));
        } else {
            log("started");
        }
        Ok(supervisor)
    }

    /// Loads the suspended guest's memory into the waiting QEMU.
    fn load_saved_state(&mut self) -> Result<()> {
        let state = self.folder.saved_state();
        let load = |qmp: &mut Qmp| -> std::result::Result<(), QmpError> {
            migration_setup(qmp)?;
            qmp.execute("migrate-incoming", Some(json!({ "uri": format!("file:{}", state.display()) })))?;
            Ok(())
        };
        let loaded = load(&mut self.qmp).map_err(|e| e.to_string()).and_then(|()| wait_migration(&mut self.qmp));
        if let Err(why) = loaded {
            // The saved state is left in place: the user can retry, or discard it.
            return Err(with_log(err!("the saved state could not be loaded: {why}"), &self.qemu_log));
        }
        let consumed = self.folder.saved_state_consumed();
        fs::rename(&state, &consumed)
            .and_then(|()| vm::sync_parent(&consumed))
            .context("cannot mark the saved state as used")
    }

    fn serve(mut self, stop: &AtomicBool) -> Result<()> {
        let mut last_poll = Instant::now();
        while self.qemu.running() {
            self.accept_requests();
            if stop.load(Ordering::Relaxed) && self.stop_deadline.is_none() {
                log("asked to stop: shutting the guest down");
                let _ = self.shutdown();
                self.stop_deadline = Some(Instant::now() + STOP_GRACE);
            }
            if self.stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                log("the guest did not shut down in time: forcing it off");
                let _ = self.force_off();
            }
            if last_poll.elapsed() >= POLL_INTERVAL {
                self.poll();
                last_poll = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        self.finish();
        Ok(())
    }

    /// QEMU has exited: let the helpers finish and clean up.
    fn finish(&mut self) {
        let exit = self.qemu.0.try_wait().ok().flatten();
        let expected = matches!(self.state, State::Stopping | State::Suspended | State::Off);
        match exit {
            Some(status) if status.success() => log("QEMU exited"),
            Some(status) if expected => log(&format!("QEMU exited ({status})")),
            Some(status) => log(&format!("QEMU stopped unexpectedly ({status}):\n{}", read_tail(&self.qemu_log, 15))),
            None => {}
        }
        // The shared-folder daemons exit on their own once QEMU is gone.
        let helpers = &mut self.helpers;
        wait_until(Duration::from_secs(4), || helpers.iter_mut().all(|h| !h.running()));
    }

    fn accept_requests(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => self.handle(stream),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }

    fn handle(&mut self, stream: UnixStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let Ok(mut writer) = stream.try_clone() else { return };
        let mut line = String::new();
        if BufReader::new(stream).read_line(&mut line).is_err() {
            return;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) => self.dispatch(request),
            Err(e) => self.reply(false, Some(format!("unknown request: {e}")), None),
        };
        let text = serde_json::to_string(&reply).expect("replies always serialize");
        let _ = match self.handoff.take() {
            Some(fd) => fdpass::send_with_fd(&writer, format!("{text}\n").as_bytes(), fd.as_fd()),
            None => writeln!(writer, "{text}"),
        };
    }

    fn dispatch(&mut self, request: Request) -> Reply {
        let result = match request {
            Request::Status => Ok(None),
            Request::Pause => self.pause(),
            Request::Resume => self.resume(),
            Request::Suspend => self.suspend(),
            Request::Shutdown => self.shutdown(),
            Request::ForceOff => self.force_off(),
            Request::Restart => self.restart(),
            Request::ForceRestart => self.command("system_reset", json!({})).map(|()| None),
            Request::Display => self.open_display(),
            Request::SetNetwork { on } => self.set_network(on),
            Request::GrowDisk { size_gb } => self.grow_disk(size_gb),
        };
        match result {
            Ok(note) => self.reply(true, None, note.or_else(|| self.trouble.clone())),
            Err(e) => {
                log(&format!("{request:?} failed: {e}"));
                self.reply(false, Some(e.to_string()), None)
            }
        }
    }

    fn reply(&self, ok: bool, error: Option<String>, note: Option<String>) -> Reply {
        Reply { ok, state: self.state, error, note, guest_agent: self.agent_seen }
    }

    /// Writes down what the guest is running with, for clients to compare with the settings file.
    fn record_running(&self) {
        let running = control::Running { render_node: self.render_node.clone(), config: self.config.clone() };
        if let Err(e) = control::record_running(&self.env, &self.folder, &running) {
            log(&format!("could not record the running settings: {e}"));
        }
    }

    fn set_network(&mut self, on: bool) -> Result<Option<String>> {
        self.command("set_link", json!({ "name": ids::NIC, "up": on }))?;
        self.config.features.network = on;
        self.record_running();
        Ok(None)
    }

    fn grow_disk(&mut self, size_gb: u64) -> Result<Option<String>> {
        let bytes = size_gb.checked_mul(1 << 30).ok_or_else(|| err!("that disk size is too large"))?;
        self.command("block_resize", json!({ "node-name": ids::DISK, "size": bytes }))?;
        Ok(None)
    }

    fn pause(&mut self) -> Result<Option<String>> {
        match self.state {
            State::Paused => Ok(None),
            State::Running => {
                self.command("stop", json!({}))?;
                self.state = State::Paused;
                Ok(None)
            }
            other => Err(err!("cannot pause while the virtual machine is {}", other.word())),
        }
    }

    fn resume(&mut self) -> Result<Option<String>> {
        match self.state {
            State::Running => Ok(None),
            State::Paused => {
                self.command("cont", json!({}))?;
                self.state = State::Running;
                self.trouble = None;
                Ok(self.sync_clock())
            }
            other => Err(err!("cannot resume while the virtual machine is {}", other.word())),
        }
    }

    fn suspend(&mut self) -> Result<Option<String>> {
        if !matches!(self.state, State::Running | State::Paused) {
            return Err(err!("cannot suspend while the virtual machine is {}", self.state.word()));
        }
        if self.config.features.gpu_acceleration {
            return Err(err!(
                "Suspend is not available while GPU acceleration is on, because QEMU cannot save the state of \
                 an accelerated graphics card. Use Pause instead, or turn GPU acceleration off (applies at next boot)."
            ));
        }
        let previous = self.state;
        self.state = State::Suspending;
        let partial = self.folder.saved_state_partial();
        let _ = fs::remove_file(&partial);

        let save = |qmp: &mut Qmp| -> std::result::Result<(), QmpError> {
            if previous == State::Running {
                qmp.execute("stop", None)?;
            }
            migration_setup(qmp)?;
            qmp.execute("migrate", Some(json!({ "uri": format!("file:{}", partial.display()) })))?;
            Ok(())
        };
        let saved = save(&mut self.qmp)
            .map_err(|e| e.to_string())
            .and_then(|()| wait_migration(&mut self.qmp))
            .and_then(|()| self.commit_saved_state(&partial).map_err(|e| e.to_string()));
        if let Err(why) = saved {
            // Nothing was committed: put the guest back the way it was.
            let _ = fs::remove_file(&partial);
            if previous == State::Running {
                let _ = self.qmp.execute("cont", None);
            }
            self.state = previous;
            return Err(err!("the virtual machine's state could not be saved, so it keeps running: {why}"));
        }

        // The state is safely on disk; the guest must not run any further.
        self.state = State::Suspended;
        self.quit_qemu();
        let _ = wait_with_limit(&mut self.qemu.0, Duration::from_secs(10));
        log("suspended to disk");
        Ok(None)
    }

    /// Makes a completely written state file the VM's saved state, durably.
    fn commit_saved_state(&self, partial: &Path) -> Result<()> {
        File::open(partial).and_then(|f| f.sync_all()).context("cannot flush the saved state to disk")?;
        vm::write_atomically(&self.folder.saved_config(), self.config.to_toml().as_bytes())?;
        let state = self.folder.saved_state();
        fs::rename(partial, &state).and_then(|()| vm::sync_parent(&state)).context("cannot store the saved state")
    }

    fn shutdown(&mut self) -> Result<Option<String>> {
        match self.state {
            State::Paused => self.command("cont", json!({}))?,
            State::Running | State::Stopping => {}
            other => return Err(err!("cannot shut down while the virtual machine is {}", other.word())),
        }
        self.command("system_powerdown", json!({}))?;
        self.state = State::Stopping;
        Ok(None)
    }

    fn force_off(&mut self) -> Result<Option<String>> {
        self.state = State::Off;
        self.quit_qemu();
        if !matches!(wait_with_limit(&mut self.qemu.0, Duration::from_secs(5)), Ok(Some(_))) {
            let _ = self.qemu.0.kill();
        }
        Ok(None)
    }

    /// Tells QEMU to exit at once, or makes it if it does not listen.
    fn quit_qemu(&mut self) {
        if self.qmp.execute("quit", None).is_err() {
            let _ = self.qemu.0.kill();
        }
    }

    fn restart(&mut self) -> Result<Option<String>> {
        match self.state {
            State::Paused => self.command("cont", json!({}))?,
            State::Running => {}
            other => return Err(err!("cannot restart while the virtual machine is {}", other.word())),
        }
        self.state = State::Running;
        // Ctrl+Alt+Delete: Linux guests reboot cleanly on it.
        let keys: Vec<_> =
            ["ctrl", "alt", "delete"].iter().map(|key| json!({ "type": "qcode", "data": key })).collect();
        self.command("send-key", json!({ "keys": keys }))?;
        Ok(None)
    }

    /// Keeps `state` in line with what QEMU reports and tidies up between requests.
    fn poll(&mut self) {
        self.qmp.take_events();
        self.agent.discard_late_replies();
        if !matches!(self.state, State::Running | State::Paused) {
            return;
        }
        // The agent starts late in the guest's boot and can stop at any time; look every few seconds.
        if self.agent_checked.is_none_or(|at| at.elapsed() >= Duration::from_secs(3)) {
            self.agent_seen = self.state == State::Running && agent::agent_is_listening(&mut self.qmp);
            self.agent_checked = Some(Instant::now());
        }
        let Ok(status) = self.qmp.execute("query-status", None) else { return };
        match status["status"].as_str() {
            Some("running") => {
                self.state = State::Running;
                self.trouble = None;
            }
            Some("paused") => self.state = State::Paused,
            Some(other) => {
                // QEMU stopped the guest itself, e.g. "io-error" when the host disk is full.
                self.state = State::Paused;
                self.trouble = Some(format!("QEMU paused the guest: {other}"));
            }
            None => {}
        }
    }

    /// Corrects the guest's clock after it has been stopped for a while.
    fn sync_clock(&mut self) -> Option<String> {
        match self.agent.sync_clock(&mut self.qmp) {
            ClockSync::Synced => None,
            ClockSync::AgentNotRunning => Some(
                "The guest's clock may be behind until the guest corrects it itself. With the QEMU guest agent \
                 installed in the guest, roomy corrects it instantly."
                    .to_owned(),
            ),
            ClockSync::NoAnswer => Some("The guest's clock may be behind: its agent has not answered yet.".to_owned()),
            ClockSync::Failed(why) => Some(format!("The guest's clock may be behind: {why}")),
        }
    }

    /// Makes a private connection to the guest's screen, sound and clipboard
    /// and leaves our end in `handoff` for the client that asked.
    fn open_display(&mut self) -> Result<Option<String>> {
        if self.state == State::Suspending {
            return Err(err!("the virtual machine is being suspended"));
        }
        let (ours, theirs) = UnixStream::pair().context("cannot create the display connection")?;
        self.displays_opened += 1;
        let name = format!("display{}", self.displays_opened);
        self.qmp
            .execute_with_fd("getfd", Some(json!({ "fdname": name })), theirs.as_fd())
            .map_err(|e| err!("QEMU did not accept the display connection: {e}"))?;
        drop(theirs);
        if let Err(e) = self.command("add_client", json!({ "protocol": "@dbus-display", "fdname": name })) {
            let _ = self.qmp.execute("closefd", Some(json!({ "fdname": name })));
            return Err(e);
        }
        self.handoff = Some(ours.into());
        Ok(None)
    }

    fn command(&mut self, name: &str, arguments: serde_json::Value) -> Result<()> {
        self.qmp.execute(name, Some(arguments)).map(|_| ()).map_err(|e| match e {
            QmpError::Command { desc, .. } => err!("QEMU refused '{name}': {desc}"),
            other => err!("QEMU did not carry out '{name}': {other}"),
        })
    }
}

/// Removes the VM's sockets directory when the supervisor ends.
struct RuntimeDir(PathBuf);

impl Drop for RuntimeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Verifies the files a launch needs, repairing what can safely be repaired.
fn check_files(folder: &Folder, config: &mut Config, firmware: &Firmware, restoring: bool) -> Result<()> {
    let disk = folder.disk_path(config);
    if !disk.is_file() {
        return Err(err!("the virtual disk {} is missing", disk.display()));
    }
    if !folder.firmware_vars().is_file() {
        if restoring {
            return Err(err!("{} is missing; the saved state cannot be continued", folder.firmware_vars().display()));
        }
        fs::copy(&firmware.vars_template, folder.firmware_vars())
            .context("cannot recreate the firmware variable store")?;
    }
    if let Some(image) = config.install_image.take_if(|image| !image.is_file()) {
        if restoring {
            return Err(err!(
                "the installation image {} is gone, but the suspended guest still has it attached",
                image.display()
            ));
        }
        // Installation media is routinely deleted after installing; boot without it.
        log(&format!("installation image {} not found; starting without it", image.display()));
    }
    for share in &config.shared_folders {
        if !share.path.is_dir() {
            return Err(err!(
                "shared folder '{}' points to {}, which is not a folder",
                share.name,
                share.path.display()
            ));
        }
    }
    Ok(())
}

fn start_share_daemons(env: &Env, folder: &Folder, config: &Config, sockets: &Sockets) -> Result<Vec<KillOnDrop>> {
    let mut daemons = Vec::new();
    if config.shared_folders.is_empty() {
        return Ok(daemons);
    }
    let virtiofsd = host::find_virtiofsd(env).ok_or_else(|| {
        err!("shared folders need virtiofsd, which is not installed (run: {} setup)", crate::env::roomy_command())
    })?;
    for (index, share) in config.shared_folders.iter().enumerate() {
        let log_path = folder.log_dir().join(format!("share-{}.log", share.name));
        start_fresh_log(&log_path);
        match shares::start_daemon(&virtiofsd, &sockets.share(index), &share.path, &log_path) {
            Ok(daemon) => daemons.push(daemon),
            Err(DaemonFailure::CannotRun(e)) => return Err(err!("cannot run {}: {e}", virtiofsd.display())),
            Err(DaemonFailure::DidNotStart) => {
                return Err(with_log(err!("shared folder '{}' could not be set up", share.name), &log_path));
            }
        }
    }
    Ok(daemons)
}

/// Settings for saving to and loading from a file: guest memory at fixed file
/// offsets, written by several threads, with no artificial speed limit.
fn migration_setup(qmp: &mut Qmp) -> std::result::Result<(), QmpError> {
    qmp.execute(
        "migrate-set-capabilities",
        Some(json!({ "capabilities": [
            { "capability": "mapped-ram", "state": true },
            { "capability": "multifd", "state": true },
        ]})),
    )?;
    qmp.execute("migrate-set-parameters", Some(json!({ "multifd-channels": 4, "max-bandwidth": 1u64 << 40 })))?;
    Ok(())
}

/// Waits for a save or load to finish. Large guests on slow disks take a
/// while, so the limit is generous; QEMU reports failures itself.
fn wait_migration(qmp: &mut Qmp) -> std::result::Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    loop {
        let progress = qmp.execute("query-migrate", None).map_err(|e| e.to_string())?;
        match progress["status"].as_str() {
            Some("completed") => return Ok(()),
            Some(status @ ("failed" | "cancelled")) => {
                return Err(progress["error-desc"].as_str().unwrap_or(status).to_owned());
            }
            _ if Instant::now() >= deadline => return Err("it did not finish within 30 minutes".to_owned()),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// Appends the end of a program's log to an error, when there is any.
fn with_log(error: crate::Error, log_path: &Path) -> crate::Error {
    let tail = read_tail(log_path, 15);
    if tail.trim().is_empty() { error } else { err!("{error}\n{tail}") }
}

/// Supervisor log line; ends up in the VM folder's `logs/supervisor.log`.
fn log(message: &str) {
    eprintln!("{} roomy supervisor: {message}", utc_timestamp_secs(SystemTime::now()));
}
