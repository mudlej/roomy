//! A virtual machine on disk.
//!
//! Each VM is one self-contained folder that can be moved or copied while the
//! VM is off:
//!
//! ```text
//! <folder>/
//!   roomy.toml             settings (this module's `Config`)
//!   disk.qcow2             virtual disk, grows as the guest writes
//!   firmware-vars.fd       the VM's UEFI variable store
//!   state/suspend.state    guest memory saved by Suspend, present only while suspended
//!   state/suspend.toml     the settings the suspended guest was started with
//!   logs/                  output of QEMU and helpers from the latest run
//! ```

use crate::env::Env;
use crate::error::Context;
use crate::host;
use crate::process::run_capture;
use crate::{Result, err, qemu};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const CONFIG_FILE: &str = "roomy.toml";
pub const SCHEMA: u32 = 1;
const DISK_FILE: &str = "disk.qcow2";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Format version of this file.
    pub schema: u32,
    /// Stable identity (a UUID); also the guest's SMBIOS UUID.
    pub id: String,
    pub name: String,
    /// Guest operating system, as an identifier from roomy's catalog.
    pub os: String,
    /// Versioned QEMU machine type, fixed at creation so that the guest's
    /// virtual hardware does not change when QEMU is updated.
    pub machine: String,
    pub cpus: u32,
    pub memory_mb: u64,
    /// File name of the virtual disk inside the VM folder.
    pub disk: String,
    /// ISO the guest can boot from while its disk is still empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_image: Option<PathBuf>,
    /// The network card's hardware address.
    pub mac: String,
    #[serde(default)]
    pub features: Features,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared_folders: Vec<SharedFolder>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub port_forwards: Vec<PortForward>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Features {
    pub gpu_acceleration: bool,
    pub network: bool,
    pub clipboard: bool,
    pub microphone: bool,
}

impl Default for Features {
    fn default() -> Features {
        Features { gpu_acceleration: false, network: true, clipboard: true, microphone: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedFolder {
    /// The name the guest mounts the folder by.
    pub name: String,
    /// The folder on this computer.
    pub path: PathBuf,
}

/// Makes a guest TCP port reachable on this computer (127.0.0.1 only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortForward {
    pub host: u16,
    pub guest: u16,
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let schema =
            text.parse::<toml::Table>().ok().and_then(|table| table.get("schema").and_then(toml::Value::as_integer));
        if let Some(found) = schema.filter(|found| *found > i64::from(SCHEMA)) {
            return Err(err!(
                "this virtual machine was saved by a newer version of roomy (format {found}; this version reads up to {SCHEMA})"
            ));
        }
        let config: Config = toml::from_str(text).map_err(|e| err!("{}", e.to_string().trim()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("Config always serializes")
    }

    /// Checks the settings for values no VM can start with.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(err!("the virtual machine needs a name"));
        }
        if self.cpus == 0 {
            return Err(err!("the virtual machine needs at least 1 processor"));
        }
        if self.memory_mb < 256 {
            return Err(err!("the virtual machine needs at least 256 MB of memory"));
        }
        if self.disk.is_empty() || self.disk.contains('/') {
            return Err(err!("the disk must be a file inside the virtual machine's folder, not '{}'", self.disk));
        }
        for (i, share) in self.shared_folders.iter().enumerate() {
            let valid = !share.name.is_empty()
                && share.name.len() <= 36
                && share.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !valid {
                return Err(err!(
                    "shared folder name '{}' is not usable: use 1 to 36 letters, digits, '-' or '_'",
                    share.name
                ));
            }
            if self.shared_folders[..i].iter().any(|other| other.name == share.name) {
                return Err(err!("two shared folders are both named '{}'", share.name));
            }
        }
        Ok(())
    }
}

/// What the user chooses when creating a VM.
#[derive(Debug, Clone)]
pub struct NewVm {
    pub name: String,
    pub os: String,
    pub cpus: u32,
    pub memory_mb: u64,
    /// Maximum size of the virtual disk.
    pub disk_gb: u64,
    pub install_image: Option<PathBuf>,
    /// Start from a copy of this qcow2 image instead of an empty disk.
    pub existing_disk: Option<PathBuf>,
    pub features: Features,
    pub shared_folders: Vec<SharedFolder>,
    pub port_forwards: Vec<PortForward>,
}

/// Handle to a VM folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    dir: PathBuf,
}

