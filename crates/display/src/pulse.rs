//! Playing the guest's sound and recording the microphone through the
//! desktop's sound server (PulseAudio's client library, which PipeWire
//! provides as well).
//!
//! The library is loaded when first needed rather than linked, so roomy's
//! window still starts on a computer without it; there is simply no sound.
//! Each stream has a thread of its own that does the waiting, and the
//! methods called from the display thread only pass data along.

use crate::audio::{AudioHost, Format};
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

const PA_STREAM_PLAYBACK: c_int = 1;
const PA_STREAM_RECORD: c_int = 2;
/// "Let the server decide" for a buffer setting.
const UNSET: u32 = u32::MAX;

type NewFn = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const BufferAttr,
    *mut c_int,
) -> *mut c_void;
type TransferFn = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
type FreeFn = unsafe extern "C" fn(*mut c_void);

/// The few functions of `libpulse-simple` roomy uses.
struct Library {
    new: NewFn,
    write: TransferFn,
    read: TransferFn,
    free: FreeFn,
}

impl Library {
    fn load() -> Result<Library, String> {
        const NAME: &CStr = c"libpulse-simple.so.0";
        // SAFETY: dlopen with a valid name; the library stays loaded for the life of the process.
        let handle = unsafe { libc::dlopen(NAME.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err("the sound library (libpulse) is not installed".to_owned());
        }
        let symbol = |name: &CStr| -> Result<*mut c_void, String> {
            // SAFETY: handle is a loaded library and name a valid string.
            let found = unsafe { libc::dlsym(handle, name.as_ptr()) };
            if found.is_null() { Err(format!("the sound library lacks {}", name.to_string_lossy())) } else { Ok(found) }
        };
        // SAFETY: the symbols are C functions with exactly these signatures (pulse/simple.h).
        unsafe {
            Ok(Library {
                new: std::mem::transmute::<*mut c_void, NewFn>(symbol(c"pa_simple_new")?),
                write: std::mem::transmute::<*mut c_void, TransferFn>(symbol(c"pa_simple_write")?),
                read: std::mem::transmute::<*mut c_void, TransferFn>(symbol(c"pa_simple_read")?),
                free: std::mem::transmute::<*mut c_void, FreeFn>(symbol(c"pa_simple_free")?),
            })
        }
    }
}

/// PulseAudio's number for a sample layout, or `None` if it has none.
fn sample_format(format: Format) -> Option<c_int> {
    Some(match (format.bits, format.signed, format.float, format.big_endian) {
        (8, false, false, _) => 0,     // PA_SAMPLE_U8
        (16, true, false, false) => 3, // PA_SAMPLE_S16LE
        (16, true, false, true) => 4,  // PA_SAMPLE_S16BE
        (32, _, true, false) => 5,     // PA_SAMPLE_FLOAT32LE
        (32, _, true, true) => 6,      // PA_SAMPLE_FLOAT32BE
        (32, true, false, false) => 7, // PA_SAMPLE_S32LE
        (32, true, false, true) => 8,  // PA_SAMPLE_S32BE
        _ => return None,
    })
}

/// One open stream to the sound server.
struct Stream {
    library: Arc<Library>,
    handle: *mut c_void,
}

// SAFETY: the raw stream handles live on their own threads; what is shared is behind locks and atomics.
unsafe impl Send for Stream {}

impl Stream {
    fn open(library: &Arc<Library>, name: &str, record: bool, format: Format) -> Result<Stream, String> {
        let sample = sample_format(format).ok_or_else(|| format!("unsupported sound format {format:?}"))?;
        if format.rate == 0 || format.channels == 0 || format.bytes_per_frame == 0 {
            return Err(format!("unusable sound format {format:?}"));
        }
        let spec = SampleSpec { format: sample, rate: format.rate, channels: format.channels };
        let bytes_per_ms = format.rate / 1000 * format.bytes_per_frame;
        // Keep delay short: about 60 ms buffered for playback, 10 ms pieces for recording.
        let attr = BufferAttr {
            maxlength: UNSET,
            tlength: if record { UNSET } else { bytes_per_ms * 60 },
            prebuf: UNSET,
            minreq: UNSET,
            fragsize: if record { bytes_per_ms * 10 } else { UNSET },
        };
        let app = c"roomy";
        let stream = CString::new(name.replace('\0', " ")).unwrap_or_default();
        let direction = if record { PA_STREAM_RECORD } else { PA_STREAM_PLAYBACK };
        let mut error: c_int = 0;
        // SAFETY: all pointers are valid for the call; null server and device mean the defaults.
        let handle = unsafe {
            (library.new)(
                std::ptr::null(),
                app.as_ptr(),
                direction,
                std::ptr::null(),
                stream.as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut error,
            )
        };
        if handle.is_null() {
            return Err(format!("the sound server did not accept the stream (error {error})"));
        }
        Ok(Stream { library: library.clone(), handle })
    }

