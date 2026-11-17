//! roomy's engine. Everything here is UI-free so it can be tested headless.

pub mod agent;
pub mod catalog;
pub mod control;
pub mod doctor;
pub mod env;
pub mod error;
pub mod fdpass;
pub mod host;
pub mod launcher;
pub mod library;
pub mod process;
pub mod qemu;
pub mod qmp;
pub mod settings;
pub mod setup;
pub mod shares;
pub mod supervisor;
pub mod vm;

/// Oldest QEMU roomy supports. Suspend-to-disk with the clipboard device
/// attached needs the vdagent migration support that arrived in 10.1.
pub const MIN_QEMU: host::Version = host::Version { major: 10, minor: 1, micro: 0 };

pub use error::{Error, Result};
