//! Text clipboard shared with the guest.
//!
//! Both sides implement the same small interface on the main connection.
//! QEMU asks for the clipboard's content and waits for the answer, so what
//! this computer offers is kept ready here and handed over without asking
//! the window.
//!
//! Every takeover of the clipboard ("grab"), from either side, carries a
//! number. The guest's helper is strict about it: the count starts at 0
//! when a session starts, a grab must carry exactly the current count, and
//! the count then goes up by one on both sides. A grab numbered otherwise is
//! thrown away by the guest without a word, so this must be exact.

use crate::{Event, EventSink};
use gio::prelude::*;
use std::sync::{Arc, Mutex};

pub(crate) const PATH: &str = "/org/qemu/Display1/Clipboard";
pub(crate) const IFACE: &str = "org.qemu.Display1.Clipboard";
/// The one kind of content roomy shares.
pub const TEXT: &str = "text/plain;charset=utf-8";
/// The ordinary copy-and-paste clipboard, as opposed to the mouse selection.
pub(crate) const SELECTION: u32 = 0;
/// Larger texts are not offered to the guest.
pub const MAX_TEXT_BYTES: usize = 4 << 20;

const XML: &str = r#"
<node>
  <interface name="org.qemu.Display1.Clipboard">
    <method name="Register"/>
    <method name="Unregister"/>
    <method name="Grab">
      <arg type="u" name="selection" direction="in"/><arg type="u" name="serial" direction="in"/>
      <arg type="as" name="mimes" direction="in"/>
    </method>
    <method name="Release"><arg type="u" name="selection" direction="in"/></method>
    <method name="Request">
      <arg type="u" name="selection" direction="in"/><arg type="as" name="mimes" direction="in"/>
      <arg type="s" name="reply_mime" direction="out"/><arg type="ay" name="data" direction="out"/>
    </method>
    <property name="Interfaces" type="as" access="read"/>
  </interface>
</node>
"#;

/// Who holds the clipboard, as agreed with QEMU.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// QEMU accepted this side as its clipboard partner. Nothing is sent before that.
    pub registered: bool,
    /// The number the next grab will carry, whichever side makes it.
    pub next_serial: u32,
    /// The text this computer is offering, if it is the one holding the clipboard.
    pub offered: Option<Vec<u8>>,
}

pub(crate) type Shared = Arc<Mutex<State>>;

/// Calls a method of QEMU's clipboard object without waiting. A refusal is
/// reported, because QEMU otherwise drops a call it dislikes without a trace.
/// Must run on the display thread.
fn call(connection: &gio::DBusConnection, sink: &EventSink, method: &'static str, body: glib::Variant) {
    let sink = sink.clone();
    connection.call(
        None,
        PATH,
        IFACE,
        method,
        Some(&body),
        None,
        gio::DBusCallFlags::NONE,
        crate::CALL_TIMEOUT_MS,
        gio::Cancellable::NONE,
        move |outcome| match outcome {
            // The connection going away is reported once, elsewhere.
            Err(e) if !e.matches(gio::IOErrorEnum::Closed) => {
                sink(Event::Problem(format!("clipboard: the virtual machine refused {method}: {}", e.message())))
            }
            _ => {}
        },
    );
}

/// Tells the guest that this computer holds the clipboard, if it has text on
/// offer. Must run on the display thread.
pub(crate) fn announce(connection: &gio::DBusConnection, sink: &EventSink, state: &Shared) {
    let serial = {
        let mut state = state.lock().unwrap();
        if !state.registered || state.offered.is_none() {
            return;
        }
        let serial = state.next_serial;
        state.next_serial = serial.wrapping_add(1);
        serial
    };
    call(connection, sink, "Grab", (SELECTION, serial, vec![TEXT]).to_variant());
}

/// Tells the guest that this computer no longer offers anything. Must run on the display thread.
pub(crate) fn withdraw(connection: &gio::DBusConnection, sink: &EventSink, state: &Shared) {
    if state.lock().unwrap().registered {
        call(connection, sink, "Release", (SELECTION,).to_variant());
    }
}

/// Makes this side's clipboard object available to QEMU and registers with it.
pub(crate) fn export(connection: &gio::DBusConnection, sink: &EventSink, state: &Shared) -> Result<(), String> {
    let node = gio::DBusNodeInfo::for_xml(XML).map_err(|e| e.message().to_owned())?;
    let description = node.lookup_interface(IFACE).ok_or("internal error: clipboard interface missing")?;
    let (sink, state) = (sink.clone(), state.clone());
    connection
        .register_object(PATH, &description)
        .method_call({
            let (sink, state) = (sink.clone(), state.clone());
            move |connection, _, _, _, method, parameters, invocation| {
                let selection = (parameters.n_children() > 0).then(|| parameters.child_value(0).get::<u32>()).flatten();
                match method {
                    "Grab" if selection == Some(SELECTION) => {
                        let serial = parameters.child_value(1).get::<u32>().unwrap_or(0);
                        let mimes = parameters.child_value(2).get::<Vec<String>>().unwrap_or_default();
                        let mut state = state.lock().unwrap();
                        // A lower number is a grab that lost a race against one of ours.
                        if serial >= state.next_serial {
                            state.next_serial = serial.wrapping_add(1);
                            state.offered = None;
                            drop(state);
                            sink(Event::ClipboardOffered { mimes });
                        }
                    }
                    "Release" if selection == Some(SELECTION) => sink(Event::ClipboardReleased),
                    "Request" => {
                        let wanted = parameters.child_value(1).get::<Vec<String>>().unwrap_or_default();
                        let text = state.lock().unwrap().offered.clone().filter(|_| {
                            selection == Some(SELECTION)
                                && (wanted.is_empty() || wanted.iter().any(|mime| mime == TEXT))
                        });
                        return match text {
                            Some(text) => {
                                let data = glib::Variant::array_from_fixed_array(&text[..]);
                                invocation
                                    .return_value(Some(&glib::Variant::tuple_from_iter([TEXT.to_variant(), data])));
                            }
                            None => invocation.return_dbus_error("org.qemu.Display1.Error.Failed", "nothing to paste"),
                        };
                    }
                    // QEMU starts the count over whenever the guest's helper (re)connects, and the guest
                    // has then forgotten what was on offer: answer first, then offer it again from 0.
                    "Register" => {
                        state.lock().unwrap().next_serial = 0;
                        invocation.return_value(None);
                        announce(&connection, &sink, &state);
                        return;
                    }
                    "Unregister" => *state.lock().unwrap() = State::default(),
                    _ => {}
                }
                invocation.return_value(None);
            }
        })
        .property(|_, _, _, _, _| Vec::<String>::new().to_variant())
        .build()
        .map_err(|e| format!("cannot set up clipboard sharing: {}", e.message()))?;

    // Not waited for here: while registering us QEMU reads this side's object, which
    // only the display thread's loop (not yet running) can answer.
    connection.call(
        None,
        PATH,
        IFACE,
        "Register",
        None,
        None,
        gio::DBusCallFlags::NONE,
        crate::CALL_TIMEOUT_MS,
        gio::Cancellable::NONE,
        {
            let connection = connection.clone();
            move |outcome| match outcome {
                Ok(_) => {
                    {
                        let mut state = state.lock().unwrap();
                        state.registered = true;
                        state.next_serial = 0;
                    }
                    // Text copied on this computer while the registration was under way.
                    announce(&connection, &sink, &state);
                }
                // Sharing simply stays off; typical when another window already shares this VM's clipboard.
                Err(e) => eprintln!("roomy: clipboard sharing is not available: {}", e.message()),
            }
        },
    );
    Ok(())
}
