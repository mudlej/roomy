//! The VM engine against a fake host: creating VM folders and running the
//! lifecycle through a real supervisor and control socket, with stand-in
//! QEMU, qemu-img and virtiofsd programs (see `common` and `tests/fixtures`).

mod common;

use common::FakeHost;
use roomy_core::control::{self, Request, State};
use roomy_core::supervisor;
use roomy_core::vm::{self, Features, Folder, NewVm, SharedFolder};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

fn new_vm(name: &str) -> NewVm {
    NewVm {
        name: name.to_owned(),
        os: "archlinux".to_owned(),
        cpus: 2,
        memory_mb: 2048,
        disk_gb: 20,
        install_image: None,
        existing_disk: None,
        features: Features::default(),
        shared_folders: vec![],
        port_forwards: vec![],
    }
}

fn create(host: &FakeHost, new: NewVm) -> Folder {
    vm::create(&host.env(), &host.base.join("vms"), new).unwrap()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A supervisor running on a thread, as `roomy supervise` would run it in its own process.
struct Supervised {
    thread: JoinHandle<roomy_core::Result<()>>,
    stop: Arc<AtomicBool>,
}

impl Supervised {
    /// Waits for the supervisor to end and returns how it ended.
    fn join(self) -> roomy_core::Result<()> {
        wait_for("the supervisor to end", || self.thread.is_finished());
        self.thread.join().unwrap()
    }
}

fn supervise(host: &FakeHost, folder: &Folder) -> Supervised {
    let (env, dir) = (host.env(), folder.dir().to_owned());
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || supervisor::run(&env, &dir, None, &flag));
    Supervised { thread, stop }
}

/// Starts a supervisor and waits until the guest runs.
fn boot(host: &FakeHost, folder: &Folder) -> Supervised {
    let supervised = supervise(host, folder);
    wait_for("the guest to run", || {
        supervised.thread.is_finished() || control::status(&host.env(), folder) == State::Running
    });
    assert!(!supervised.thread.is_finished(), "the supervisor ended during startup");
    supervised
}

fn qmp_commands(host: &FakeHost) -> Vec<String> {
    host.qemu_log()
        .iter()
        .filter_map(|l| l.strip_prefix("qmp "))
        .map(|l| l.split(' ').next().unwrap().to_owned())
        .collect()
}

#[test]
fn creating_a_vm_makes_a_self_contained_folder() {
    let host = FakeHost::healthy("create");
    let folder = create(&host, new_vm("Arch Linux"));
    assert_eq!(folder.dir(), host.base.join("vms/Arch Linux").canonicalize().unwrap());
    let config = folder.load().unwrap();
    assert_eq!((config.name.as_str(), config.cpus, config.memory_mb), ("Arch Linux", 2, 2048));
    // The machine type is pinned to what this QEMU's "q35" means today.
    assert_eq!(config.machine, "pc-q35-11.1");
    assert_eq!(fs::read_to_string(folder.disk_path(&config)).unwrap(), format!("qcow2 {}", 20u64 << 30));
    assert_eq!(fs::read_to_string(folder.firmware_vars()).unwrap(), "vars");
    assert_eq!(control::status(&host.env(), &folder), State::Off);

    let again = vm::create(&host.env(), &host.base.join("vms"), new_vm("Arch Linux")).unwrap_err();
    assert!(again.message().contains("already exists"), "{again}");
}

#[test]
fn an_existing_disk_is_copied_and_only_ever_grown() {
    let host = FakeHost::healthy("fromdisk");
    let source = host.base.join("cloud.qcow2");
    fs::write(&source, format!("qcow2 {}", 2u64 << 30)).unwrap();
    let grown = create(&host, NewVm { existing_disk: Some(source.clone()), ..new_vm("grown") });
    assert_eq!(fs::read_to_string(grown.dir().join("disk.qcow2")).unwrap(), format!("qcow2 {}", 20u64 << 30));
    let kept = create(&host, NewVm { existing_disk: Some(source.clone()), disk_gb: 1, ..new_vm("kept") });
    assert_eq!(fs::read_to_string(kept.dir().join("disk.qcow2")).unwrap(), format!("qcow2 {}", 2u64 << 30));
    assert_eq!(fs::read_to_string(&source).unwrap(), format!("qcow2 {}", 2u64 << 30), "the source image is untouched");
}

