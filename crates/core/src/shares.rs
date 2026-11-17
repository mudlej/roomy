//! Shared folders: how roomy runs the virtiofsd daemon.

use crate::host::Version;
use crate::process::{KillOnDrop, spawn_logged, wait_until};
use std::io;
use std::path::Path;
use std::time::Duration;

/// Oldest virtiofsd roomy asks distributions for.
pub const MIN_VIRTIOFSD: Version = Version { major: 1, minor: 13, micro: 0 };

/// The user ID that the first regular user gets on nearly every Linux
/// distribution. Host files show up in the guest as owned by this user.
pub const DEFAULT_GUEST_UID: u32 = 1000;

/// Command-line arguments for one shared folder's daemon.
///
/// The configuration was validated in `lab/` on virtiofsd 1.14:
/// - runs without root in virtiofsd's own namespace sandbox;
/// - every guest user acts as the host user, so files never end up owned by
///   an ID the host user cannot manage, and the host user's files appear in
///   the guest as owned by `guest_uid`;
/// - suspend-to-disk reopens files by path, checks that each is still the
///   same file, and reports per-file errors to the guest instead of failing
///   the whole restore when the folder changed while suspended.
pub fn virtiofsd_args(socket: &Path, shared_dir: &Path, host_uid: u32, host_gid: u32, guest_uid: u32) -> Vec<String> {
    const ALL_IDS: u32 = u32::MAX;
    vec![
        "--socket-path".into(),
        socket.display().to_string(),
        "--shared-dir".into(),
        shared_dir.display().to_string(),
        "--sandbox".into(),
        "namespace".into(),
        "--translate-uid".into(),
        format!("squash-guest:0:{host_uid}:{ALL_IDS}"),
        "--translate-gid".into(),
        format!("squash-guest:0:{host_gid}:{ALL_IDS}"),
        "--translate-uid".into(),
        format!("host:{host_uid}:{guest_uid}:1"),
        "--translate-gid".into(),
        format!("host:{host_gid}:{guest_uid}:1"),
        "--migration-mode=find-paths".into(),
        "--migration-on-error=guest-error".into(),
        "--migration-verify-handles".into(),
        "--migration-confirm-paths".into(),
    ]
}

/// Why a shared folder's daemon is not running.
#[derive(Debug)]
pub enum DaemonFailure {
    /// The program could not be run at all.
    CannotRun(io::Error),
    /// It ran, but ended or never opened its socket; its log says why.
    DidNotStart,
}

/// Starts virtiofsd for the folder `shared_dir` with its output appended to `log`, and waits
/// until it listens on `socket`. Files of this computer's user show up in the guest as owned
/// by [`DEFAULT_GUEST_UID`].
pub fn start_daemon(
    virtiofsd: &Path,
    socket: &Path,
    shared_dir: &Path,
    log: &Path,
) -> Result<KillOnDrop, DaemonFailure> {
    // SAFETY: getuid and getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let args = virtiofsd_args(socket, shared_dir, uid, gid, DEFAULT_GUEST_UID);
    let mut daemon = KillOnDrop(spawn_logged(virtiofsd, &args, log).map_err(DaemonFailure::CannotRun)?);
    let up = wait_until(Duration::from_secs(8), || socket.exists() || !daemon.running());
    if up && daemon.running() { Ok(daemon) } else { Err(DaemonFailure::DidNotStart) }
}
