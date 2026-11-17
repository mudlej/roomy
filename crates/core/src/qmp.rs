//! Minimal synchronous client for the QEMU Machine Protocol (QMP).
//!
//! QMP is line-delimited JSON over a unix socket. Replies carry the `id` of
//! the request; asynchronous events can arrive between any two replies and
//! are collected for the caller.

use serde_json::{Value, json};
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum QmpError {
    Io(io::Error),
    /// QEMU did not answer in time. The connection must not be reused.
    Timeout,
    /// QEMU closed the connection or exited.
    Closed,
    /// QEMU sent something that is not valid QMP.
    Protocol(String),
    /// QEMU understood the command and refused it.
    Command {
        class: String,
        desc: String,
    },
}

impl fmt::Display for QmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QmpError::Io(e) => write!(f, "QMP I/O error: {e}"),
            QmpError::Timeout => write!(f, "QEMU did not answer in time"),
            QmpError::Closed => write!(f, "QEMU closed the control connection"),
            QmpError::Protocol(what) => write!(f, "unexpected QMP data: {what}"),
            QmpError::Command { desc, .. } => write!(f, "{desc}"),
        }
    }
}

impl std::error::Error for QmpError {}

pub struct Qmp {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    events: Vec<Value>,
    /// How long a single command may take.
    pub command_timeout: Duration,
}

impl Qmp {
    /// Connects to QEMU's QMP socket and completes the handshake.
    ///
    /// The socket appears a moment after QEMU starts, so this retries for up
    /// to `wait`. `still_running` is polled between attempts; returning
    /// `false` (QEMU exited) ends the wait early with [`QmpError::Closed`].
    pub fn connect(path: &Path, wait: Duration, mut still_running: impl FnMut() -> bool) -> Result<Qmp, QmpError> {
        let deadline = Instant::now() + wait;
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(stream) => break stream,
                Err(_) if !still_running() => return Err(QmpError::Closed),
                Err(_) if Instant::now() >= deadline => return Err(QmpError::Timeout),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        let writer = stream.try_clone().map_err(QmpError::Io)?;
        let mut qmp = Qmp {
            reader: BufReader::new(stream),
            writer,
            next_id: 0,
            events: Vec::new(),
            command_timeout: Duration::from_secs(30),
        };
        let greeting = qmp.read_message(Instant::now() + Duration::from_secs(10))?;
        if greeting.get("QMP").is_none() {
            return Err(QmpError::Protocol(format!("expected a greeting, got {greeting}")));
        }
        qmp.execute("qmp_capabilities", None)?;
        Ok(qmp)
    }

    /// Runs one command and returns its result.
    pub fn execute(&mut self, command: &str, arguments: Option<Value>) -> Result<Value, QmpError> {
        self.run(command, arguments, None)
    }

    /// Runs one command with an open file descriptor attached, which is how
    /// QEMU's `getfd` receives the descriptor it is being given.
    pub fn execute_with_fd(
        &mut self,
        command: &str,
        arguments: Option<Value>,
        fd: BorrowedFd<'_>,
    ) -> Result<Value, QmpError> {
        self.run(command, arguments, Some(fd))
    }

    fn run(&mut self, command: &str, arguments: Option<Value>, fd: Option<BorrowedFd<'_>>) -> Result<Value, QmpError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({ "execute": command, "id": id });
        if let Some(arguments) = arguments {
            request["arguments"] = arguments;
        }
        let mut line = request.to_string();
        line.push('\n');
        match fd {
            Some(fd) => crate::fdpass::send_with_fd(&self.writer, line.as_bytes(), fd),
            None => self.writer.write_all(line.as_bytes()),
        }
        .map_err(io_error)?;

        let deadline = Instant::now() + self.command_timeout;
        loop {
            let mut message = self.read_message(deadline)?;
            if message.get("event").is_some() {
                self.events.push(message);
            } else if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(QmpError::Command {
                        class: error["class"].as_str().unwrap_or_default().to_owned(),
                        desc: error["desc"].as_str().unwrap_or("unknown error").to_owned(),
                    });
                }
                return Ok(message.get_mut("return").map(Value::take).unwrap_or(Value::Null));
            }
            // Anything else is a reply to a request we gave up on; ignore it.
        }
    }

    /// Events received so far, oldest first. Calling this clears the list.
    pub fn take_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }

    fn read_message(&mut self, deadline: Instant) -> Result<Value, QmpError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(QmpError::Timeout);
        }
        self.reader.get_ref().set_read_timeout(Some(remaining)).map_err(QmpError::Io)?;
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Err(QmpError::Closed),
            Ok(_) => serde_json::from_str(&line).map_err(|e| QmpError::Protocol(format!("{e}: {line:?}"))),
            Err(e) => Err(io_error(e)),
        }
    }
}