    fn write(&self, data: &[u8]) -> bool {
        let mut error: c_int = 0;
        // SAFETY: handle is open; the library only reads `data`.
        unsafe { (self.library.write)(self.handle, data.as_ptr() as *mut c_void, data.len(), &mut error) >= 0 }
    }

    fn read(&self, data: &mut [u8]) -> bool {
        let mut error: c_int = 0;
        // SAFETY: handle is open; `data` is writable for its length.
        unsafe { (self.library.read)(self.handle, data.as_mut_ptr().cast(), data.len(), &mut error) >= 0 }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: handle came from pa_simple_new and is freed once.
        unsafe { (self.library.free)(self.handle) };
    }
}

enum Playback {
    Data(Vec<u8>),
    Volume { muted: bool, volume: Vec<u8> },
}

/// Scales 16-bit little-endian samples in place; other formats can only be muted.
fn apply_volume(data: &mut [u8], format: Format, muted: bool, volume: &[u8]) {
    if muted {
        let silence = if format.bits == 8 && !format.signed { 0x80 } else { 0 };
        data.fill(silence);
        return;
    }
    let scalable = format.bits == 16 && format.signed && !format.float && !format.big_endian;
    if !scalable || volume.iter().all(|v| *v == 255) {
        return;
    }
    let channels = usize::from(format.channels.max(1));
    for (index, sample) in data.chunks_exact_mut(2).enumerate() {
        let level = i32::from(volume.get(index % channels).or(volume.first()).copied().unwrap_or(255));
        let scaled = (i32::from(i16::from_le_bytes([sample[0], sample[1]])) * level / 255) as i16;
        sample.copy_from_slice(&scaled.to_le_bytes());
    }
}

fn playback_thread(
    library: Arc<Library>,
    name: String,
    format: Format,
    incoming: Receiver<Playback>,
    counters: Arc<Counters>,
) {
    let (mut stream, mut muted, mut volume) = (None::<Stream>, false, Vec::new());
    let mut failed_at = None::<std::time::Instant>;
    loop {
        // An idle guest should not keep a stream open on the desktop.
        let message = match incoming.recv_timeout(Duration::from_secs(5)) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => {
                stream = None;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match message {
            Playback::Volume { muted: m, volume: v } => (muted, volume) = (m, v),
            Playback::Data(mut data) => {
                if stream.is_none() && failed_at.is_none_or(|at| at.elapsed() > Duration::from_secs(5)) {
                    match Stream::open(&library, &name, false, format) {
                        Ok(opened) => stream = Some(opened),
                        Err(problem) => {
                            if failed_at.is_none() {
                                eprintln!("roomy: no sound from {name}: {problem}");
                            }
                            failed_at = Some(std::time::Instant::now());
                        }
                    }
                }
                apply_volume(&mut data, format, muted, &volume);
                match stream.as_ref().map(|open| open.write(&data)) {
                    Some(true) => {
                        counters.played.fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                    // The server went away; try again with the next data.
                    Some(false) => stream = None,
                    None => {}
                }
            }
        }
    }
}

/// Microphone sound waiting to be read by the guest.
struct Captured {
    waiting: Mutex<VecDeque<u8>>,
    wanted: AtomicBool,
    stopped: AtomicBool,
}

fn capture_thread(
    library: Arc<Library>,
    name: String,
    format: Format,
    allowed: Arc<AtomicBool>,
    shared: Arc<Captured>,
    counters: Arc<Counters>,
) {
    let piece = (format.rate / 100 * format.bytes_per_frame).max(format.bytes_per_frame) as usize;
    // Keep at most about 200 ms, so what the guest hears is never far behind.
    let limit = piece * 20;
    let (mut stream, mut buffer) = (None::<Stream>, vec![0u8; piece]);
    while !shared.stopped.load(Ordering::Relaxed) {
        if !(shared.wanted.load(Ordering::Relaxed) && allowed.load(Ordering::Relaxed)) {
            // Not listening: the microphone is released, not just ignored.
            stream = None;
            shared.waiting.lock().unwrap().clear();
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if stream.is_none() {
            match Stream::open(&library, &name, true, format) {
                Ok(opened) => stream = Some(opened),
                Err(problem) => {
                    eprintln!("roomy: no microphone for {name}: {problem}");
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            }
        }
        if stream.as_ref().is_some_and(|open| open.read(&mut buffer)) {
            counters.recorded.fetch_add(buffer.len() as u64, Ordering::Relaxed);
            let mut waiting = shared.waiting.lock().unwrap();
            waiting.extend(&buffer);
            let excess = waiting.len().saturating_sub(limit);
            waiting.drain(..excess);
        } else {
            stream = None;
        }
    }
}

/// Running totals, for telling whether sound is actually flowing.
#[derive(Debug, Default)]
pub struct Counters {
    /// Bytes of guest sound received from QEMU.
    pub received: AtomicU64,
    /// Bytes of guest sound the sound server accepted.
    pub played: AtomicU64,
    /// Bytes of microphone sound recorded for the guest.
    pub recorded: AtomicU64,
    /// Bytes the guest asked for from its microphone.
    pub requested: AtomicU64,
}

/// Sound through the desktop's sound server.
pub struct PulseAudio {
    counters: Arc<Counters>,
    library: Arc<Library>,
    /// Shown by the desktop's volume control next to the stream.
    name: String,
    microphone: Arc<AtomicBool>,
    playing: Mutex<HashMap<u64, SyncSender<Playback>>>,
    capturing: Mutex<HashMap<u64, Arc<Captured>>>,
}

impl PulseAudio {
    /// Fails when the sound library is not installed.
    pub fn new(name: &str, microphone: bool) -> Result<PulseAudio, String> {
        Ok(PulseAudio {
            counters: Arc::default(),
            library: Arc::new(Library::load()?),
            name: name.to_owned(),
            microphone: Arc::new(AtomicBool::new(microphone)),
            playing: Mutex::default(),
            capturing: Mutex::default(),
        })
    }

    /// (received, played, recorded, requested) byte counts so far.
    pub fn totals(&self) -> (u64, u64, u64, u64) {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let c = &self.counters;
        (read(&c.received), read(&c.played), read(&c.recorded), read(&c.requested))
    }

    /// Lets the guest hear this computer's microphone, or not. Takes effect at once.
    pub fn set_microphone(&self, on: bool) {
        self.microphone.store(on, Ordering::Relaxed);
    }

    fn send(&self, id: u64, message: Playback) {
        if let Some(stream) = self.playing.lock().unwrap().get(&id) {
            // A full queue means the desktop is not taking sound right now; dropping
            // keeps the guest's sound current instead of ever further behind.
            let _ = stream.try_send(message);
        }
    }
}

impl AudioHost for PulseAudio {
    fn playback_start(&self, id: u64, format: Format) {
        let (sender, receiver) = sync_channel(64);
        let (library, name, counters) = (self.library.clone(), self.name.clone(), self.counters.clone());
        let spawned = std::thread::Builder::new()
            .name("roomy-sound".into())
            .spawn(move || playback_thread(library, name, format, receiver, counters));
        if spawned.is_ok() {
            // Replacing an earlier stream with this id ends its thread.
            self.playing.lock().unwrap().insert(id, sender);
        }
    }

    fn playback_data(&self, id: u64, data: Vec<u8>) {
        self.counters.received.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.send(id, Playback::Data(data));
    }

    fn playback_volume(&self, id: u64, muted: bool, volume: Vec<u8>) {
        self.send(id, Playback::Volume { muted, volume });
    }

    fn playback_enabled(&self, _id: u64, _enabled: bool) {}

    fn playback_stop(&self, id: u64) {
        self.playing.lock().unwrap().remove(&id);
    }

    fn capture_start(&self, id: u64, format: Format) {
        let shared = Arc::new(Captured {
            waiting: Mutex::default(),
            wanted: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let (library, name, allowed, for_thread) =
            (self.library.clone(), self.name.clone(), self.microphone.clone(), shared.clone());
        let counters = self.counters.clone();
        let spawned = std::thread::Builder::new()
            .name("roomy-microphone".into())
            .spawn(move || capture_thread(library, name, format, allowed, for_thread, counters));
        if spawned.is_ok() {
            if let Some(previous) = self.capturing.lock().unwrap().insert(id, shared) {
                previous.stopped.store(true, Ordering::Relaxed);
            }
        }
    }

    fn capture_enabled(&self, id: u64, enabled: bool) {
        if let Some(stream) = self.capturing.lock().unwrap().get(&id) {
            stream.wanted.store(enabled, Ordering::Relaxed);
        }
    }

    fn capture_read(&self, id: u64, size: usize) -> Vec<u8> {
        self.counters.requested.fetch_add(size as u64, Ordering::Relaxed);
        let mut data = Vec::with_capacity(size);
        if let Some(stream) = self.capturing.lock().unwrap().get(&id) {
            // Being asked for sound means the guest is recording, whether or not it said so first.
            stream.wanted.store(true, Ordering::Relaxed);
            let mut waiting = stream.waiting.lock().unwrap();
            let take = waiting.len().min(size);
            data.extend(waiting.drain(..take));
        }
        // Silence for whatever is missing: the guest gets what it asked for, at once.
        data.resize(size, 0);
        data
    }

    fn capture_stop(&self, id: u64) {
        if let Some(stream) = self.capturing.lock().unwrap().remove(&id) {
            stream.stopped.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for PulseAudio {
    fn drop(&mut self) {
        for stream in self.capturing.lock().unwrap().values() {
            stream.stopped.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CD: Format = Format {
        bits: 16,
        signed: true,
        float: false,
        rate: 48_000,
        channels: 2,
        bytes_per_frame: 4,
        big_endian: false,
    };

    #[test]
    fn volume_scales_each_channel_and_mute_silences() {
        // Two frames of (left = 1000, right = -1000).
        let frames: Vec<u8> = [1000i16, -1000, 1000, -1000].iter().flat_map(|s| s.to_le_bytes()).collect();
        let samples =
            |data: &[u8]| -> Vec<i16> { data.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])).collect() };

        let mut data = frames.clone();
        apply_volume(&mut data, CD, false, &[255, 255]);
        assert_eq!(data, frames, "full volume changes nothing");
        apply_volume(&mut data, CD, false, &[51, 255]);
        assert_eq!(samples(&data), [200, -1000, 200, -1000], "left at a fifth, right untouched");
        apply_volume(&mut data, CD, true, &[255, 255]);
        assert_eq!(samples(&data), [0, 0, 0, 0]);

        // Formats that cannot be scaled safely are passed through, and still muted when asked.
        let float = Format { bits: 32, float: true, bytes_per_frame: 8, ..CD };
        let mut data = vec![7u8; 8];
        apply_volume(&mut data, float, false, &[10, 10]);
        assert_eq!(data, [7; 8]);
        let mut unsigned = vec![1u8, 2];
        apply_volume(&mut unsigned, Format { bits: 8, signed: false, bytes_per_frame: 2, ..CD }, true, &[]);
        assert_eq!(unsigned, [0x80, 0x80], "silence for unsigned 8-bit sound is the middle value");
    }

    #[test]
    fn sound_formats_map_to_the_servers_numbers() {
        assert_eq!(sample_format(CD), Some(3));
        assert_eq!(sample_format(Format { big_endian: true, ..CD }), Some(4));
        assert_eq!(sample_format(Format { bits: 32, float: true, ..CD }), Some(5));
        assert_eq!(sample_format(Format { bits: 24, ..CD }), None);
    }

    #[test]
    fn the_guest_always_gets_an_immediate_answer_even_with_no_sound_server() {
        // Skipped where the library is not installed; there is then nothing to test.
        let Ok(sound) = PulseAudio::new("test", false) else { return };
        let started = std::time::Instant::now();
        sound.playback_start(1, CD);
        for _ in 0..200 {
            sound.playback_data(1, vec![0; 1920]);
        }
        sound.playback_volume(1, true, vec![0, 0]);
        sound.capture_start(2, CD);
        sound.capture_enabled(2, true);
        assert_eq!(sound.capture_read(2, 960), vec![0; 960], "silence while the microphone is off");
        assert_eq!(sound.capture_read(99, 16), vec![0; 16], "and for a stream that was never started");
        sound.playback_stop(1);
        sound.capture_stop(2);
        assert!(started.elapsed() < Duration::from_secs(2), "nothing here may wait for the sound server");
    }
}
