//! The guest agent: a helper program inside the guest that roomy can ask to
//! do things only the guest can do. roomy uses it to correct the guest's
//! clock after a pause or a restore.
//!
//! Two rules from the lab runs shape this client:
//! - a request is only sent when the agent is known to be listening, so no
//!   stale "set the time to ..." command can sit in a queue and run later;
//! - the connection stays open for the VM's lifetime and late replies are
//!   read and discarded, because an agent whose reply is never collected can
//!   block and hold up the guest's shutdown.

use crate::qemu::ids;
use crate::qmp::Qmp;
use serde_json::{Value, json};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockSync {
    /// The guest's clock now matches this computer's.
    Synced,
    /// No agent is running in the guest; its clock catches up on its own schedule.
    AgentNotRunning,
    /// The agent accepted the request but has not answered yet.
    NoAnswer,
    Failed(String),
}

pub struct Agent {
    socket: PathBuf,
    stream: Option<UnixStream>,
}

impl Agent {
    pub fn new(socket: PathBuf) -> Agent {
        Agent { socket, stream: None }
    }

    /// Sets the guest's clock to this computer's current time.
    pub fn sync_clock(&mut self, qmp: &mut Qmp) -> ClockSync {
        if !agent_is_listening(qmp) {
            return ClockSync::AgentNotRunning;
        }
        self.discard_late_replies();
        let stream = match &mut self.stream {
            Some(stream) => stream,
            empty => match UnixStream::connect(&self.socket) {
                Ok(stream) => empty.insert(stream),
                Err(e) => return ClockSync::Failed(format!("cannot reach the agent channel: {e}")),
            },
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        // An explicit time is essential: without it the agent reads the
        // guest's hardware clock, which took 10 seconds and failed in the lab.
        let request = json!({ "execute": "guest-set-time", "arguments": { "time": now as u64 } });
        if let Err(e) = stream.write_all(format!("{request}\n").as_bytes()) {
            self.stream = None;
            return ClockSync::Failed(format!("cannot write to the agent channel: {e}"));
        }
        match read_line(stream, Duration::from_secs(3)) {
            Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                Ok(reply) if reply.get("return").is_some() => ClockSync::Synced,
                Ok(reply) => {
                    ClockSync::Failed(reply["error"]["desc"].as_str().unwrap_or("the agent refused").to_owned())
                }
                Err(_) => ClockSync::Failed(format!("unexpected answer from the agent: {}", line.trim())),
            },
            Ok(None) => ClockSync::NoAnswer,
            Err(e) => {
                self.stream = None;
                ClockSync::Failed(format!("agent channel error: {e}"))
            }
        }
    }

    /// Reads and drops anything the agent sent since the last request.
    pub fn discard_late_replies(&mut self) {
        let Some(stream) = &mut self.stream else { return };
        if stream.set_nonblocking(true).is_err() {
            return;
        }
        let mut sink = [0u8; 4096];
        let closed = loop {
            match stream.read(&mut sink) {
                Ok(0) => break true,
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break false,
                Err(_) => break true,
            }
        };
        if closed || stream.set_nonblocking(false).is_err() {
            self.stream = None;
        }
    }
}

/// Whether the guest's agent has its end of the channel open, according to QEMU.
pub fn agent_is_listening(qmp: &mut Qmp) -> bool {
    qmp.execute("query-chardev", None).ok().is_some_and(|chardevs| {
        chardevs
            .as_array()
            .is_some_and(|list| list.iter().any(|c| c["label"] == ids::AGENT_CHARDEV && c["frontend-open"] == true))
    })
}

/// Reads one line, waiting at most `limit`. `Ok(None)` means nothing arrived in time.
fn read_line(stream: &mut UnixStream, limit: Duration) -> std::io::Result<Option<String>> {
    let deadline = Instant::now() + limit;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut byte) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(_) if byte[0] == b'\n' => return Ok(Some(String::from_utf8_lossy(&line).into_owned())),
            Ok(_) => line.push(byte[0]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Ok(None),
            Err(e) => return Err(e),
        }
    }
}
