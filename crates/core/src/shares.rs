//! Shared folders: how roomy runs the virtiofsd daemon.

use std::path::Path;

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
