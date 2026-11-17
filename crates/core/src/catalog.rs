//! Guest systems roomy knows something about: what to call them and what to
//! install inside them so that every feature works.

/// What each helper inside the guest is for, in the order the setup guide lists them.
pub const HELPERS: [(&str, &str); 2] = [
    ("qemu-guest-agent", "keeps the guest's clock right after Pause and Suspend"),
    ("spice-vdagent", "copy and paste between the guest and this computer (needs a desktop in the guest)"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestSystem {
    /// Stored in the VM's settings.
    pub id: &'static str,
    /// Shown to the user.
    pub label: &'static str,
    /// A command to run inside the guest that installs and starts the helpers,
    /// or `None` when roomy does not know the system's package manager.
    pub install_helpers: Option<&'static str>,
}

const START_AGENT: &str = "sudo systemctl enable --now qemu-guest-agent";

macro_rules! system {
    ($id:literal, $label:literal, $install:literal) => {
        GuestSystem {
            id: $id,
            label: $label,
            install_helpers: Some(concat!($install, " && sudo systemctl enable --now qemu-guest-agent")),
        }
    };
}

/// Every system offered when creating a VM; the first is the fallback.
pub const SYSTEMS: [GuestSystem; 6] = [
    GuestSystem { id: "linux", label: "Linux (other)", install_helpers: None },
    system!("archlinux", "Arch Linux", "sudo pacman -S --needed qemu-guest-agent spice-vdagent"),
    system!("debian", "Debian", "sudo apt install qemu-guest-agent spice-vdagent"),
    system!("fedora", "Fedora", "sudo dnf install qemu-guest-agent spice-vdagent"),
    system!("opensuse", "openSUSE", "sudo zypper install qemu-guest-agent spice-vdagent"),
    system!("ubuntu", "Ubuntu", "sudo apt install qemu-guest-agent spice-vdagent"),
];

/// The system with this identifier; unknown identifiers get the generic entry.
pub fn system(id: &str) -> GuestSystem {
    SYSTEMS.iter().copied().find(|system| system.id == id).unwrap_or(SYSTEMS[0])
}

/// What to do inside a guest whose package manager roomy does not know.
pub fn generic_install_advice() -> String {
    format!(
        "Install the packages qemu-guest-agent and spice-vdagent with the guest's package manager, then run: {START_AGENT}"
    )
}

/// The command that makes a shared folder appear inside the guest until it restarts.
pub fn mount_command(share: &str) -> String {
    format!("sudo mkdir -p /mnt/{share} && sudo mount -t virtiofs {share} /mnt/{share}")
}

/// The line for the guest's /etc/fstab that mounts a shared folder at every boot.
/// `nofail` keeps the guest booting when the folder is later removed from the VM.
pub fn fstab_line(share: &str) -> String {
    format!("{share} /mnt/{share} virtiofs defaults,nofail 0 0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_systems_install_and_start_the_helpers_and_unknown_ones_fall_back() {
        let arch = system("archlinux");
        assert_eq!(arch.label, "Arch Linux");
        let command = arch.install_helpers.unwrap();
        assert!(command.starts_with("sudo pacman -S --needed qemu-guest-agent spice-vdagent"), "{command}");
        assert!(command.ends_with(START_AGENT), "{command}");
        assert_eq!(system("plan9"), SYSTEMS[0]);
        assert_eq!(SYSTEMS[0].install_helpers, None);
        assert!(generic_install_advice().contains("spice-vdagent"));
        for known in &SYSTEMS[1..] {
            let command = known.install_helpers.unwrap();
            assert!(HELPERS.iter().all(|(package, _)| command.contains(package)), "{command}");
        }
    }

    #[test]
    fn shared_folders_mount_under_mnt_by_their_name() {
        assert_eq!(mount_command("docs"), "sudo mkdir -p /mnt/docs && sudo mount -t virtiofs docs /mnt/docs");
        assert_eq!(fstab_line("docs"), "docs /mnt/docs virtiofs defaults,nofail 0 0");
    }
}