#[test]
fn a_failed_creation_leaves_nothing_behind() {
    let host = FakeHost::healthy("createfail");
    host.configure_qemu_img(r#"{"fail": "No space left on device"}"#);
    let error = vm::create(&host.env(), &host.base.join("vms"), new_vm("doomed")).unwrap_err();
    assert!(error.message().contains("No space left on device"), "{error}");
    assert!(!host.base.join("vms/doomed").exists());
}

#[test]
fn pause_resume_and_graceful_shutdown() {
    let host = FakeHost::healthy("life");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);

    assert_eq!(control::request(&env, &folder, Request::Pause).unwrap().state, State::Paused);
    assert_eq!(control::status(&env, &folder), State::Paused);
    // Asking again is not an error.
    assert_eq!(control::request(&env, &folder, Request::Pause).unwrap().state, State::Paused);
    let resumed = control::request(&env, &folder, Request::Resume).unwrap();
    assert_eq!(resumed.state, State::Running);
    assert!(resumed.note.unwrap().contains("guest agent"), "without an agent the user is told about the clock");

    assert_eq!(control::request(&env, &folder, Request::Shutdown).unwrap().state, State::Stopping);
    running.join().unwrap();
    assert_eq!(control::status(&env, &folder), State::Off);
    assert_eq!(qmp_commands(&host), ["cont", "stop", "cont", "system_powerdown"]);
    assert!(!control::runtime_dir(&env, &folder).exists(), "sockets are cleaned up");
    assert!(control::request(&env, &folder, Request::Pause).unwrap_err().message().contains("not running"));
}

#[test]
fn suspend_saves_to_disk_and_start_continues_exactly_once() {
    let host = FakeHost::healthy("suspend");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);

    assert_eq!(control::request(&env, &folder, Request::Suspend).unwrap().state, State::Suspended);
    running.join().unwrap();
    assert_eq!(control::status(&env, &folder), State::Suspended);
    assert!(folder.saved_state().is_file() && folder.saved_config().is_file());
    assert!(!folder.saved_state_partial().exists());
    // Settings changed while suspended must not alter the hardware the saved guest expects.
    let mut edited = folder.load().unwrap();
    edited.cpus = 8;
    folder.save(&edited).unwrap();

    let restored = boot(&host, &folder);
    assert!(!folder.is_suspended(), "a loaded state can never be loaded again");
    assert!(!folder.saved_state_consumed().exists() && !folder.saved_config().exists());
    let starts: Vec<String> = host.qemu_log().into_iter().filter(|l| l.starts_with("start ")).collect();
    assert_eq!(starts.len(), 2);
    assert!(!starts[0].contains("-incoming") && starts[1].ends_with("-incoming defer"));
    assert!(starts[1].contains("-smp 2 "), "restored with the suspended guest's settings: {}", starts[1]);
    assert_eq!(
        qmp_commands(&host),
        [
            "cont",
            "stop",
            "migrate-set-capabilities",
            "migrate-set-parameters",
            "migrate",
            "quit",
            "migrate-set-capabilities",
            "migrate-set-parameters",
            "migrate-incoming",
            "cont",
        ]
    );

    assert_eq!(control::request(&env, &folder, Request::ForceOff).unwrap().state, State::Off);
    restored.join().unwrap();
    assert_eq!(control::status(&env, &folder), State::Off);
}

