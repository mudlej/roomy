//! Downloading an image into a folder and checking it.
//!
//! curl writes to `<name>.part`; only once the whole file is there and its checksum matches is it
//! renamed to its real name, so a file under that name is always complete and genuine. A
//! download that is stopped keeps its `.part`, and the next one carries on from there.

use super::{Checksum, Image};
use crate::env::Env;
use crate::process;
use crate::{Result, err};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How a download is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// `done` bytes are on disk; `total` when the size is known.
    Fetching {
        done: u64,
        total: Option<u64>,
    },
    /// All of it arrived and its checksum is being worked out.
    Checking,
    /// Checked and saved here.
    Done(PathBuf),
    /// Stopped at the user's request; what arrived is kept for the next try.
    Stopped,
    Failed(String),
}

impl Progress {
    pub fn finished(&self) -> bool {
        matches!(self, Progress::Done(_) | Progress::Stopped | Progress::Failed(_))
    }
}

/// A download running in the background.
pub struct Download {
    target: PathBuf,
    partial: PathBuf,
    total: Option<u64>,
    shared: Arc<Shared>,
}

struct Shared {
    /// `None` while curl is fetching; the size of what is on disk is read when asked.
    phase: Mutex<Option<Progress>>,
    stop: AtomicBool,
    /// The running curl, so stopping does not have to wait for it.
    curl: AtomicI32,
}

/// Where an image is saved in `dir`.
pub fn target(dir: &Path, image: &Image) -> PathBuf {
    dir.join(&image.file_name)
}

impl Download {
    /// Starts downloading `image` into `dir` (created if missing). A file already there under the
    /// image's name counts as done once its checksum matches.
    pub fn start(env: &Env, image: &Image, dir: &Path) -> Result<Download> {
        let curl = env.require("curl")?;
        let summer = env.require(image.checksum.program())?;
        std::fs::create_dir_all(dir).map_err(|e| err!("cannot create {}: {e}", dir.display()))?;
        let target = target(dir, image);
        let partial = dir.join(format!("{}.part", image.file_name));
        let shared =
            Arc::new(Shared { phase: Mutex::new(None), stop: AtomicBool::new(false), curl: AtomicI32::new(0) });
        let job = Job {
            curl,
            summer,
            url: image.url.clone(),
            checksum: image.checksum.clone(),
            total: image.size,
            target: target.clone(),
            partial: partial.clone(),
            log: env.runtime_dir.join(format!("roomy-download-{}.log", image.file_name)),
            shared: shared.clone(),
        };
        std::thread::Builder::new()
            .name("download".into())
            .spawn(move || {
                let outcome = job.run();
                let _ = std::fs::remove_file(&job.log);
                *job.shared.phase.lock().unwrap() = Some(outcome);
            })
            .map_err(|e| err!("cannot start the download: {e}"))?;
        Ok(Download { target, partial, total: image.size, shared })
    }

    pub fn progress(&self) -> Progress {
        if let Some(phase) = self.shared.phase.lock().unwrap().clone() {
            return phase;
        }
        let done = std::fs::metadata(&self.partial).map(|m| m.len()).unwrap_or(0);
        Progress::Fetching { done, total: self.total }
    }

    /// Stops the download, keeping what has arrived.
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let pid = self.shared.curl.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: plain signal to the curl this download started; it is reaped by its thread.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }

    /// Where the image is saved once done.
    pub fn target(&self) -> &Path {
        &self.target
    }
}

struct Job {
    curl: PathBuf,
    summer: PathBuf,
    url: String,
    checksum: Checksum,
    total: Option<u64>,
    target: PathBuf,
    partial: PathBuf,
    log: PathBuf,
    shared: Arc<Shared>,
}

impl Job {
    fn run(&self) -> Progress {
        if self.target.exists() {
            self.set(Progress::Checking);
            return match self.matches(&self.target) {
                Ok(true) => Progress::Done(self.target.clone()),
                Ok(false) => Progress::Failed(format!(
                    "{} is already in {}, but it is not the current image; move it away and try again",
                    file_name(&self.target),
                    folder(&self.target)
                )),
                Err(e) => Progress::Failed(e.to_string()),
            };
        }
        let have = std::fs::metadata(&self.partial).map(|m| m.len()).unwrap_or(0);
        // Asking a server to resume a file that is already whole makes some of them answer with an error.
        let complete = self.total.is_some_and(|total| have == total);
        if !complete {
            if let Err(problem) = self.fetch() {
                return problem;
            }
        }
        self.set(Progress::Checking);
        match self.matches(&self.partial) {
            Ok(true) => match std::fs::rename(&self.partial, &self.target) {
                Ok(()) => Progress::Done(self.target.clone()),
                Err(e) => Progress::Failed(format!("cannot name the image {}: {e}", file_name(&self.target))),
            },
            Ok(false) => {
                let _ = std::fs::remove_file(&self.partial);
                Progress::Failed(
                    "the downloaded file is not the image its project describes (its checksum does not match), so it was deleted; try again".into(),
                )
            }
            Err(e) => Progress::Failed(e.to_string()),
        }
    }

