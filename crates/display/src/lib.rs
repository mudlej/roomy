//! Client for a running VM's screen, keyboard and pointer.
//!
//! QEMU's D-Bus display protocol, spoken privately over a socket that the
//! VM's supervisor hands out (`roomy_core::control::open_display`). There is
//! no message bus involved.
//!
//! QEMU calls into its display listener and, for some calls, waits for the
//! answer. A client that answers late stalls the whole VM, so the protocol
//! runs on a thread of its own and never waits for the window: screen
//! changes are passed on as [`Event`]s, and input is sent without waiting
//! for a reply.

mod args;
pub mod audio;
pub mod clipboard;
pub mod keymap;
pub mod pixels;
pub mod pulse;
pub mod view;

use args::{arg, bytes};
use gio::prelude::*;
use glib::variant::Handle;
use pixels::Mapping;
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

const CONSOLE: &str = "/org/qemu/Display1/Console_0";
const LISTENER_PATH: &str = "/org/qemu/Display1/Listener";
const CONSOLE_IFACE: &str = "org.qemu.Display1.Console";
const KEYBOARD_IFACE: &str = "org.qemu.Display1.Keyboard";
const MOUSE_IFACE: &str = "org.qemu.Display1.Mouse";
const LISTENER_IFACE: &str = "org.qemu.Display1.Listener";
const MAP_IFACE: &str = "org.qemu.Display1.Listener.Unix.Map";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
pub(crate) const CALL_TIMEOUT_MS: i32 = 5_000;

/// Signatures as in QEMU's `ui/dbus-display1.xml`.
const LISTENER_XML: &str = r#"
<node>
  <interface name="org.qemu.Display1.Listener">
    <method name="Scanout">
      <arg type="u" name="width" direction="in"/><arg type="u" name="height" direction="in"/>
      <arg type="u" name="stride" direction="in"/><arg type="u" name="pixman_format" direction="in"/>
      <arg type="ay" name="data" direction="in"/>
    </method>
    <method name="Update">
      <arg type="i" name="x" direction="in"/><arg type="i" name="y" direction="in"/>
      <arg type="i" name="width" direction="in"/><arg type="i" name="height" direction="in"/>
      <arg type="u" name="stride" direction="in"/><arg type="u" name="pixman_format" direction="in"/>
      <arg type="ay" name="data" direction="in"/>
    </method>
    <method name="ScanoutDMABUF">
      <arg type="h" name="dmabuf" direction="in"/>
      <arg type="u" name="width" direction="in"/><arg type="u" name="height" direction="in"/>
      <arg type="u" name="stride" direction="in"/><arg type="u" name="fourcc" direction="in"/>
      <arg type="t" name="modifier" direction="in"/><arg type="b" name="y0_top" direction="in"/>
    </method>
    <method name="UpdateDMABUF">
      <arg type="i" name="x" direction="in"/><arg type="i" name="y" direction="in"/>
      <arg type="i" name="width" direction="in"/><arg type="i" name="height" direction="in"/>
    </method>
    <method name="Disable"/>
    <method name="MouseSet">
      <arg type="i" name="x" direction="in"/><arg type="i" name="y" direction="in"/>
      <arg type="i" name="on" direction="in"/>
    </method>
    <method name="CursorDefine">
      <arg type="i" name="width" direction="in"/><arg type="i" name="height" direction="in"/>
      <arg type="i" name="hot_x" direction="in"/><arg type="i" name="hot_y" direction="in"/>
      <arg type="ay" name="data" direction="in"/>
    </method>
    <property name="Interfaces" type="as" access="read"/>
  </interface>
  <interface name="org.qemu.Display1.Listener.Unix.Map">
    <method name="ScanoutMap">
      <arg type="h" name="handle" direction="in"/><arg type="u" name="offset" direction="in"/>
      <arg type="u" name="width" direction="in"/><arg type="u" name="height" direction="in"/>
      <arg type="u" name="stride" direction="in"/><arg type="u" name="pixman_format" direction="in"/>
    </method>
    <method name="UpdateMap">
      <arg type="i" name="x" direction="in"/><arg type="i" name="y" direction="in"/>
      <arg type="i" name="width" direction="in"/><arg type="i" name="height" direction="in"/>
    </method>
  </interface>
