//! The guest's sound: QEMU's side of it.
//!
//! QEMU sends what the guest plays to an "out listener" and asks an "in
//! listener" for microphone sound. It asks for microphone sound and waits
//! for the answer, so [`AudioHost::capture_read`] must return at once.
//! Where the sound actually goes is up to the [`AudioHost`]; see [`crate::pulse`].

use gio::prelude::*;
use glib::variant::Handle;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

const AUDIO_PATH: &str = "/org/qemu/Display1/Audio";
const AUDIO_IFACE: &str = "org.qemu.Display1.Audio";

/// Signatures as in QEMU's `ui/dbus-display1.xml`.
const LISTENERS_XML: &str = r#"
<node>
  <interface name="org.qemu.Display1.AudioOutListener">
    <method name="Init">
      <arg type="t" name="id" direction="in"/><arg type="y" name="bits" direction="in"/>
      <arg type="b" name="is_signed" direction="in"/><arg type="b" name="is_float" direction="in"/>
      <arg type="u" name="freq" direction="in"/><arg type="y" name="nchannels" direction="in"/>
      <arg type="u" name="bytes_per_frame" direction="in"/><arg type="u" name="bytes_per_second" direction="in"/>
      <arg type="b" name="be" direction="in"/>
    </method>
    <method name="Fini"><arg type="t" name="id" direction="in"/></method>
    <method name="SetEnabled"><arg type="t" name="id" direction="in"/><arg type="b" name="enabled" direction="in"/></method>
    <method name="SetVolume">
      <arg type="t" name="id" direction="in"/><arg type="b" name="mute" direction="in"/>
      <arg type="ay" name="volume" direction="in"/>
    </method>
    <method name="Write"><arg type="t" name="id" direction="in"/><arg type="ay" name="data" direction="in"/></method>
    <property name="Interfaces" type="as" access="read"/>
  </interface>
  <interface name="org.qemu.Display1.AudioInListener">
    <method name="Init">
      <arg type="t" name="id" direction="in"/><arg type="y" name="bits" direction="in"/>
      <arg type="b" name="is_signed" direction="in"/><arg type="b" name="is_float" direction="in"/>
      <arg type="u" name="freq" direction="in"/><arg type="y" name="nchannels" direction="in"/>
      <arg type="u" name="bytes_per_frame" direction="in"/><arg type="u" name="bytes_per_second" direction="in"/>
      <arg type="b" name="be" direction="in"/>
    </method>
    <method name="Fini"><arg type="t" name="id" direction="in"/></method>
    <method name="SetEnabled"><arg type="t" name="id" direction="in"/><arg type="b" name="enabled" direction="in"/></method>
    <method name="SetVolume">
      <arg type="t" name="id" direction="in"/><arg type="b" name="mute" direction="in"/>
      <arg type="ay" name="volume" direction="in"/>
    </method>
    <method name="Read">
      <arg type="t" name="id" direction="in"/><arg type="t" name="size" direction="in"/>
      <arg type="ay" name="data" direction="out"/>
    </method>
    <property name="Interfaces" type="as" access="read"/>
  </interface>
</node>
"#;

/// How a stream's samples are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub bits: u8,
    pub signed: bool,
    pub float: bool,
    /// Frames per second.
    pub rate: u32,
    pub channels: u8,
    pub bytes_per_frame: u32,
    pub big_endian: bool,
}

/// Where the guest's sound goes and its microphone sound comes from.
///
/// Every method is called on the display thread and must return at once:
/// QEMU waits for [`capture_read`](AudioHost::capture_read), and a slow
/// answer to anything else delays the guest's screen. `id` tells streams apart.
pub trait AudioHost: Send + Sync {
    fn playback_start(&self, id: u64, format: Format);
    fn playback_data(&self, id: u64, data: Vec<u8>);
    /// `volume` has one value per channel, 0 to 255.
    fn playback_volume(&self, id: u64, muted: bool, volume: Vec<u8>);
    fn playback_enabled(&self, id: u64, enabled: bool);
    fn playback_stop(&self, id: u64);
    fn capture_start(&self, id: u64, format: Format);
    fn capture_enabled(&self, id: u64, enabled: bool);
    /// Up to `size` bytes of microphone sound; silence when there is none.
    fn capture_read(&self, id: u64, size: usize) -> Vec<u8>;
    fn capture_stop(&self, id: u64);
}

