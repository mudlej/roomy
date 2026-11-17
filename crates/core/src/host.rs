//! Facts about the host: operating system, CPU, KVM, installed tools, firmware.

use crate::env::{Env, is_executable};
use crate::process::run_capture;
use serde_json::Value;
use std::fmt;
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub micro: u32,
}

impl Version {
    /// Extracts the first `N.N` or `N.N.N` found in `text`, e.g. from
    /// `QEMU emulator version 10.0.3 (Debian 1:10.0.3+ds-0+deb13u1)`.
    pub fn find_in(text: &str) -> Option<Version> {
        for token in text.split(|c: char| !(c.is_ascii_digit() || c == '.')) {
            let mut parts = token.split('.').filter(|p| !p.is_empty()).map(str::parse::<u32>);
            if let (Some(Ok(major)), Some(Ok(minor))) = (parts.next(), parts.next()) {
                let micro = parts.next().and_then(Result::ok).unwrap_or(0);
                return Some(Version { major, minor, micro });
            }
        }
        None
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsRelease {
    pub id: String,
    pub id_like: Vec<String>,
    pub pretty_name: String,
}

impl OsRelease {
    pub fn parse(text: &str) -> OsRelease {
        let mut os = OsRelease::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            match key.trim() {
                "ID" => os.id = value.to_ascii_lowercase(),
                "ID_LIKE" => os.id_like = value.split_whitespace().map(str::to_ascii_lowercase).collect(),
                "PRETTY_NAME" => os.pretty_name = value.to_owned(),
                _ => {}
            }
        }
        os
    }

    /// True if this distribution is, or is derived from, `family` (e.g. "arch").
    pub fn is_like(&self, family: &str) -> bool {
        self.id == family || self.id_like.iter().any(|like| like == family)
    }
}

pub fn os_release(env: &Env) -> OsRelease {
    let text = env.read("/etc/os-release").or_else(|_| env.read("/usr/lib/os-release")).unwrap_or_default();
    let mut os = OsRelease::parse(&text);
    if os.pretty_name.is_empty() {
        os.pretty_name = if os.id.is_empty() { "unknown Linux".to_owned() } else { os.id.clone() };
    }
    os
}

pub fn kernel_release(env: &Env) -> String {
    env.read("/proc/sys/kernel/osrelease").map(|s| s.trim().to_owned()).unwrap_or_else(|_| "unknown".to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuVirtualization {
    AmdV,
    IntelVtx,
}

impl CpuVirtualization {
    pub fn name(self) -> &'static str {
        match self {
            CpuVirtualization::AmdV => "AMD-V",
            CpuVirtualization::IntelVtx => "Intel VT-x",
        }
    }

    pub fn kvm_module(self) -> &'static str {
        match self {
            CpuVirtualization::AmdV => "kvm_amd",
            CpuVirtualization::IntelVtx => "kvm_intel",
        }
    }
}

/// Whether the CPU advertises hardware virtualization to the kernel.
pub fn cpu_virtualization(env: &Env) -> Option<CpuVirtualization> {
    let cpuinfo = env.read("/proc/cpuinfo").ok()?;
    let flags = cpuinfo.lines().find(|l| l.starts_with("flags"))?;
    flags.split_whitespace().find_map(|flag| match flag {
        "svm" => Some(CpuVirtualization::AmdV),
        "vmx" => Some(CpuVirtualization::IntelVtx),
        _ => None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KvmAccess {
    Ready,
    /// `/dev/kvm` does not exist.
    Missing,
    /// `/dev/kvm` exists but this user may not open it.
    Denied,
    /// `/dev/kvm` could be opened but does not behave like KVM.
    Broken(String),
}

pub fn kvm_access(env: &Env) -> KvmAccess {
    const KVM_GET_API_VERSION: libc::c_ulong = 0xAE00;
    const KVM_API_VERSION: libc::c_int = 12;
    let path = env.path("/dev/kvm");
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return KvmAccess::Missing,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return KvmAccess::Denied,
        Err(e) => return KvmAccess::Broken(e.to_string()),
    };
    // SAFETY: KVM_GET_API_VERSION takes no argument and only returns an integer.
    let version = unsafe { libc::ioctl(file.as_raw_fd(), KVM_GET_API_VERSION as _, 0) };
    match version {
        KVM_API_VERSION => KvmAccess::Ready,
        -1 => KvmAccess::Broken(std::io::Error::last_os_error().to_string()),
        other => KvmAccess::Broken(format!("unexpected KVM API version {other}")),
    }
}

pub const QEMU_PROGRAM: &str = "qemu-system-x86_64";

/// Asks a tool for its version (`<tool> --version`).
pub fn tool_version(program: &Path) -> Result<Version, String> {
    let out = run_capture(program, &["--version"], Duration::from_secs(10)).map_err(|e| e.to_string())?;
    Version::find_in(&out.stdout)
        .or_else(|| Version::find_in(&out.stderr))
        .ok_or_else(|| format!("could not find a version number in: {}", out.stdout.trim()))
}

/// Locates the shared-folder daemon. Distributions install it outside `PATH`,
/// so QEMU's vhost-user descriptors are consulted first.
pub fn find_virtiofsd(env: &Env) -> Option<PathBuf> {
    for descriptor in env.list("/usr/share/qemu/vhost-user") {
        let Some(json) = read_json(&descriptor) else { continue };
        if json["type"] == "fs" {
            if let Some(binary) = json["binary"].as_str() {
                let binary = env.path(binary);
                if is_executable(&binary) {
                    return Some(binary);
                }
            }
        }
    }
    env.find_program("virtiofsd").or_else(|| {
        ["/usr/lib/virtiofsd", "/usr/libexec/virtiofsd", "/usr/lib/qemu/virtiofsd"]
            .iter()
            .map(|p| env.path(p))
            .find(|p| is_executable(p))
    })
}

/// A UEFI firmware image pair as described by a QEMU firmware descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firmware {
    /// Read-only firmware code.
    pub code: PathBuf,
    pub code_format: String,
    /// Template for the per-VM variable store; each VM gets its own copy.
    pub vars_template: PathBuf,
    pub vars_format: String,
}

/// Picks UEFI firmware for x86-64 q35 machines without Secure Boot.
///
/// Uses the firmware descriptors that distributions ship for exactly this
/// purpose (`/usr/share/qemu/firmware`, overridable in `/etc/qemu/firmware`),
/// so no distribution-specific paths are hard-coded.
pub fn find_firmware(env: &Env) -> Option<Firmware> {
    let mut descriptors = env.list("/etc/qemu/firmware");
    let overridden: Vec<_> = descriptors.iter().filter_map(|p| p.file_name().map(|n| n.to_owned())).collect();
    descriptors.extend(
        env.list("/usr/share/qemu/firmware")
            .into_iter()
            .filter(|p| p.file_name().is_none_or(|n| !overridden.iter().any(|o| o == n))),
    );
    descriptors.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    descriptors.iter().filter_map(|d| read_json(d)).find_map(|json| firmware_from_descriptor(env, &json))
}

fn firmware_from_descriptor(env: &Env, json: &Value) -> Option<Firmware> {
    let has = |list: &Value, wanted: &str| list.as_array().is_some_and(|a| a.iter().any(|v| v == wanted));
    if !has(&json["interface-types"], "uefi") || has(&json["features"], "secure-boot") {
        return None;
    }
    let fits_q35 = json["targets"].as_array()?.iter().any(|target| {
        target["architecture"] == "x86_64"
            && target["machines"].as_array().is_some_and(|machines| {
                machines.iter().filter_map(Value::as_str).any(|glob| glob_matches(glob, "pc-q35-10.1"))
            })
    });
    let mapping = &json["mapping"];
    if !fits_q35 || mapping["device"] != "flash" {
        return None;
    }
    let firmware = Firmware {
        code: env.path(mapping["executable"]["filename"].as_str()?),
        code_format: mapping["executable"]["format"].as_str()?.to_owned(),
        vars_template: env.path(mapping["nvram-template"]["filename"].as_str()?),
        vars_format: mapping["nvram-template"]["format"].as_str()?.to_owned(),
    };
    (firmware.code.is_file() && firmware.vars_template.is_file()).then_some(firmware)
}

/// Matches the only glob form firmware descriptors use: a literal with `*` wildcards.
fn glob_matches(glob: &str, text: &str) -> bool {
    let mut rest = text;
    let mut parts = glob.split('*').peekable();
    let mut first = true;
    while let Some(part) = parts.next() {
        let last = parts.peek().is_none();
        if first && last {
            return rest == part;
        } else if first {
            let Some(stripped) = rest.strip_prefix(part) else { return false };
            rest = stripped;
        } else if last {
            return rest.ends_with(part);
        } else {
            let Some(at) = rest.find(part) else { return false };
            rest = &rest[at + part.len()..];
        }
        first = false;
    }
    true
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_versions_in_tool_output() {
        let v = |major, minor, micro| Some(Version { major, minor, micro });
        assert_eq!(Version::find_in("QEMU emulator version 11.1.3\nCopyright (c) 2003-2026"), v(11, 1, 3));
        assert_eq!(Version::find_in("QEMU emulator version 10.0.3 (Debian 1:10.0.3+ds-0+deb13u1)"), v(10, 0, 3));
        assert_eq!(Version::find_in("virtiofsd 1.14.0"), v(1, 14, 0));
        assert_eq!(Version::find_in("tool 9.2"), v(9, 2, 0));
        assert_eq!(Version::find_in("no version here"), None);
        assert!(Version::find_in("QEMU emulator version 10.0.3").unwrap() < crate::MIN_QEMU);
        assert!(Version::find_in("QEMU emulator version 10.1.0").unwrap() >= crate::MIN_QEMU);
    }

    #[test]
    fn recognises_distribution_families() {
        let cachy = OsRelease::parse("NAME=\"CachyOS Linux\"\nPRETTY_NAME=\"CachyOS\"\nID=cachyos\nID_LIKE=arch\n");
        assert_eq!(cachy.pretty_name, "CachyOS");
        assert!(cachy.is_like("arch"));
        assert!(!cachy.is_like("debian"));
        let ubuntu = OsRelease::parse("ID=ubuntu\nID_LIKE=\"debian\"\n");
        assert!(ubuntu.is_like("debian") && ubuntu.is_like("ubuntu") && !ubuntu.is_like("arch"));
        assert!(OsRelease::parse("ID=arch\n").is_like("arch"));
    }

    #[test]
    fn globs_as_used_by_firmware_descriptors() {
        assert!(glob_matches("pc-q35-*", "pc-q35-10.1"));
        assert!(!glob_matches("pc-i440fx-*", "pc-q35-10.1"));
        assert!(glob_matches("pc-q35-10.1", "pc-q35-10.1"));
        assert!(glob_matches("*", "anything"));
        assert!(glob_matches("pc-*-10.1", "pc-q35-10.1"));
        assert!(!glob_matches("pc-q35-9.*", "pc-q35-10.1"));
    }
}

/// A graphics card of this computer, as far as choosing one for a VM goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    /// Kernel name, e.g. `card1`.
    pub card: String,
    /// The device programs render with, e.g. `/dev/dri/renderD128`.
    pub render_node: PathBuf,
    /// Kernel driver, e.g. `amdgpu`.
    pub driver: String,
    /// The card the firmware showed its start-up screen on.
    pub boot_display: bool,
    /// Whether a monitor is connected to this card.
    pub has_monitor: bool,
}

/// The graphics cards that can render, in kernel order.
pub fn gpus(env: &Env) -> Vec<Gpu> {
    const DRM: &str = "/sys/class/drm";
    let entries = env.list(DRM);
    let name = |path: &Path| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut found = Vec::new();
    for entry in &entries {
        let card = name(entry);
        let is_card =
            card.strip_prefix("card").is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
        if !is_card {
            continue;
        }
        // The card's render device is listed next to it under its hardware device.
        let Some(render) =
            env.list(&format!("{DRM}/{card}/device/drm")).iter().map(|p| name(p)).find(|n| n.starts_with("renderD"))
        else {
            continue;
        };
        let driver = std::fs::read_link(entry.join("device/driver")).map(|target| name(&target)).unwrap_or_default();
        let boot_display = env.read(&format!("{DRM}/{card}/device/boot_vga")).is_ok_and(|v| v.trim() == "1");
        // Connectors are named after their card: card1-DP-1, card1-HDMI-A-1, ...
        let has_monitor = entries.iter().any(|connector| {
            name(connector).starts_with(&format!("{card}-"))
                && std::fs::read_to_string(connector.join("status")).is_ok_and(|status| status.trim() == "connected")
        });
        found.push(Gpu { card, render_node: Path::new("/dev/dri").join(render), driver, boot_display, has_monitor });
    }
    found
}

/// The graphics card a VM's accelerated graphics should be drawn on: the one
/// the desktop is shown with. A picture drawn on one card generally cannot be
/// shown by another without being copied through main memory, and handing
/// such a picture to the wrong card shows garbage. QEMU's own default is
/// whichever card the kernel happens to list first.
pub fn display_gpu(env: &Env) -> Option<Gpu> {
    let all = gpus(env);
    let rank = |gpu: &Gpu| (gpu.has_monitor, gpu.boot_display);
    // max_by_key keeps the last of equals; reversing makes the first listed card win ties.
    all.into_iter().rev().max_by_key(rank)
}