</node>
"#;

/// A screen that lives in graphics memory (GPU-accelerated guests).
#[derive(Debug)]
pub struct Dmabuf {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub fourcc: u32,
    pub modifier: u64,
    /// The first row in memory is the bottom of the picture as the guest shows
    /// it, so the picture must be turned over to be drawn. This is QEMU's
    /// `y0_top` flag, which speaks in OpenGL's bottom-up terms.
    pub stored_bottom_up: bool,
}

/// Something the guest's display did. Delivered on the display thread.
#[derive(Debug)]
pub enum Event {
    /// A whole new screen, pixels included.
    Scanout { width: u32, height: u32, stride: u32, format: u32, data: Vec<u8> },
    /// New pixels for part of the screen.
    Update { x: i32, y: i32, width: i32, height: i32, stride: u32, format: u32, data: Vec<u8> },
    /// A whole new screen in memory shared with QEMU.
    ScanoutMap(Arc<Mapping>),
    /// Part of the shared screen changed.
    UpdateMap { x: i32, y: i32, width: i32, height: i32 },
    /// A whole new screen in graphics memory.
    ScanoutDmabuf(Dmabuf),
    /// Part of the graphics-memory screen changed.
    UpdateDmabuf { x: i32, y: i32, width: i32, height: i32 },
    /// The guest turned its screen off.
    Disable,
    /// Where the guest draws its pointer, and whether it is visible.
    MouseSet { x: i32, y: i32, on: bool },
    /// The guest's pointer image: `width * height` premultiplied BGRA pixels.
    CursorDefine { width: i32, height: i32, hot_x: i32, hot_y: i32, data: Vec<u8> },
    /// Whether the guest's pointer takes absolute positions (a tablet) or only movement (a mouse).
    PointerMode { absolute: bool },
    /// The guest copied something; `mimes` are the kinds of content on offer.
    /// Fetch it with [`Display::clipboard_request`].
    ClipboardOffered { mimes: Vec<String> },
    /// The guest's clipboard is empty again.
    ClipboardReleased,
    /// The guest sent something roomy could not use; the screen may be stale.
    Problem(String),
    /// The connection ended: the VM stopped, was suspended, or another window took over.
    Closed,
}

/// What QEMU reports about the guest's screen at connection time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsoleInfo {
    pub label: String,
    pub width: u32,
    pub height: u32,
    /// Whether the pointer takes absolute positions.
    pub pointer_absolute: bool,
}

/// Pointer buttons, numbered as QEMU's protocol does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
    WheelUp = 3,
    WheelDown = 4,
    Side = 5,
    Extra = 6,
    WheelLeft = 7,
    WheelRight = 8,
}

pub(crate) type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// What a connection should carry besides the screen, keyboard and pointer.
#[derive(Default, Clone)]
pub struct Options {
    /// Where the guest's sound goes and its microphone sound comes from; `None` for a silent connection.
    pub audio: Option<Arc<dyn audio::AudioHost>>,
    /// Share the text clipboard with the guest.
    pub clipboard: bool,
}

/// A live connection to a VM's display. Dropping it disconnects.
pub struct Display {
    clipboard: clipboard::Shared,
    sink: EventSink,
    connection: gio::DBusConnection,
    main_loop: glib::MainLoop,
    thread: Option<std::thread::JoinHandle<()>>,
    closing: Arc<AtomicBool>,
    info: ConsoleInfo,
}

