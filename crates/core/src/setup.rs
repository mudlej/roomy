//! Installing what roomy needs from the distribution's packages.

use crate::host::OsRelease;

/// What must be present on any distribution, in plain words. Shown when
/// roomy cannot install packages itself.
pub const REQUIRED_COMPONENTS: &[&str] = &[
    "QEMU 10.1 or newer for x86-64 (qemu-system-x86_64) with KVM support",
    "QEMU modules: D-Bus display, D-Bus audio, virtio-gpu and virtio-vga including their OpenGL variants",
    "qemu-img (disk image tool)",
    "virtiofsd (shared folders)",
    "UEFI firmware for virtual machines (OVMF / edk2) with its QEMU firmware descriptor",
    "GTK 4.14 or newer and libadwaita 1.5 or newer (for roomy's window)",
    "The PulseAudio client library, libpulse (for sound; PipeWire systems provide it too)",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// The package manager and its arguments, to be run as root.
    pub command: Vec<&'static str>,
    /// The argument that stops the package manager from asking its own questions.
    pub unattended: &'static str,
}

impl InstallPlan {
    pub fn command_line(&self) -> String {
        self.command.join(" ")
    }

    /// The command for when the user has already said yes: nothing may stop to ask.
    pub fn unattended_command(&self) -> Vec<&'static str> {
        let mut command = self.command.clone();
        command.push(self.unattended);
        command
    }
}

/// The package installation for this distribution, if roomy knows it.
///
/// Only Arch-based distributions are covered so far: that package set is the
/// one the engine was validated against. Other distributions get the
/// component list instead of a guess.
pub fn install_plan(os: &OsRelease) -> Option<InstallPlan> {
    os.is_like("arch").then(|| InstallPlan {
        unattended: "--noconfirm",
        command: vec![
            "pacman",
            "-S",
            "--needed",
            "qemu-desktop",
            "qemu-img",
            "virtiofsd",
            "edk2-ovmf",
            "gtk4",
            "libadwaita",
            "libpulse",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_derivatives_get_a_pacman_plan_and_others_get_none() {
        let cachy = OsRelease::parse("ID=cachyos\nID_LIKE=arch\n");
        let plan = install_plan(&cachy).unwrap();
        assert_eq!(
            plan.command_line(),
            "pacman -S --needed qemu-desktop qemu-img virtiofsd edk2-ovmf gtk4 libadwaita libpulse"
        );
        assert_eq!(plan.unattended_command().last(), Some(&"--noconfirm"));
        assert_eq!(install_plan(&OsRelease::parse("ID=fedora\n")), None);
        assert_eq!(install_plan(&OsRelease::parse("ID=ubuntu\nID_LIKE=debian\n")), None);
    }
}
