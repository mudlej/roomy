//! The display client against a stand-in for QEMU's display: a real D-Bus
//! conversation over a real socket pair, including descriptor passing.

use roomy_display::pixels::{Canvas, Rect};
use roomy_display::{Button, Display, Event, Options, keymap};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

struct MockQemu {
    child: Child,
    log: PathBuf,
}

impl MockQemu {
    /// Starts the stand-in and returns it with our end of the display socket.
    fn start(name: &str) -> (MockQemu, OwnedFd) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let log = std::env::temp_dir().join(format!("roomy-display-{name}-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let theirs_fd = theirs.as_raw_fd();
        let mut command = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock-qemu-display"));
        command.arg("3").arg(&log);
        // SAFETY: dup2 is async-signal-safe; it hands the child its socket as descriptor 3.
        unsafe {
            command.pre_exec(
                move || if libc::dup2(theirs_fd, 3) == 3 { Ok(()) } else { Err(std::io::Error::last_os_error()) },
            );
        }
        let child = command.spawn().unwrap();
        (MockQemu { child, log }, ours.into())
    }

    fn log(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    fn wait_for_log(&self, line: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.log().iter().any(|l| l == line) {
            assert!(Instant::now() < deadline, "'{line}' never arrived; QEMU saw:\n{}", self.log().join("\n"));
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for MockQemu {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.log);
    }
}

fn next(events: &Receiver<Event>) -> Event {
    events.recv_timeout(Duration::from_secs(10)).expect("an event from the display")
}

fn connect(mock_name: &str) -> (MockQemu, Display, Receiver<Event>) {
    connect_with(mock_name, Options::default())
}

fn connect_with(mock_name: &str, options: Options) -> (MockQemu, Display, Receiver<Event>) {
    let (mock, socket) = MockQemu::start(mock_name);
    let (tx, events) = channel();
    let display = Display::connect(socket, options, move |event| {
        let _ = tx.send(event);
    })
    .unwrap();
    (mock, display, events)
}

#[test]
fn the_screen_arrives_in_every_form_qemu_uses() {
    let (mock, display, events) = connect("screen");
    assert_eq!(display.info().label, "mock screen");
    assert_eq!((display.info().width, display.info().height, display.info().pointer_absolute), (4, 2, true));

    // Pixels sent inline: a whole screen, then part of it.
    let mut canvas = Canvas::default();
    match next(&events) {
        Event::Scanout { width, height, stride, format, data } => {
            canvas.set(width as usize, height as usize, stride as usize, format, &data).unwrap();
        }
        other => panic!("expected a screen, got {other:?}"),
    }
    assert_eq!(canvas.pixels(), (0..32).collect::<Vec<u8>>());
    match next(&events) {
        Event::Update { x, y, width, height, stride, format, data } => {
            let area = Rect { x: x as usize, y: y as usize, width: width as usize, height: height as usize };
            canvas.draw(area, stride as usize, format, &data).unwrap();
        }
        other => panic!("expected an update, got {other:?}"),
    }
    assert_eq!(&canvas.pixels()[..16], &[0, 1, 2, 3, 200, 200, 200, 200, 200, 200, 200, 200, 12, 13, 14, 15]);

    // Shared memory: the descriptor QEMU passed is mapped at its offset.
    let map = match next(&events) {
        Event::ScanoutMap(map) => map,
        other => panic!("expected a shared screen, got {other:?}"),
    };
    canvas.draw_mapped(&map, None).unwrap();
    assert_eq!((canvas.width(), canvas.height()), (2, 2));
    assert_eq!(canvas.pixels(), (0..16).collect::<Vec<u8>>());
    assert!(matches!(next(&events), Event::UpdateMap { x: 0, y: 1, width: 2, height: 1 }));
    // A frame larger than its memory is reported, never mapped.
    match next(&events) {
        Event::Problem(problem) => assert!(problem.contains("ScanoutMap") && problem.contains("smaller"), "{problem}"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Graphics memory: the descriptor arrives usable, with its description.
    match next(&events) {
        Event::ScanoutDmabuf(dmabuf) => {
            assert_eq!((dmabuf.width, dmabuf.height, dmabuf.stride), (640, 480, 2560));
            assert_eq!((dmabuf.fourcc, dmabuf.modifier, dmabuf.y0_top), (0x3432_4258, 0x00ff_ffff_ffff_ffff, true));
            assert!(dmabuf.stored_bottom_up(), "QEMU's flag set means the picture is stored bottom first");
            let mut content = String::new();
            use std::io::{Read, Seek};
            let mut file = std::fs::File::from(dmabuf.fd);
            file.rewind().unwrap();
            file.read_to_string(&mut content).unwrap();
            assert_eq!(content, "dmabuf");
        }
        other => panic!("expected a graphics-memory screen, got {other:?}"),
    }
    assert!(matches!(next(&events), Event::UpdateDmabuf { x: 0, y: 0, width: 640, height: 480 }));
    match next(&events) {
        Event::CursorDefine { width: 1, height: 1, hot_x: 0, hot_y: 0, data } => assert_eq!(data, [1, 2, 3, 4]),
        other => panic!("expected a pointer image, got {other:?}"),
    }
    assert!(matches!(next(&events), Event::MouseSet { x: 5, y: 6, on: true }));
    // The last two travel on different connections, so their order is not fixed.
    let last = [next(&events), next(&events)];
    assert!(last.iter().any(|e| matches!(e, Event::PointerMode { absolute: false })), "{last:?}");
    assert!(last.iter().any(|e| matches!(e, Event::Disable)), "{last:?}");

    // QEMU got an answer to every call, including the one that was refused.
    mock.wait_for_log("listener-answered Disable");
    let log = mock.log();
    assert!(log.contains(&"listener-interfaces org.qemu.Display1.Listener.Unix.Map".to_owned()), "{log:?}");
    assert!(!log.iter().any(|l| l.starts_with("listener-failed")), "{log:?}");
    assert_eq!(log.iter().filter(|l| l.starts_with("listener-answered")).count(), 10, "{log:?}");
}

#[test]
fn keys_and_pointer_reach_the_guest() {
    let (mock, display, _events) = connect("input");
    let enter = keymap::qnum_from_linux(28).unwrap();
    display.key(enter, true);
    display.key(enter, false);
    display.pointer_position(3, 1);
    display.pointer_motion(-2, 7);
    display.button(Button::Right, true);
    display.button(Button::Right, false);
    display.set_window_size(1280, 800, 340, 210);
    mock.wait_for_log("Console.SetUIInfo 340 210 0 0 1280 800");
    let log = mock.log();
    let sent: Vec<&str> =
        log.iter().map(String::as_str).filter(|l| l.starts_with("Keyboard.") || l.starts_with("Mouse.")).collect();
    assert_eq!(
        sent,
        [
            "Keyboard.Press 28",
            "Keyboard.Release 28",
            "Mouse.SetAbsPosition 3 1",
            "Mouse.RelMotion -2 7",
            "Mouse.Press 2",
            "Mouse.Release 2"
        ]
    );
}

#[test]
fn a_vm_that_goes_away_is_reported_once_and_hanging_up_ourselves_is_not() {
    let (mut mock, display, events) = connect("closed");
    mock.wait_for_log("listener-answered Disable");
    mock.child.kill().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the closed connection was never reported");
        if matches!(events.recv_timeout(Duration::from_secs(10)), Ok(Event::Closed)) {
            break;
        }
    }
    display.key(1, true); // Input after the end is dropped quietly.
    drop(display);
    assert!(!events.try_iter().any(|e| matches!(e, Event::Closed)), "reported only once");

    let (_mock, display, events) = connect("hangup");
    drop(display);
    assert!(!events.try_iter().any(|e| matches!(e, Event::Closed)), "our own disconnect is not an event");
}

#[test]
fn something_that_is_not_qemu_is_an_error_not_a_hang() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    drop(theirs);
    let error =
        Display::connect(ours.into(), Options::default(), |_| {}).err().expect("no display behind a closed socket");
    assert!(error.contains("cannot connect to the virtual machine's display"), "{error}");
}

/// Writes down everything QEMU does with the guest's sound.
#[derive(Default)]
struct RecordingSound(std::sync::Mutex<Vec<String>>);

impl roomy_display::audio::AudioHost for RecordingSound {
    fn playback_start(&self, id: u64, format: roomy_display::audio::Format) {
        self.0.lock().unwrap().push(format!("start {id} {}bit {}Hz {}ch", format.bits, format.rate, format.channels));
    }
    fn playback_data(&self, id: u64, data: Vec<u8>) {
        self.0.lock().unwrap().push(format!("data {id} {data:?}"));
    }
    fn playback_volume(&self, id: u64, muted: bool, volume: Vec<u8>) {
        self.0.lock().unwrap().push(format!("volume {id} {muted} {volume:?}"));
    }
    fn playback_enabled(&self, id: u64, enabled: bool) {
        self.0.lock().unwrap().push(format!("enabled {id} {enabled}"));
    }
    fn playback_stop(&self, id: u64) {
        self.0.lock().unwrap().push(format!("stop {id}"));
    }
    fn capture_start(&self, id: u64, format: roomy_display::audio::Format) {
        self.0.lock().unwrap().push(format!("mic start {id} {}Hz", format.rate));
    }
    fn capture_enabled(&self, id: u64, enabled: bool) {
        self.0.lock().unwrap().push(format!("mic enabled {id} {enabled}"));
    }
    fn capture_read(&self, id: u64, size: usize) -> Vec<u8> {
        self.0.lock().unwrap().push(format!("mic read {id} {size}"));
        // More than was asked for: only the asked-for amount may reach the guest.
        vec![42; size + 10]
    }
    fn capture_stop(&self, id: u64) {
        self.0.lock().unwrap().push(format!("mic stop {id}"));
    }
}

#[test]
fn the_guests_sound_is_passed_on_and_its_microphone_is_answered() {
    let sound = std::sync::Arc::new(RecordingSound::default());
    let options = Options { audio: Some(sound.clone()), clipboard: false };
    let (mock, _display, _events) = connect_with("sound", options);
    mock.wait_for_log("sound-answered AudioOutListener.Fini");
    mock.wait_for_log("sound-answered AudioInListener.Fini");
    let log = mock.log();
    assert!(!log.iter().any(|l| l.starts_with("sound-failed")), "{log:?}");
    assert!(log.contains(&"sound-read 6 bytes, first 42".to_owned()), "{log:?}");

    let heard = sound.0.lock().unwrap().clone();
    let playback: Vec<&str> = heard.iter().map(String::as_str).filter(|l| !l.starts_with("mic")).collect();
    assert_eq!(
        playback,
        [
            "start 7 16bit 48000Hz 2ch",
            "enabled 7 true",
            "volume 7 false [255, 128]",
            "data 7 [0, 0, 0, 0, 0, 0, 0, 0]",
            "data 7 [1, 1, 1, 1, 1, 1, 1, 1]",
            "data 7 [2, 2, 2, 2, 2, 2, 2, 2]",
            "stop 7"
        ]
    );
    let microphone: Vec<&str> = heard.iter().map(String::as_str).filter(|l| l.starts_with("mic")).collect();
    assert_eq!(microphone, ["mic start 9 48000Hz", "mic enabled 9 true", "mic read 9 6", "mic stop 9"]);
}

#[test]
fn text_is_copied_between_the_guest_and_this_computer() {
    let options = Options { audio: None, clipboard: true };
    let (mock, display, events) = connect_with("clipboard", options);
    // The guest copies: we are told, and can fetch the text.
    let offered = loop {
        match next(&events) {
            Event::ClipboardOffered { mimes } => break mimes,
            _ => continue,
        }
    };
    assert_eq!(offered, ["text/plain;charset=utf-8"]);
    let context = glib::MainContext::new();
    let fetched = std::rc::Rc::new(std::cell::RefCell::new(None));
    context
        .with_thread_default(|| {
            let slot = fetched.clone();
            display.clipboard_request(move |text| *slot.borrow_mut() = Some(text));
            let deadline = Instant::now() + Duration::from_secs(10);
            while fetched.borrow().is_none() && Instant::now() < deadline {
                context.iteration(false);
                std::thread::sleep(Duration::from_millis(5));
            }
        })
        .unwrap();
    assert_eq!(fetched.borrow().clone(), Some(Ok("copied in the guest".to_owned())));

    // This computer copies: the guest is told, and gets the text when it pastes, without us being asked again.
    // The guest's own copy was grab 0, so this one must be numbered 1 or the guest throws it away.
    display.clipboard_offer(Some("copied on the host"));
    mock.wait_for_log("clipboard-pasted text/plain;charset=utf-8 copied on the host");
    assert!(mock.log().contains(&"Clipboard.Grab 0 1 ['text/plain;charset=utf-8']".to_owned()), "{:?}", mock.log());
    // The guest's helper restarts, which starts the numbering over and makes the guest forget the
    // offer: it is made again, numbered 0, and a paste still finds the text.
    mock.wait_for_log("clipboard-pasted-after-restart text/plain;charset=utf-8 copied on the host");
    assert!(mock.log().contains(&"Clipboard.Grab 0 0 ['text/plain;charset=utf-8']".to_owned()), "{:?}", mock.log());
    // Withdrawn: a paste in the guest now finds nothing.
    display.clipboard_offer(None);
    mock.wait_for_log("clipboard-nothing GDBus.Error:org.qemu.Display1.Error.Failed: nothing to paste");
    // The guest copies once more, with the next number: accepted, not mistaken for a stale grab.
    mock.wait_for_log("clipboard-announced 1");
    loop {
        if let Event::ClipboardOffered { .. } = next(&events) {
            break;
        }
    }
    assert!(!mock.log().iter().any(|line| line.contains("clipboard-discarded")), "{:?}", mock.log());
}
