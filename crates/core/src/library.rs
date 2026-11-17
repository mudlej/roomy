//! The list of virtual machines roomy knows about.
//!
//! A VM is a self-contained folder that can live anywhere; the library is
//! only a list of where those folders are. Removing an entry never touches
//! the folder, and deleting a VM's files removes only what roomy put there.

use crate::control::{self, State};
use crate::env::{self, Env};
use crate::error::Context;
use crate::vm::{self, Folder, remove_if_present};
use crate::{Result, err, process};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a supervisor may take to finish after its VM stopped.
const SUPERVISOR_EXIT: Duration = Duration::from_secs(10);

const SCHEMA: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    schema: u32,
    #[serde(default)]
    folders: Vec<PathBuf>,
}

/// The library file for the current user: `$XDG_CONFIG_HOME/roomy/library.toml`,
/// by default `~/.config/roomy/library.toml`.
pub fn default_path() -> Result<PathBuf> {
    let config_home = match env::dir_from_env("XDG_CONFIG_HOME") {
        Some(dir) => dir,
        None => env::home_dir()?.join(".config"),
    };
    Ok(config_home.join("roomy").join("library.toml"))
}

/// Where new VMs go unless the user picks another place: `~/VMs`.
pub fn default_vm_parent() -> Result<PathBuf> {
    Ok(env::home_dir()?.join("VMs"))
}

/// The folders in the library, in the order they were added. A library that
/// does not exist yet is empty; one that cannot be read is an error, so a
/// damaged file is never mistaken for "no VMs" and overwritten.
pub fn folders(library: &Path) -> Result<Vec<PathBuf>> {
    let text = match fs::read_to_string(library) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(err!("cannot read {}: {e}", library.display())),
    };
    let stored: Stored = toml::from_str(&text).context(format!("{} is not a valid library file", library.display()))?;
    if stored.schema > SCHEMA {
        return Err(err!("{} was written by a newer version of roomy", library.display()));
    }
    Ok(stored.folders)
}

fn store(library: &Path, folders: Vec<PathBuf>) -> Result<()> {
    if let Some(parent) = library.parent() {
        fs::create_dir_all(parent).context(format!("cannot create {}", parent.display()))?;
    }
    let text = toml::to_string(&Stored { schema: SCHEMA, folders }).expect("the library always serializes");
    vm::write_atomically(library, text.as_bytes())
}

/// The form in which folders are stored and compared.
fn canonical(folder: &Path) -> PathBuf {
    fs::canonicalize(folder).or_else(|_| std::path::absolute(folder)).unwrap_or_else(|_| folder.to_path_buf())
}

/// Adds a VM folder to the library. Returns whether it was new.
pub fn add(library: &Path, folder: &Path) -> Result<bool> {
    let folder = Folder::open(folder)?;
    folder.load()?;
    let dir = canonical(folder.dir());
    let mut all = folders(library)?;
    if all.contains(&dir) {
        return Ok(false);
    }
    all.push(dir);
    store(library, all)?;
    Ok(true)
}

/// Takes a folder off the list without touching it. Returns whether it was listed.
pub fn forget(library: &Path, folder: &Path) -> Result<bool> {
    let dir = canonical(folder);
    let mut all = folders(library)?;
    let before = all.len();
    all.retain(|listed| *listed != dir && listed != folder);
    if all.len() == before {
        return Ok(false);
    }
    store(library, all)?;
    Ok(true)
}

/// Deletes a VM: its disk, saved state, firmware settings, logs and settings
/// file, and takes it off the list. Only files roomy created are removed;
/// anything else found in the folder is left alone and returned.
pub fn delete(env: &Env, library: &Path, folder: &Folder) -> Result<Vec<PathBuf>> {
    let state = control::status(env, folder);
    if !matches!(state, State::Off | State::Suspended) {
        return Err(err!("the virtual machine is {}; turn it off before deleting it", state.word()));
    }
    // Holding the lock means no supervisor can start on a folder that is being taken apart. A
    // lock file that cannot even be opened is no obstacle: then nothing can be running there.
    // A supervisor whose VM has just stopped stops answering a moment before it lets go of the lock.
    let mut attempt = folder.try_lock();
    if matches!(attempt, Ok(None)) {
        process::wait_until(SUPERVISOR_EXIT, || {
            attempt = folder.try_lock();
            !matches!(attempt, Ok(None))
        });
    }
    let lock = match attempt {
        Ok(None) => return Err(err!("the virtual machine is in use; turn it off before deleting it")),
        Ok(Some(lock)) => Some(lock),
        Err(_) => None,
    };

    let dir = folder.dir();
    let mut files = vec![folder.firmware_vars(), folder.lock_path(), folder.thumbnail()];
    if let Ok(config) = folder.load() {
        let disk = folder.disk_path(&config);
        // A disk outside the folder was not created by roomy for this VM.
        if disk.parent() == Some(dir) {
            files.push(disk);
        }
    }
    // The settings file goes last: while it exists the folder is still recognisable as a VM.
    files.push(folder.config_path());
    let directories = [folder.saved_state().parent().map(Path::to_path_buf), Some(folder.log_dir())];
    for path in directories.into_iter().flatten().chain(files) {
        remove_if_present(&path).context(format!("cannot delete {}", path.display()))?;
    }
    drop(lock);

    let left = env::sorted_entries(dir);
    if left.is_empty() {
        fs::remove_dir(dir).context(format!("cannot remove the folder {}", dir.display()))?;
    }
    forget(library, dir)?;
    Ok(left)
}