#[test]
fn a_failed_suspend_keeps_the_guest_running_and_saves_nothing() {
    let host = FakeHost::healthy("savefail");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);
    host.configure_qemu(r#"{"migrate_fail": "No space left on device"}"#);

    let error = control::request(&env, &folder, Request::Suspend).unwrap_err();
    assert!(
        error.message().contains("keeps running") && error.message().contains("No space left on device"),
        "{error}"
    );
    assert_eq!(control::status(&env, &folder), State::Running);
    assert!(!folder.is_suspended() && !folder.saved_state_partial().exists() && !folder.saved_config().exists());
    assert_eq!(qmp_commands(&host).last().unwrap(), "cont");

    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn a_state_that_cannot_be_loaded_is_kept_until_the_user_discards_it() {
    let host = FakeHost::healthy("loadfail");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);
    control::request(&env, &folder, Request::Suspend).unwrap();
    running.join().unwrap();

    host.configure_qemu(r#"{"incoming_fail": "Machine type received is 'pc-q35-11.1' and local is 'pc-q35-12.0'"}"#);
    let error = supervise(&host, &folder).join().unwrap_err();
    assert!(error.message().contains("saved state could not be loaded"), "{error}");
    assert!(error.message().contains("pc-q35-12.0"), "{error}");
    assert_eq!(control::status(&env, &folder), State::Suspended, "the state is still there for another try");

    folder.discard_saved_state().unwrap();
    assert_eq!(control::status(&env, &folder), State::Off);
    host.configure_qemu("{}");
    let booted = boot(&host, &folder);
    assert!(!host.qemu_log().last().unwrap().contains("migrate-incoming"));
    control::request(&env, &folder, Request::ForceOff).unwrap();
    booted.join().unwrap();
}

#[test]
fn suspend_is_refused_with_gpu_acceleration_and_says_why() {
    let host = FakeHost::healthy("gpu");
    let env = host.env();
    let mut new = new_vm("vm");
    new.features.gpu_acceleration = true;
    let folder = create(&host, new);
    let running = boot(&host, &folder);
    let error = control::request(&env, &folder, Request::Suspend).unwrap_err();
    assert!(error.message().starts_with("Suspend is not available while GPU acceleration is on"), "{error}");
    assert_eq!(control::status(&env, &folder), State::Running);
    assert_eq!(qmp_commands(&host), ["cont"], "the guest was never even stopped");
    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn the_network_switch_is_applied_before_the_guest_runs() {
    let host = FakeHost::healthy("net");
    let mut new = new_vm("vm");
    new.features.network = false;
    let folder = create(&host, new);
    let running = boot(&host, &folder);
    let log = host.qemu_log();
    assert_eq!(log[1], r#"qmp set_link {"name": "nic0", "up": false}"#);
    assert!(log[2].starts_with("qmp cont"));
    control::request(&host.env(), &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn a_vm_cannot_be_run_twice() {
    let host = FakeHost::healthy("twice");
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);
    let second = supervise(&host, &folder).join().unwrap_err();
    assert_eq!(second.message(), "the virtual machine is already running");
    assert_eq!(control::status(&host.env(), &folder), State::Running, "the first one is unaffected");
    control::request(&host.env(), &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn qemus_own_error_is_reported_when_it_will_not_start() {
    let host = FakeHost::healthy("nostart");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    host.configure_qemu(r#"{"reject": ["memory-backend-memfd"]}"#);
    let error = supervise(&host, &folder).join().unwrap_err();
    assert!(error.message().contains("QEMU did not start"), "{error}");
    assert!(error.message().contains("fake refusal of 'memory-backend-memfd'"), "{error}");
    assert_eq!(control::status(&env, &folder), State::Off);
    assert!(!control::runtime_dir(&env, &folder).exists());
    // Nothing is left locked: once the cause is fixed it starts.
    host.configure_qemu("{}");
    let running = boot(&host, &folder);
    // The log of the run that went wrong is still there after the next start.
    let previous = std::fs::read_to_string(folder.log_dir().join("qemu.log.1")).unwrap();
    assert!(previous.contains("fake refusal"), "{previous}");
    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn leftovers_of_an_interrupted_suspend_or_restore_are_cleared_on_start() {
    let host = FakeHost::healthy("leftover");
    let folder = create(&host, new_vm("vm"));
    fs::write(folder.saved_state_partial(), "half a state").unwrap();
    fs::write(folder.saved_state_consumed(), "a state that was already loaded").unwrap();
    fs::write(folder.saved_config(), "stale").unwrap();
    assert_eq!(control::status(&host.env(), &folder), State::Off, "neither leftover counts as suspended");
    let running = boot(&host, &folder);
    assert!(
        !folder.saved_state_partial().exists()
            && !folder.saved_state_consumed().exists()
            && !folder.saved_config().exists()
    );
    assert!(!host.qemu_log()[0].contains("-incoming"), "it boots normally");
    control::request(&host.env(), &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn the_guest_clock_is_corrected_on_resume_when_an_agent_is_listening() {
    let host = FakeHost::healthy("clock");
    let env = host.env();
    host.configure_qemu(r#"{"agent": true}"#);
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);
    control::request(&env, &folder, Request::Pause).unwrap();
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    let resumed = control::request(&env, &folder, Request::Resume).unwrap();
    assert_eq!(resumed.note, None, "nothing to tell the user when the clock was corrected");

    let agent: Vec<String> =
        host.qemu_log().into_iter().filter_map(|l| l.strip_prefix("agent ").map(str::to_owned)).collect();
    assert_eq!(agent.len(), 1, "exactly one request: {agent:?}");
    let request: serde_json::Value = serde_json::from_str(&agent[0]).unwrap();
    assert_eq!(request["execute"], "guest-set-time");
    let sent = request["arguments"]["time"].as_u64().expect("an explicit time, never the guest's hardware clock");
    assert!(sent >= before && sent < before + 10_000_000_000, "{sent} vs {before}");

    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn nothing_is_sent_to_a_guest_without_an_agent() {
    let host = FakeHost::healthy("noagent");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);
    control::request(&env, &folder, Request::Pause).unwrap();
    control::request(&env, &folder, Request::Resume).unwrap();
    assert!(
        !host.qemu_log().iter().any(|l| l.starts_with("agent ")),
        "a queued time request would run later with a stale time"
    );
    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn a_guest_paused_by_qemu_itself_is_reported_with_the_reason() {
    let host = FakeHost::healthy("ioerror");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);
    host.configure_qemu(r#"{"status": "io-error"}"#);
    wait_for("the supervisor to notice", || control::status(&env, &folder) == State::Paused);
    let reply = control::request(&env, &folder, Request::Status).unwrap();
    assert_eq!(reply.note.as_deref(), Some("QEMU paused the guest: io-error"));
    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn shared_folders_get_one_daemon_each_and_a_missing_folder_blocks_the_start() {
    let host = FakeHost::healthy("shares");
    let env = host.env();
    let (docs, music) = (host.base.join("docs"), host.base.join("music"));
    fs::create_dir_all(&docs).unwrap();
    fs::create_dir_all(&music).unwrap();
    let mut new = new_vm("vm");
    new.shared_folders = vec![
        SharedFolder { name: "docs".into(), path: docs.clone() },
        SharedFolder { name: "music".into(), path: music.clone() },
    ];
    let folder = create(&host, new);

    let running = boot(&host, &folder);
    let start = host.qemu_log()[0].clone();
    assert!(start.contains("tag=docs") && start.contains("tag=music"), "{start}");
    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();

    fs::remove_dir(&music).unwrap();
    let error = supervise(&host, &folder).join().unwrap_err();
    assert!(error.message().contains("shared folder 'music'") && error.message().contains("not a folder"), "{error}");
}

#[test]
fn a_stop_signal_shuts_the_guest_down_gracefully() {
    let host = FakeHost::healthy("sigterm");
    let folder = create(&host, new_vm("vm"));
    let running = boot(&host, &folder);
    running.stop.store(true, Ordering::Relaxed);
    running.join().unwrap();
    assert_eq!(qmp_commands(&host), ["cont", "system_powerdown"]);
    assert_eq!(control::status(&host.env(), &folder), State::Off);
}

#[test]
fn a_missing_installation_image_does_not_block_a_normal_boot() {
    let host = FakeHost::healthy("iso");
    let iso = host.base.join("installer.iso");
    fs::write(&iso, "iso").unwrap();
    let folder = create(&host, NewVm { install_image: Some(iso.clone()), ..new_vm("vm") });
    fs::remove_file(&iso).unwrap();
    let running = boot(&host, &folder);
    assert!(!host.qemu_log()[0].contains("install0"), "started without the deleted image");
    assert_eq!(folder.load().unwrap().install_image, Some(iso), "the setting itself is left for the user to change");
    control::request(&host.env(), &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn a_window_gets_its_own_connection_to_the_display() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;

    let host = FakeHost::healthy("display");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    assert!(control::open_display(&env, &folder).is_err(), "a VM that is off has no display");

    let running = boot(&host, &folder);
    // Twice: a window that is closed and opened again gets a fresh connection.
    for _ in 0..2 {
        let connection = UnixStream::from(control::open_display(&env, &folder).unwrap());
        let mut greeting = String::new();
        BufReader::new(connection).read_line(&mut greeting).unwrap();
        assert_eq!(greeting, "FAKE DISPLAY @dbus-display\n", "QEMU is on the other end of the socket");
    }
    assert_eq!(control::status(&env, &folder), State::Running);

    // QEMU's refusal is reported, and nothing is handed over.
    host.configure_qemu(r#"{"display_fail": "no dbus display"}"#);
    let error = control::open_display(&env, &folder).unwrap_err();
    assert!(error.message().contains("no dbus display"), "{error}");
    assert!(qmp_commands(&host).iter().any(|c| c.starts_with("closefd")), "the unused descriptor is closed in QEMU");

    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
}

#[test]
fn a_running_vm_records_what_it_booted_with_and_what_is_waiting() {
    use roomy_core::settings::{self, Applies, Changes};

    let host = FakeHost::healthy("running");
    let env = host.env();
    let folder = create(&host, new_vm("vm"));
    host.configure_qemu(r#"{"agent": true}"#);
    assert_eq!(control::running(&env, &folder), None, "nothing is recorded for a VM that is off");
    assert!(settings::waiting_for_next_boot(&env, &folder).is_empty());

    let running = boot(&host, &folder);
    let booted = control::running(&env, &folder).expect("the supervisor records what it started");
    assert_eq!(booted.config, folder.load().unwrap());
    assert_eq!(booted.render_node, None, "no graphics card is involved without GPU acceleration");

    // Hardware changes wait; the settings file and the running guest now differ.
    let changes = Changes { cpus: Some(8), network: Some(false), ..Changes::default() };
    let applied = settings::apply(&env, &folder, &changes).unwrap();
    let when: Vec<(&str, Applies)> = applied.iter().map(|a| (a.setting, a.applies)).collect();
    assert_eq!(when, [("processors", Applies::NextBoot), ("network", Applies::Now)]);
    assert_eq!(settings::waiting_for_next_boot(&env, &folder), ["processors"]);
    assert!(!control::running(&env, &folder).unwrap().config.features.network, "the cable was pulled at once");

    // The guest's agent is noticed within a few seconds of starting to listen.
    wait_for("the agent to be noticed", || control::send(&env, &folder, Request::Status).unwrap().guest_agent);

    control::request(&env, &folder, Request::ForceOff).unwrap();
    running.join().unwrap();
    assert!(settings::waiting_for_next_boot(&env, &folder).is_empty(), "once it is off nothing is waiting");
}