    /// Runs curl until the file is whole. `Err` holds how it ended otherwise.
    fn fetch(&self) -> std::result::Result<(), Progress> {
        let partial = self.partial.to_string_lossy();
        #[rustfmt::skip]
        let args = [
            "--fail", "--location", "--silent", "--show-error",
            "--proto", "=https", "--proto-redir", "=https",
            "--retry", "5", "--retry-delay", "3", "--connect-timeout", "30",
            // Give up on a connection that stalls for a minute rather than hang forever.
            "--speed-limit", "1024", "--speed-time", "60",
            "--continue-at", "-", "--output", &partial, &self.url,
        ];
        let log = std::fs::File::create(&self.log).map_err(|e| Progress::Failed(format!("cannot start curl: {e}")))?;
        let errors = log.try_clone().map_err(|e| Progress::Failed(format!("cannot start curl: {e}")))?;
        let mut command = Command::new(&self.curl);
        command.args(args).stdin(Stdio::null()).stdout(log).stderr(errors);
        // If roomy goes away, so does curl: it never keeps writing to the Downloads folder unwatched.
        // SAFETY: prctl is async-signal-safe and touches no memory shared with the parent.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|e| Progress::Failed(format!("cannot start curl: {e}")))?;
        self.shared.curl.store(child.id() as i32, Ordering::SeqCst);
        if self.shared.stop.load(Ordering::SeqCst) {
            process::kill_and_reap(&mut child);
        }
        let status = child.wait();
        self.shared.curl.store(0, Ordering::SeqCst);
        if self.shared.stop.load(Ordering::SeqCst) {
            return Err(Progress::Stopped);
        }
        match status {
            Ok(status) if status.success() => Ok(()),
            _ => {
                let said = process::read_tail(&self.log, 3);
                let said = said.lines().map(|line| line.trim_start_matches("curl: ")).collect::<Vec<_>>().join("; ");
                Err(Progress::Failed(format!("the download did not finish: {said}")))
            }
        }
    }

    /// Whether the file's checksum is the expected one.
    fn matches(&self, file: &Path) -> Result<bool> {
        let file_arg = file.to_string_lossy();
        // A large image takes a while to read; a slow disk gets plenty of time.
        let ran = process::run_capture(&self.summer, &[&file_arg], Duration::from_secs(3600))
            .map_err(|e| err!("cannot check {}: {e}", file_name(file)))?;
        if !ran.status.success() {
            return Err(err!("cannot check {}: {}", file_name(file), ran.stderr.trim()));
        }
        let computed = ran.stdout.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
        Ok(computed == self.checksum.hex())
    }

    fn set(&self, progress: Progress) {
        *self.shared.phase.lock().unwrap() = Some(progress);
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn folder(path: &Path) -> String {
    path.parent().map(|p| p.display().to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    const CONTENT: &str = "pretend installation image\n";
    // sha256 of CONTENT.
    const RIGHT: &str = "9f1a1e0d0d1a3e8d31f1e2e7a83c9c0c5d4e0d46ba0db0e5d1b7a9c5ac6a2c6a";

    /// A host whose curl writes `CONTENT` to the file it is asked to fill, and counts its runs.
    fn host(dir: &Path) -> Env {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let curl = bin.join("curl");
        let script = format!(
            "#!/bin/sh\necho run >> '{}'\nwhile [ $# -gt 0 ]; do [ \"$1\" = --output ] && out=\"$2\"; shift; done\nprintf '%s' '{CONTENT}' > \"$out\"\n",
            dir.join("curl-runs").display()
        );
        std::fs::write(&curl, script).unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        Env::rooted("/", vec![bin, "/usr/bin".into(), "/bin".into()], dir)
    }

    fn image(checksum: &str) -> Image {
        Image {
            version: "1".into(),
            file_name: "pretend-1-amd64.iso".into(),
            url: "https://example.org/pretend-1-amd64.iso".into(),
            size: Some(CONTENT.len() as u64),
            checksum: Checksum::from_hex(checksum).unwrap(),
        }
    }

    fn finish(download: &Download) -> Progress {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let progress = download.progress();
            if progress.finished() || Instant::now() > deadline {
                return progress;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn real_checksum(dir: &Path) -> String {
        let file = dir.join("content");
        std::fs::write(&file, CONTENT).unwrap();
        let ran =
            process::run_capture(Path::new("/usr/bin/sha256sum"), &[&file.to_string_lossy()], Duration::from_secs(10))
                .unwrap();
        ran.stdout.split_whitespace().next().unwrap().to_owned()
    }

    #[test]
    fn an_image_is_named_only_once_its_checksum_matches() {
        let dir = TempDir::new("dl-ok");
        let env = host(&dir);
        let downloads = dir.join("Downloads");
        let right = real_checksum(&dir);
        let done = finish(&Download::start(&env, &image(&right), &downloads).unwrap());
        assert_eq!(done, Progress::Done(downloads.join("pretend-1-amd64.iso")));
        assert_eq!(std::fs::read_to_string(downloads.join("pretend-1-amd64.iso")).unwrap(), CONTENT);
        assert!(!downloads.join("pretend-1-amd64.iso.part").exists());

        // Asked again, the file already there is checked rather than fetched again.
        let again = finish(&Download::start(&env, &image(&right), &downloads).unwrap());
        assert_eq!(again, Progress::Done(downloads.join("pretend-1-amd64.iso")));
        assert_eq!(std::fs::read_to_string(dir.join("curl-runs")).unwrap().lines().count(), 1);
    }

    #[test]
    fn a_file_that_does_not_match_is_thrown_away() {
        let dir = TempDir::new("dl-bad");
        let env = host(&dir);
        let downloads = dir.join("Downloads");
        assert_ne!(real_checksum(&dir), RIGHT);
        let outcome = finish(&Download::start(&env, &image(RIGHT), &downloads).unwrap());
        assert!(matches!(&outcome, Progress::Failed(why) if why.contains("checksum")), "{outcome:?}");
        assert!(!downloads.join("pretend-1-amd64.iso").exists());
        assert!(!downloads.join("pretend-1-amd64.iso.part").exists());
    }
}