impl Folder {
    /// Opens an existing VM folder.
    pub fn open(dir: &Path) -> Result<Folder> {
        let dir = dir.canonicalize().context(format!("cannot open {}", dir.display()))?;
        if !dir.join(CONFIG_FILE).is_file() {
            return Err(err!("{} is not a roomy virtual machine (no {CONFIG_FILE} inside)", dir.display()));
        }
        Ok(Folder { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    pub fn disk_path(&self, config: &Config) -> PathBuf {
        self.dir.join(&config.disk)
    }

    pub fn firmware_vars(&self) -> PathBuf {
        self.dir.join("firmware-vars.fd")
    }

    /// The image format of this VM's firmware variables, read from the file itself.
    ///
    /// Distributions ship the template it is copied from as a plain image or as qcow2, so a VM
    /// made on one computer and moved to another can differ from what the firmware installed
    /// there would suggest. `None` when the file cannot be read.
    pub fn firmware_vars_format(&self) -> Option<&'static str> {
        let mut magic = [0u8; 4];
        let mut file = File::open(self.firmware_vars()).ok()?;
        let read = file.read(&mut magic).ok()?;
        Some(if read == magic.len() && magic == *b"QFI\xfb" { "qcow2" } else { "raw" })
    }

    pub fn log_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join(".lock")
    }

    /// Takes the folder's lock, which whoever runs or takes apart the VM holds for as long as
    /// that lasts. `Ok(None)` when someone else holds it. The lock goes with the returned file.
    pub fn try_lock(&self) -> Result<Option<File>> {
        let path = self.lock_path();
        let file =
            File::options().create(true).append(true).open(&path).context(format!("cannot open {}", path.display()))?;
        // SAFETY: flock on a descriptor we own; the lock is released when the file is closed.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        Ok(locked.then_some(file))
    }

    /// A small picture of the screen as the window last showed it, for lists of VMs.
    pub fn thumbnail(&self) -> PathBuf {
        self.dir.join("thumbnail.png")
    }

    fn state_dir(&self) -> PathBuf {
        self.dir.join("state")
    }

    /// Guest memory saved by Suspend. Its existence is what makes a VM "suspended".
    pub fn saved_state(&self) -> PathBuf {
        self.state_dir().join("suspend.state")
    }

    /// Where a suspend writes before it is complete.
    pub fn saved_state_partial(&self) -> PathBuf {
        self.state_dir().join("suspend.state.partial")
    }

    /// A saved state that a restore has loaded and must never load again.
    pub fn saved_state_consumed(&self) -> PathBuf {
        self.state_dir().join("suspend.state.consumed")
    }

    /// The settings the suspended guest was started with.
    pub fn saved_config(&self) -> PathBuf {
        self.state_dir().join("suspend.toml")
    }

    pub fn is_suspended(&self) -> bool {
        self.saved_state().is_file()
    }

    pub fn load(&self) -> Result<Config> {
        Self::load_from(&self.config_path())
    }

    pub fn load_from(path: &Path) -> Result<Config> {
        let text = fs::read_to_string(path).context(format!("cannot read {}", path.display()))?;
        Config::parse(&text).context(format!("{} is not valid", path.display()))
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        config.validate()?;
        write_atomically(&self.config_path(), config.to_toml().as_bytes())
    }

    /// Throws away a suspended guest's saved memory. The next start is a
    /// normal boot, as after a power cut at the moment of suspending.
    pub fn discard_saved_state(&self) -> Result<()> {
        remove_files(&[
            self.saved_state(),
            self.saved_state_partial(),
            self.saved_state_consumed(),
            self.saved_config(),
        ])
    }

    /// Removes leftovers of a suspend or restore that was interrupted.
    pub fn clean_interrupted_state(&self) -> Result<()> {
        remove_files(&[self.saved_state_partial(), self.saved_state_consumed()])?;
        if !self.is_suspended() {
            remove_files(&[self.saved_config()])?;
        }
        Ok(())
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [self.state_dir(), self.log_dir()] {
            fs::create_dir_all(&dir).context(format!("cannot create {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Creates a new VM folder under `parent`, named after the VM.
pub fn create(env: &Env, parent: &Path, new: NewVm) -> Result<Folder> {
    let qemu_path = env.require(host::QEMU_PROGRAM)?;
    let qemu_img = env.require("qemu-img")?;
    let firmware = host::require_firmware(env)?;

    let config = Config {
        schema: SCHEMA,
        id: random_uuid()?,
        name: new.name.trim().to_owned(),
        os: new.os,
        machine: qemu::default_machine(&qemu_path)?,
        cpus: new.cpus,
        memory_mb: new.memory_mb,
        disk: DISK_FILE.to_owned(),
        install_image: new.install_image,
        mac: random_mac()?,
        features: new.features,
        shared_folders: new.shared_folders,
        port_forwards: new.port_forwards,
    };
    config.validate()?;
    if new.disk_gb == 0 {
        return Err(err!("the disk needs a size of at least 1 GB"));
    }
    if let Some(image) = &config.install_image {
        if !image.is_file() {
            return Err(err!("installation image {} does not exist", image.display()));
        }
    }

    fs::create_dir_all(parent).context(format!("cannot create {}", parent.display()))?;
    let dir = parent.join(folder_name(&config.name));
    fs::create_dir(&dir).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => err!("{} already exists; choose another name or location", dir.display()),
        _ => err!("cannot create {}: {e}", dir.display()),
    })?;
    let folder = Folder { dir: dir.canonicalize().context("cannot resolve the new folder")? };

    let populate = || -> Result<()> {
        folder.ensure_dirs()?;
        fs::copy(&firmware.vars_template, folder.firmware_vars())
            .context(format!("cannot copy firmware variables from {}", firmware.vars_template.display()))?;
        let disk = folder.disk_path(&config);
        let size = format!("{}G", new.disk_gb);
        match &new.existing_disk {
            Some(source) => {
                check_importable(&qemu_img, source)?;
                fs::copy(source, &disk).context(format!("cannot copy disk image {}", source.display()))?;
                let current = disk_virtual_size(&qemu_img, &disk)?;
                if new.disk_gb << 30 > current {
                    run_tool(&qemu_img, &["resize", "-f", "qcow2", &disk.to_string_lossy(), &size])?;
                }
            }
            None => run_tool(&qemu_img, &["create", "-q", "-f", "qcow2", &disk.to_string_lossy(), &size])?,
        }
        folder.save(&config)
    };
    if let Err(e) = populate() {
        let _ = fs::remove_dir_all(folder.dir());
        return Err(e);
    }
    Ok(folder)
}

/// A VM's disk can start as a copy of `source` when that is one complete qcow2 file: the copy
/// is used as qcow2, and it is all that is copied.
fn check_importable(qemu_img: &Path, source: &Path) -> Result<()> {
    let info = disk_info(qemu_img, source)?;
    let format = info["format"].as_str().unwrap_or("unknown");
    let shown = source.display();
    if format != "qcow2" {
        return Err(err!(
            "{shown} is a {format} disk image, and roomy starts from qcow2 images. Convert it first: \
             qemu-img convert -O qcow2 '{shown}' converted.qcow2"
        ));
    }
    if let Some(base) = info["backing-filename"].as_str() {
        return Err(err!(
            "{shown} only holds the changes made to another image ({base}). Make a complete image of it first: \
             qemu-img convert -O qcow2 '{shown}' complete.qcow2"
        ));
    }
    Ok(())
}

/// What qemu-img says about an image. `-U` lets this work while a running VM holds the image.
fn disk_info(qemu_img: &Path, disk: &Path) -> Result<serde_json::Value> {
    let out = run_capture(qemu_img, &["info", "-U", "--output=json", &disk.to_string_lossy()], Duration::from_secs(30))
        .context("cannot run qemu-img")?;
    serde_json::from_str(&out.stdout)
        .map_err(|_| err!("{} is not a usable disk image: {}", disk.display(), out.stderr.trim()))
}

/// The size the guest sees, in bytes.
pub(crate) fn disk_virtual_size(qemu_img: &Path, disk: &Path) -> Result<u64> {
    disk_info(qemu_img, disk)?["virtual-size"]
        .as_u64()
        .ok_or_else(|| err!("{} is not a usable disk image: qemu-img did not tell its size", disk.display()))
}

pub(crate) fn run_tool(program: &Path, args: &[&str]) -> Result<()> {
    let out =
        run_capture(program, args, Duration::from_secs(120)).context(format!("cannot run {}", program.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(err!("{} {} failed: {}", program.display(), args.first().unwrap_or(&""), out.stderr.trim()))
    }
}

/// Turns a VM name into a folder name: path separators and control characters become '-'.
fn folder_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c == '/' || c == '\0' || c.is_control() { '-' } else { c }).collect();
    match cleaned.trim().trim_start_matches('.') {
        "" => "vm".to_owned(),
        other => other.to_owned(),
    }
}

/// Writes a file so that a crash leaves either the old or the new content, never a mix.
pub fn write_atomically(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let write = || -> std::io::Result<()> {
        fs::write(&tmp, content)?;
        File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, path)?;
        sync_parent(path)
    };
    write().context(format!("cannot write {}", path.display()))
}

/// Makes a rename or removal in `path`'s directory durable.
pub fn sync_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) => File::open(parent)?.sync_all(),
        None => Ok(()),
    }
}

