//! Changing a VM's settings after it was created.
//!
//! Some settings can change while the guest runs; the rest describe the
//! guest's hardware and are read when it boots. Every change is reported
//! with which of the two it was, so the user is never left guessing whether
//! a restart is needed.

use crate::control::{self, Request, State};
use crate::env::Env;
use crate::vm::{self, Config, Folder, PortForward, SharedFolder};
use crate::{Result, err};

/// When a change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// Already in effect.
    Now,
    /// When the guest next boots (a restart from inside the guest is not enough:
    /// the virtual machine itself has to be shut down and started).
    NextBoot,
}

/// Which settings take effect at once on a running guest, and which wait.
pub mod timing {
    use super::Applies;
    pub const NAME: Applies = Applies::Now;
    pub const NETWORK: Applies = Applies::Now;
    pub const CLIPBOARD: Applies = Applies::Now;
    pub const MICROPHONE: Applies = Applies::Now;
    pub const DISK_SIZE: Applies = Applies::Now;
    pub const PROCESSORS: Applies = Applies::NextBoot;
    pub const MEMORY: Applies = Applies::NextBoot;
    pub const GPU_ACCELERATION: Applies = Applies::NextBoot;
    pub const SHARED_FOLDERS: Applies = Applies::NextBoot;
    pub const PORT_FORWARDS: Applies = Applies::NextBoot;
}

/// What to change; everything left at its default stays as it is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    pub name: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u64>,
    pub gpu_acceleration: Option<bool>,
    pub network: Option<bool>,
    pub clipboard: Option<bool>,
    pub microphone: Option<bool>,
    /// The new complete list of shared folders.
    pub shared_folders: Option<Vec<SharedFolder>>,
    /// The new complete list of forwarded ports.
    pub port_forwards: Option<Vec<PortForward>>,
    /// A larger disk size. Disks cannot shrink.
    pub disk_gb: Option<u64>,
}

/// One setting that was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub setting: &'static str,
    pub applies: Applies,
    /// Something the user should know about this change.
    pub note: Option<String>,
}

/// The size of the VM's disk as the guest sees it, in whole GB (rounded up).
pub fn disk_gb(env: &Env, folder: &Folder, config: &Config) -> Result<u64> {
    let qemu_img = env
        .find_program("qemu-img")
        .ok_or_else(|| err!("qemu-img is not installed (run: {} setup)", crate::env::roomy_command()))?;
    let bytes = vm::disk_virtual_size(&qemu_img, &folder.disk_path(config))?;
    Ok(bytes.div_ceil(1 << 30))
}

/// Carries out `changes`. Settings equal to the current ones are skipped.
/// Nothing is saved unless every change is valid and every immediate change worked.
pub fn apply(env: &Env, folder: &Folder, changes: &Changes) -> Result<Vec<Applied>> {
    let mut config = folder.load()?;
    let state = control::status(env, folder);
    if matches!(state, State::Starting | State::Suspending | State::Stopping) {
        return Err(err!("the virtual machine is {}; try again in a moment", state.word()));
    }
    let running = matches!(state, State::Running | State::Paused);
    // A suspended guest continues with the hardware it was suspended with.
    let hardware = if state == State::Off { Applies::Now } else { Applies::NextBoot };
    let mut applied = Vec::new();
    let mut done = |setting, applies, note: Option<&str>| {
        applied.push(Applied { setting, applies, note: note.map(str::to_owned) })
    };

    macro_rules! set {
        ($field:expr, $new:expr, $name:literal, $applies:expr) => {
            if let Some(new) = $new {
                if $field != *new {
                    $field = new.clone();
                    done($name, $applies, None);
                }
            }
        };
    }
    set!(config.name, &changes.name.as_ref().map(|n| n.trim().to_owned()), "name", Applies::Now);
    set!(config.cpus, &changes.cpus, "processors", hardware);
    set!(config.memory_mb, &changes.memory_mb, "memory", hardware);
    set!(config.features.gpu_acceleration, &changes.gpu_acceleration, "GPU acceleration", hardware);
    set!(config.shared_folders, &changes.shared_folders, "shared folders", hardware);
    set!(config.port_forwards, &changes.port_forwards, "forwarded ports", hardware);
    set!(config.features.clipboard, &changes.clipboard, "clipboard sharing", Applies::Now);
    set!(config.features.microphone, &changes.microphone, "microphone", Applies::Now);
    let network_changed = changes.network.is_some_and(|on| on != config.features.network);
    if let Some(on) = changes.network.filter(|_| network_changed) {
        config.features.network = on;
        done("network", Applies::Now, None);
    }
    config.validate()?;
    for share in &config.shared_folders {
        if !share.path.is_dir() {
            return Err(err!("the shared folder {} does not exist", share.path.display()));
        }
    }

    // Everything is valid: now the changes that act on the running guest or the disk.
    let grow_to = match changes.disk_gb {
        Some(wanted) => {
            let current = disk_gb(env, folder, &config)?;
            if wanted < current {
                return Err(err!("the disk is {current} GB and cannot be made smaller ({wanted} GB was asked for)"));
            }
            (wanted > current).then_some(wanted)
        }
        None => None,
    };
    if grow_to.is_some() && state == State::Suspended {
        return Err(err!(
            "the disk cannot be resized while the virtual machine is suspended; continue it or discard its saved state first"
        ));
    }
    if running && network_changed {
        control::request(env, folder, Request::SetNetwork { on: config.features.network })?;
    }
    if let Some(size_gb) = grow_to {
        let note = "The guest still has to grow its partition and file system to use the new space.";
        if running {
            control::request(env, folder, Request::GrowDisk { size_gb })?;
        } else {
            let qemu_img = env
                .find_program("qemu-img")
                .ok_or_else(|| err!("qemu-img is not installed (run: {} setup)", crate::env::roomy_command()))?;
            let disk = folder.disk_path(&config);
            vm::run_tool(&qemu_img, &["resize", "-q", &disk.to_string_lossy(), &format!("{size_gb}G")])?;
        }
        done("disk size", Applies::Now, Some(note));
    }
    folder.save(&config)?;
    Ok(applied)
}

/// The settings that were changed since the guest booted and are waiting for
/// its next boot. Empty when the VM is off or nothing is waiting.
pub fn waiting_for_next_boot(env: &Env, folder: &Folder) -> Vec<&'static str> {
    let booted = match control::running(env, folder) {
        Some(running) => running.config,
        // A suspended guest continues with the settings saved next to its state.
        None if folder.is_suspended() => match Folder::load_from(&folder.saved_config()) {
            Ok(config) => config,
            Err(_) => return Vec::new(),
        },
        None => return Vec::new(),
    };
    let Ok(wanted) = folder.load() else { return Vec::new() };
    let mut waiting = Vec::new();
    let mut check = |differs: bool, setting| {
        if differs {
            waiting.push(setting)
        }
    };
    check(booted.cpus != wanted.cpus, "processors");
    check(booted.memory_mb != wanted.memory_mb, "memory");
    check(booted.features.gpu_acceleration != wanted.features.gpu_acceleration, "GPU acceleration");
    check(booted.shared_folders != wanted.shared_folders, "shared folders");
    check(booted.port_forwards != wanted.port_forwards, "forwarded ports");
    waiting
}
