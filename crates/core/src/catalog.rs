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

// A macro rather than only a constant, so that `concat!` can take it.
macro_rules! start_agent {
    () => {
        "sudo systemctl enable --now qemu-guest-agent"
    };
}
const START_AGENT: &str = start_agent!();

macro_rules! system {
    ($id:literal, $label:literal, $install:literal) => {
        GuestSystem { id: $id, label: $label, install_helpers: Some(concat!($install, " && ", start_agent!())) }
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

/// Where a shared folder is found inside the guest.
pub fn guest_path(share: &str) -> String {
    format!("/mnt/{share}")
}

/// The command that makes a shared folder appear inside the guest until it restarts, for a
/// guest that does not mount it by itself.
pub fn mount_command(share: &str) -> String {
    let path = guest_path(share);
    format!("sudo mkdir -p {path} && sudo mount -t virtiofs {share} {path}")
}

/// The line of a guest's table of mounts (fstab) that mounts a shared folder at every boot.
/// `nofail` keeps the guest booting when the folder cannot be mounted, and `x-gvfs-show` has
/// the guest's file manager list it among its places.
pub fn fstab_line(share: &str) -> String {
    format!("{share} {} virtiofs defaults,nofail,x-gvfs-show 0 0", guest_path(share))
}

/// The mounts of all shared folders named, as lines of a table of mounts.
pub fn fstab<'a>(shares: impl IntoIterator<Item = &'a str>) -> String {
    shares.into_iter().map(|share| fstab_line(share) + "\n").collect()
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
        assert_eq!(fstab_line("docs"), "docs /mnt/docs virtiofs defaults,nofail,x-gvfs-show 0 0");
        assert_eq!(
            fstab(["docs", "music"]),
            "docs /mnt/docs virtiofs defaults,nofail,x-gvfs-show 0 0\nmusic /mnt/music virtiofs defaults,nofail,x-gvfs-show 0 0\n"
        );
    }
}