/// Registers both listeners with QEMU and returns their connections, which
/// must be kept for as long as sound should flow.
///
/// Nothing here waits with the display thread blocked. While QEMU sets up a
/// listener it asks that listener questions and waits for the answers, which
/// only this thread's loop can give; blocking on QEMU at the same time would
/// leave each side waiting for the other.
pub(crate) async fn register(
    connection: &gio::DBusConnection,
    host: &Arc<dyn AudioHost>,
) -> Result<Vec<gio::DBusConnection>, String> {
    let node = gio::DBusNodeInfo::for_xml(LISTENERS_XML).map_err(|e| e.message().to_owned())?;
    let mut connections = Vec::new();
    for (register, path, interface, playback) in [
        ("RegisterOutListener", "/org/qemu/Display1/AudioOutListener", "org.qemu.Display1.AudioOutListener", true),
        ("RegisterInListener", "/org/qemu/Display1/AudioInListener", "org.qemu.Display1.AudioInListener", false),
    ] {
        // As with the screen: QEMU answers first, then starts talking D-Bus on the socket it was given.
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("cannot create a sound connection: {e}"))?;
        connection
            .call_with_unix_fd_list_future(
                None,
                AUDIO_PATH,
                AUDIO_IFACE,
                register,
                Some(&(Handle(0),).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                crate::CALL_TIMEOUT_MS,
                Some(&gio::UnixFDList::from_array([OwnedFd::from(theirs)])),
            )
            .await
            .map_err(|e| format!("QEMU did not accept the sound connection: {}", e.message()))?;
        let listener = gio::DBusConnection::new_future(
            &crate::stream_for(ours.into())?,
            None,
            gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::DELAY_MESSAGE_PROCESSING,
            None,
        )
        .await
        .map_err(|e| format!("cannot open the sound connection: {}", e.message()))?;
        listener.set_exit_on_close(false);
        let description = node.lookup_interface(interface).ok_or("internal error: sound interface missing")?;
        let host = host.clone();
        listener
            .register_object(path, &description)
            .method_call(move |_, _, _, _, method, parameters, invocation| {
                let reply = if playback {
                    playback_call(&*host, method, &parameters)
                } else {
                    capture_call(&*host, method, &parameters)
                };
                // A call that cannot be understood is answered all the same: QEMU may be waiting.
                invocation.return_value(reply.unwrap_or(None).as_ref());
            })
            .property(|_, _, _, _, _| Vec::<String>::new().to_variant())
            .build()
            .map_err(|e| format!("cannot set up the sound listener: {}", e.message()))?;
        listener.start_message_processing();
        connections.push(listener);
    }
    Ok(connections)
}

fn arg<T: glib::variant::FromVariant>(parameters: &glib::Variant, index: usize) -> Option<T> {
    (index < parameters.n_children()).then(|| parameters.child_value(index).get()).flatten()
}

fn bytes(parameters: &glib::Variant, index: usize) -> Option<Vec<u8>> {
    if index >= parameters.n_children() {
        return None;
    }
    parameters.child_value(index).fixed_array::<u8>().ok().map(<[u8]>::to_vec)
}

fn format(p: &glib::Variant) -> Option<Format> {
    Some(Format {
        bits: arg(p, 1)?,
        signed: arg(p, 2)?,
        float: arg(p, 3)?,
        rate: arg(p, 4)?,
        channels: arg(p, 5)?,
        bytes_per_frame: arg(p, 6)?,
        big_endian: arg(p, 8)?,
    })
}

type Reply = Option<Option<glib::Variant>>;

fn playback_call(host: &dyn AudioHost, method: &str, p: &glib::Variant) -> Reply {
    let id: u64 = arg(p, 0)?;
    match method {
        "Init" => host.playback_start(id, format(p)?),
        "Fini" => host.playback_stop(id),
        "SetEnabled" => host.playback_enabled(id, arg(p, 1)?),
        "SetVolume" => host.playback_volume(id, arg(p, 1)?, bytes(p, 2)?),
        "Write" => host.playback_data(id, bytes(p, 1)?),
        _ => {}
    }
    Some(None)
}

fn capture_call(host: &dyn AudioHost, method: &str, p: &glib::Variant) -> Reply {
    let id: u64 = arg(p, 0)?;
    match method {
        "Init" => host.capture_start(id, format(p)?),
        "Fini" => host.capture_stop(id),
        "SetEnabled" => host.capture_enabled(id, arg(p, 1)?),
        "Read" => {
            // Never more than asked for, and never an absurd amount of silence for a wild request.
            let size = arg::<u64>(p, 1)?.min(1 << 20) as usize;
            let mut data = host.capture_read(id, size);
            data.truncate(size);
            let data = glib::Variant::array_from_fixed_array(&data[..]);
            return Some(Some(glib::Variant::tuple_from_iter([data])));
        }
        _ => {}
    }
    Some(None)
}