fn io_error(e: io::Error) -> QmpError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => QmpError::Timeout,
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof => QmpError::Closed,
        _ => QmpError::Io(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A scripted QMP server: sends the greeting, then for each request line
    /// received writes the next canned reply (with `$ID` replaced by the
    /// request id).
    fn serve(dir: &Path, replies: Vec<&'static str>) -> std::path::PathBuf {
        let path = dir.join("qmp.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            writer.write_all(b"{\"QMP\": {\"version\": {}, \"capabilities\": []}}\r\n").unwrap();
            for reply in replies {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    return;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let reply = reply.replace("$ID", &request["id"].to_string());
                writer.write_all(reply.as_bytes()).unwrap();
                writer.write_all(b"\r\n").unwrap();
            }
            // Stay silent, but keep the connection open until the client goes away.
            while reader.read_line(&mut String::new()).is_ok_and(|n| n > 0) {}
        });
        path
    }

    /// A scratch directory that is removed again when the test ends.
    struct TempDir(std::path::PathBuf);

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("roomy-qmp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    #[test]
    fn returns_results_and_collects_interleaved_events() {
        let dir = tempdir("events");
        let path = serve(
            &dir,
            vec![
                r#"{"return": {}, "id": $ID}"#,
                "{\"event\": \"STOP\", \"timestamp\": {}}\r\n{\"return\": {\"running\": false, \"status\": \"paused\"}, \"id\": $ID}",
            ],
        );
        let mut qmp = Qmp::connect(&path, Duration::from_secs(5), || true).unwrap();
        let status = qmp.execute("query-status", None).unwrap();
        assert_eq!(status["status"], "paused");
        let events = qmp.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"], "STOP");
        assert!(qmp.take_events().is_empty());
    }

    #[test]
    fn command_errors_carry_qemus_description() {
        let dir = tempdir("error");
        let path = serve(
            &dir,
            vec![
                r#"{"return": {}, "id": $ID}"#,
                r#"{"error": {"class": "GenericError", "desc": "virgl is not yet migratable"}, "id": $ID}"#,
            ],
        );
        let mut qmp = Qmp::connect(&path, Duration::from_secs(5), || true).unwrap();
        let err = qmp.execute("migrate", Some(json!({"uri": "file:/x"}))).unwrap_err();
        assert!(matches!(&err, QmpError::Command { class, .. } if class == "GenericError"));
        assert_eq!(err.to_string(), "virgl is not yet migratable");
    }

    #[test]
    fn a_silent_qemu_times_out() {
        let dir = tempdir("timeout");
        let path = serve(&dir, vec![r#"{"return": {}, "id": $ID}"#]);
        let mut qmp = Qmp::connect(&path, Duration::from_secs(5), || true).unwrap();
        qmp.command_timeout = Duration::from_millis(100);
        assert!(matches!(qmp.execute("query-status", None), Err(QmpError::Timeout)));
    }

    #[test]
    fn connect_gives_up_early_when_qemu_has_exited() {
        let dir = tempdir("gone");
        let started = Instant::now();
        let result = Qmp::connect(&dir.join("never.sock"), Duration::from_secs(30), || false);
        assert!(matches!(result, Err(QmpError::Closed)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