impl Display {
    /// Connects over `socket` and starts listening to the guest's screen.
    /// `on_event` is called on the display thread and must return quickly.
    pub fn connect(
        socket: OwnedFd,
        options: Options,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<Display, String> {
        let sink: EventSink = Arc::new(on_event);
        let clipboard = clipboard::Shared::default();
        let closing = Arc::new(AtomicBool::new(false));
        let cancellable = gio::Cancellable::new();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = {
            let (cancellable, closing, clipboard) = (cancellable.clone(), closing.clone(), clipboard.clone());
            let sink = sink.clone();
            std::thread::Builder::new()
                .name("roomy-display".into())
                .spawn(move || display_thread(socket, options, clipboard, sink, closing, cancellable, ready_tx))
                .map_err(|e| format!("cannot start the display thread: {e}"))?
        };
        let outcome = match ready_rx.recv_timeout(Duration::from_secs(15)) {
            Ok(outcome) => outcome,
            Err(_) => {
                cancellable.cancel();
                Err("the virtual machine did not answer on its display connection".to_owned())
            }
        };
        match outcome {
            Ok((connection, main_loop, info)) => {
                Ok(Display { clipboard, sink, connection, main_loop, thread: Some(thread), closing, info })
            }
            Err(e) => {
                let _ = thread.join();
                Err(e)
            }
        }
    }

    /// The guest's screen as it was when the connection was made.
    pub fn info(&self) -> &ConsoleInfo {
        &self.info
    }

    /// Presses or releases a key, given its QEMU key number (see [`keymap`]).
    pub fn key(&self, qnum: u32, pressed: bool) {
        self.send(CONSOLE, KEYBOARD_IFACE, if pressed { "Press" } else { "Release" }, &(qnum,).to_variant());
    }

    /// Moves the pointer to a position in guest pixels (absolute pointers only).
    pub fn pointer_position(&self, x: u32, y: u32) {
        self.send(CONSOLE, MOUSE_IFACE, "SetAbsPosition", &(x, y).to_variant());
    }

    /// Moves the pointer by a distance (relative pointers only).
    pub fn pointer_motion(&self, dx: i32, dy: i32) {
        self.send(CONSOLE, MOUSE_IFACE, "RelMotion", &(dx, dy).to_variant());
    }

    pub fn button(&self, button: Button, pressed: bool) {
        let method = if pressed { "Press" } else { "Release" };
        self.send(CONSOLE, MOUSE_IFACE, method, &(button as u32,).to_variant());
    }

    /// Tells the guest how large the window's drawing area is, in pixels and
    /// millimetres, so a guest with display integration can match it.
    pub fn set_window_size(&self, width: u32, height: u32, width_mm: u16, height_mm: u16) {
        self.send(CONSOLE, CONSOLE_IFACE, "SetUIInfo", &(width_mm, height_mm, 0i32, 0i32, width, height).to_variant());
    }

    /// Offers `text` to the guest as the clipboard's content, or withdraws the
    /// offer with `None`. The guest fetches the text when something is pasted there.
    pub fn clipboard_offer(&self, text: Option<&str>) {
        let text = text.filter(|text| text.len() <= clipboard::MAX_TEXT_BYTES);
        let had_offer = {
            let mut state = self.clipboard.lock().unwrap();
            std::mem::replace(&mut state.offered, text.map(|text| text.as_bytes().to_vec())).is_some()
        };
        if text.is_none() && !had_offer {
            return;
        }
        // Said on the display thread, in order with what QEMU says to us there.
        let (connection, sink, state) = (self.connection.clone(), self.sink.clone(), self.clipboard.clone());
        let offering = text.is_some();
        self.main_loop.context().invoke(move || {
            if offering {
                clipboard::announce(&connection, &sink, &state);
            } else {
                clipboard::withdraw(&connection, &sink, &state);
            }
        });
    }

    /// Fetches the text the guest offered (see [`Event::ClipboardOffered`]).
    /// `done` is called on the calling thread's main loop.
    pub fn clipboard_request(&self, done: impl FnOnce(Result<String, String>) + 'static) {
        let body = (clipboard::SELECTION, vec![clipboard::TEXT]).to_variant();
        self.connection.call(
            None,
            clipboard::PATH,
            clipboard::IFACE,
            "Request",
            Some(&body),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            gio::Cancellable::NONE,
            move |reply| {
                done(reply.map_err(|e| e.message().to_owned()).and_then(|reply| {
                    let data = (reply.n_children() == 2).then(|| reply.child_value(1));
                    let text = data.as_ref().and_then(|data| data.fixed_array::<u8>().ok());
                    text.map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                        .ok_or_else(|| "unexpected clipboard reply".to_owned())
                }))
            },
        );
    }

    /// Sends a call without waiting for its outcome, so input can never block the window.
    fn send(&self, path: &str, interface: &str, method: &str, body: &glib::Variant) {
        let message = gio::DBusMessage::new_method_call(None, path, Some(interface), method);
        message.set_body(body);
        message.set_flags(gio::DBusMessageFlags::NO_REPLY_EXPECTED);
        // A failure here means the connection is gone, which is reported as Event::Closed.
        let _ = self.connection.send_message(&message, gio::DBusSendMessageFlags::NONE);
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // Our own disconnect is not news.
        self.closing.store(true, Ordering::SeqCst);
        self.main_loop.quit();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = self.connection.close_sync(gio::Cancellable::NONE);
    }
}

type Ready = Result<(gio::DBusConnection, glib::MainLoop, ConsoleInfo), String>;

fn display_thread(
    socket: OwnedFd,
    options: Options,
    clipboard: clipboard::Shared,
    sink: EventSink,
    closing: Arc<AtomicBool>,
    cancellable: gio::Cancellable,
    ready: mpsc::Sender<Ready>,
) {
    let context = glib::MainContext::new();
    let main_loop = glib::MainLoop::new(Some(&context), false);
    let run = || match setup(socket, &options, &clipboard, &sink, &closing, &cancellable) {
        Ok(session) => {
            if ready.send(Ok((session.connection.clone(), main_loop.clone(), session.info.clone()))).is_ok() {
                main_loop.run();
            }
            let _ = session.listener.close_sync(gio::Cancellable::NONE);
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    };
    // Fails only for a context that another thread owns, which this one cannot be.
    let _ = context.with_thread_default(run);
}

pub(crate) fn stream_for(socket: OwnedFd) -> Result<gio::SocketConnection, String> {
    let socket = gio::Socket::from_fd(socket).map_err(|e| format!("cannot use the display socket: {e}"))?;
    Ok(socket.connection_factory_create_connection())
}

/// Everything that has to stay alive on the display thread while the connection is in use.
struct Session {
    connection: gio::DBusConnection,
    listener: gio::DBusConnection,
    info: ConsoleInfo,
    _sound: SoundConnections,
}

/// The sound listeners' connections, filled in once they are set up.
type SoundConnections = Rc<RefCell<Vec<gio::DBusConnection>>>;

/// Connects and registers the screen listener.
fn setup(
    socket: OwnedFd,
    options: &Options,
    clipboard: &clipboard::Shared,
    sink: &EventSink,
    closing: &Arc<AtomicBool>,
    cancellable: &gio::Cancellable,
) -> Result<Session, String> {
    let explain = |what: &str, e: glib::Error| format!("{what}: {}", e.message());
    let connection = gio::DBusConnection::new_sync(
        &stream_for(socket)?,
        None,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT,
        None,
        Some(cancellable),
    )
    .map_err(|e| explain("cannot connect to the virtual machine's display", e))?;
    connection.set_exit_on_close(false);

    let properties = |interface: &str| -> Result<HashMap<String, glib::Variant>, String> {
        let reply = connection
            .call_sync(
                None,
                CONSOLE,
                PROPERTIES_IFACE,
                "GetAll",
                Some(&(interface,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
                Some(cancellable),
            )
            .map_err(|e| explain("the virtual machine has no usable screen", e))?;
        reply.child_value(0).get().ok_or_else(|| "unexpected screen description from QEMU".to_owned())
    };
    let console = properties(CONSOLE_IFACE)?;
    let text = |name: &str| console.get(name).and_then(|v| v.get::<String>()).unwrap_or_default();
    let number = |name: &str| console.get(name).and_then(|v| v.get::<u32>()).unwrap_or_default();
    // A console without a pointer (unusual) simply reports a relative one.
    let pointer_absolute =
        properties(MOUSE_IFACE).ok().and_then(|p| p.get("IsAbsolute").and_then(|v| v.get::<bool>())).unwrap_or(false);
    let info = ConsoleInfo { label: text("Label"), width: number("Width"), height: number("Height"), pointer_absolute };

    // QEMU answers RegisterListener first, then starts talking D-Bus on the socket it was given.
    let (ours, theirs) = UnixStream::pair().map_err(|e| format!("cannot create the screen connection: {e}"))?;
    connection
        .call_with_unix_fd_list_sync(
            None,
            CONSOLE,
            CONSOLE_IFACE,
            "RegisterListener",
            Some(&(Handle(0),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            Some(&gio::UnixFDList::from_array([OwnedFd::from(theirs)])),
            Some(cancellable),
        )
        .map_err(|e| explain("QEMU did not accept the screen connection", e))?;
    let listener = gio::DBusConnection::new_sync(
        &stream_for(ours.into())?,
        None,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::DELAY_MESSAGE_PROCESSING,
        None,
        Some(cancellable),
    )
    .map_err(|e| explain("cannot open the screen connection", e))?;
    listener.set_exit_on_close(false);

    let node = gio::DBusNodeInfo::for_xml(LISTENER_XML)
        .map_err(|e| explain("internal error in the listener description", e))?;
    for interface in [LISTENER_IFACE, MAP_IFACE] {
        let description = node.lookup_interface(interface).ok_or("internal error: listener interface missing")?;
        let sink = sink.clone();
        listener
            .register_object(LISTENER_PATH, &description)
            .method_call(move |_, _, _, _, method, parameters, invocation| {
                // QEMU may be waiting for this answer: decode, pass on, reply. Nothing slow.
                let event = decode(method, &parameters, &invocation)
                    .unwrap_or_else(|problem| Event::Problem(format!("{method}: {problem}")));
                sink(event);
                invocation.return_value(None);
            })
            // The only property: which optional interfaces this listener implements.
            .property(|_, _, _, _, _| vec![MAP_IFACE].to_variant())
            .build()
            .map_err(|e| explain("cannot set up the screen listener", e))?;
    }

    // Pointer mode changes when the guest starts or stops using its tablet. This is read with a
    // message filter rather than a signal subscription: on a connection without a message bus
    // signals carry no sender name, which gio's Rust binding for subscriptions cannot represent.
    {
        // The filter runs on GDBus's own thread, so it may only capture what is safe to share.
        let sink: EventSink = sink.clone();
        connection.add_filter(move |_, message, incoming| {
            if incoming && message.message_type() == gio::DBusMessageType::Signal {
                if let Some(absolute) = pointer_mode_change(message) {
                    sink(Event::PointerMode { absolute });
                }
            }
            Some(message.clone())
        });
    }
    for watched in [&connection, &listener] {
        let (sink, closing) = (sink.clone(), closing.clone());
        watched.connect_closed(move |_, _, _| {
            // Both connections close together; report it once, and not when we hung up ourselves.
            if !closing.swap(true, Ordering::SeqCst) {
                sink(Event::Closed);
            }
        });
    }
    listener.start_message_processing();

    // Sound and clipboard are extras: without them the screen still works, so their failures are
    // only reported. They are set up from the loop, once it runs (see `audio::register` for why).
    let sound: SoundConnections = Rc::default();
    if let Some(host) = options.audio.clone() {
        let (connection, sink, sound) = (connection.clone(), sink.clone(), sound.clone());
        glib::MainContext::ref_thread_default().spawn_local(async move {
            match audio::register(&connection, &host).await {
                Ok(connections) => *sound.borrow_mut() = connections,
                Err(problem) => sink(Event::Problem(format!("sound: {problem}"))),
            }
        });
    }
    if options.clipboard {
        if let Err(problem) = clipboard::export(&connection, sink, clipboard) {
            sink(Event::Problem(format!("clipboard: {problem}")));
        }
    }
    Ok(Session { connection, listener, info, _sound: sound })
}

/// The new pointer mode, if `message` announces a change of it.
fn pointer_mode_change(message: &gio::DBusMessage) -> Option<bool> {
    let is = |value: Option<glib::GString>, wanted: &str| value.is_some_and(|v| v == wanted);
    if !(is(message.interface(), PROPERTIES_IFACE)
        && is(message.member(), "PropertiesChanged")
        && is(message.path(), CONSOLE))
    {
        return None;
    }
    let body = message.body()?;
    if body.n_children() < 2 || body.child_value(0).str() != Some(MOUSE_IFACE) {
        return None;
    }
    let changed: HashMap<String, glib::Variant> = body.child_value(1).get()?;
    changed.get("IsAbsolute")?.get::<bool>()
}

/// Turns one call from QEMU into an event.
fn decode(method: &str, parameters: &glib::Variant, invocation: &gio::DBusMethodInvocation) -> Result<Event, String> {
    let descriptor = |index: usize| -> Result<OwnedFd, String> {
        let Handle(handle) = arg(parameters, index)?;
        let list = invocation.message().unix_fd_list().ok_or("no file descriptor came with the call")?;
        list.get(handle).map_err(|e| format!("cannot take the file descriptor: {}", e.message()))
    };
    let p = parameters;
    Ok(match method {
        "Scanout" => Event::Scanout {
            width: arg(p, 0)?,
            height: arg(p, 1)?,
            stride: arg(p, 2)?,
            format: arg(p, 3)?,
            data: bytes(p, 4)?,
        },
        "Update" => Event::Update {
            x: arg(p, 0)?,
            y: arg(p, 1)?,
            width: arg(p, 2)?,
            height: arg(p, 3)?,
            stride: arg(p, 4)?,
            format: arg(p, 5)?,
            data: bytes(p, 6)?,
        },
        "ScanoutMap" => {
            let (offset, width, height, stride): (u32, u32, u32, u32) =
                (arg(p, 1)?, arg(p, 2)?, arg(p, 3)?, arg(p, 4)?);
            let map = Mapping::new(
                descriptor(0)?,
                offset as usize,
                width as usize,
                height as usize,
                stride as usize,
                arg(p, 5)?,
            )?;
            Event::ScanoutMap(Arc::new(map))
        }
        "UpdateMap" => Event::UpdateMap { x: arg(p, 0)?, y: arg(p, 1)?, width: arg(p, 2)?, height: arg(p, 3)? },
        "ScanoutDMABUF" => Event::ScanoutDmabuf(Dmabuf {
            fd: descriptor(0)?,
            width: arg(p, 1)?,
            height: arg(p, 2)?,
            stride: arg(p, 3)?,
            fourcc: arg(p, 4)?,
            modifier: arg(p, 5)?,
            stored_bottom_up: arg(p, 6)?,
        }),
        "UpdateDMABUF" => Event::UpdateDmabuf { x: arg(p, 0)?, y: arg(p, 1)?, width: arg(p, 2)?, height: arg(p, 3)? },
        "Disable" => Event::Disable,
        "MouseSet" => Event::MouseSet { x: arg(p, 0)?, y: arg(p, 1)?, on: arg::<i32>(p, 2)? != 0 },
        "CursorDefine" => Event::CursorDefine {
            width: arg(p, 0)?,
            height: arg(p, 1)?,
            hot_x: arg(p, 2)?,
            hot_y: arg(p, 3)?,
            data: bytes(p, 4)?,
        },
        other => return Err(format!("unknown call '{other}'")),
    })
}