/// Removes a file, or a directory with everything in it. One that is not there is fine.
pub(crate) fn remove_if_present(path: &Path) -> std::io::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match removed {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn remove_files(paths: &[PathBuf]) -> Result<()> {
    for path in paths {
        remove_if_present(path).context(format!("cannot remove {}", path.display()))?;
    }
    Ok(())
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).context("cannot read /dev/urandom")?;
    Ok(bytes)
}

fn random_uuid() -> Result<String> {
    let mut b = random_bytes::<16>()?;
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let hex: Vec<String> = b.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    ))
}

/// A random address in the range QEMU/KVM guests conventionally use.
fn random_mac() -> Result<String> {
    let b = random_bytes::<3>()?;
    Ok(format!("52:54:00:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2]))
}

#[cfg(test)]
pub(crate) fn example_config() -> Config {
    Config {
        schema: SCHEMA,
        id: "0b7c2d5e-8f1a-4c3b-9d2e-6a5f4e3d2c1b".into(),
        name: "Arch test".into(),
        os: "archlinux".into(),
        machine: "pc-q35-11.1".into(),
        cpus: 4,
        memory_mb: 4096,
        disk: DISK_FILE.into(),
        install_image: None,
        mac: "52:54:00:12:34:56".into(),
        features: Features::default(),
        shared_folders: vec![],
        port_forwards: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_round_trip_through_the_file_format() {
        let mut config = example_config();
        config.install_image = Some("/isos/arch.iso".into());
        config.features.gpu_acceleration = true;
        config.shared_folders = vec![SharedFolder { name: "projects".into(), path: "/home/me/projects".into() }];
        config.port_forwards = vec![PortForward { host: 2222, guest: 22 }];
        let text = config.to_toml();
        assert_eq!(Config::parse(&text).unwrap(), config);
        assert!(text.starts_with("schema = 1\nid = \"0b7c2d5e-"), "{text}");
    }

    #[test]
    fn a_minimal_file_gets_default_features() {
        let config = Config::parse(
            "schema = 1\nid = \"x\"\nname = \"n\"\nos = \"linux\"\nmachine = \"pc-q35-11.1\"\ncpus = 2\nmemory_mb = 2048\ndisk = \"disk.qcow2\"\nmac = \"52:54:00:00:00:01\"\n",
        )
        .unwrap();
        assert_eq!(
            config.features,
            Features { gpu_acceleration: false, network: true, clipboard: true, microphone: false }
        );
        assert!(config.shared_folders.is_empty());
    }

    #[test]
    fn files_from_a_newer_roomy_are_refused_with_an_explanation() {
        let text = example_config().to_toml().replace("schema = 1", "schema = 2") + "new_thing = true\n";
        let error = Config::parse(&text).unwrap_err();
        assert!(error.message().contains("newer version of roomy"), "{error}");
    }

    #[test]
    fn unknown_settings_are_errors_not_silently_dropped() {
        let text = example_config().to_toml().replace("cpus = 4", "cpus = 4\ncpu_count = 8");
        assert!(Config::parse(&text).unwrap_err().message().contains("cpu_count"));
    }

    #[test]
    fn impossible_settings_are_rejected() {
        fn share(name: &str) -> SharedFolder {
            SharedFolder { name: name.into(), path: "/tmp".into() }
        }
        let broken = |change: fn(&mut Config)| {
            let mut config = example_config();
            change(&mut config);
            config.validate().unwrap_err().message().to_owned()
        };
        assert!(broken(|c| c.name = "  ".into()).contains("needs a name"));
        assert!(broken(|c| c.cpus = 0).contains("at least 1 processor"));
        assert!(broken(|c| c.memory_mb = 128).contains("256 MB"));
        assert!(broken(|c| c.disk = "../other.qcow2".into()).contains("inside the virtual machine's folder"));
        assert!(broken(|c| c.shared_folders = vec![share("my files")]).contains("not usable"));
        assert!(broken(|c| c.shared_folders = vec![share("docs"), share("docs")]).contains("both named 'docs'"));
    }

    #[test]
    fn folder_names_are_safe_path_components() {
        assert_eq!(folder_name("Arch Linux"), "Arch Linux");
        assert_eq!(folder_name("a/b"), "a-b");
        assert_eq!(folder_name("..hidden"), "hidden");
        assert_eq!(folder_name(" / "), "-");
        assert_eq!(folder_name(""), "vm");
    }

    #[test]
    fn identities_are_well_formed() {
        let uuid = random_uuid().unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
        assert_ne!(uuid, random_uuid().unwrap());
        let mac = random_mac().unwrap();
        assert!(mac.starts_with("52:54:00:") && mac.len() == 17, "{mac}");
    }
}
