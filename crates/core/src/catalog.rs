//! Guest systems roomy knows something about: what to call them, what to
//! install inside them so that every feature works, and which features a
//! system cannot offer whatever is installed.

use crate::{Result, err};

/// Each helper's package, what it is for, and the feature it serves when a system may lack that.
const HELPERS: [(&str, &str, Option<Feature>); 2] = [
    ("qemu-guest-agent", "keeps the guest's clock right after Pause and Suspend", None),
    (
        "spice-vdagent",
        "copy and paste between the guest and this computer (needs a desktop in the guest)",
        Some(Feature::Clipboard),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Linux,
    Bsd,
}

impl Family {
    pub const ALL: [Family; 2] = [Family::Linux, Family::Bsd];

    pub fn label(self) -> &'static str {
        match self {
            Family::Linux => "Linux",
            Family::Bsd => "BSD",
        }
    }
}

/// Something roomy offers that only works when the guest system plays its part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    GpuAcceleration,
    Clipboard,
    SharedFolders,
    /// The guest's screen taking the size of the window it is shown in.
    ScreenResizing,
}

impl Feature {
    pub const ALL: [Feature; 4] =
        [Feature::GpuAcceleration, Feature::Clipboard, Feature::SharedFolders, Feature::ScreenResizing];

    pub fn name(self) -> &'static str {
        match self {
            Feature::GpuAcceleration => "GPU acceleration",
            Feature::Clipboard => "clipboard sharing",
            Feature::SharedFolders => "shared folders",
            Feature::ScreenResizing => "screen resizing",
        }
    }

    pub fn title(self) -> String {
        let mut letters = self.name().chars();
        letters.next().map(|first| first.to_uppercase().chain(letters).collect()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestSystem {
    /// Stored in the VM's settings.
    pub id: &'static str,
    /// Shown to the user.
    pub label: &'static str,
    pub family: Family,
    /// A command to run inside the guest that installs and starts the helpers,
    /// or `None` when roomy does not know the system's package manager.
    pub install_helpers: Option<&'static str>,
    /// What this system cannot do as a guest, each with the reason as a sentence about it.
    pub lacks: &'static [(Feature, &'static str)],
}

impl GuestSystem {
    pub fn offers(&self, feature: Feature) -> bool {
        self.why_not(feature).is_none()
    }

    pub fn why_not(&self, feature: Feature) -> Option<&'static str> {
        self.lacks.iter().find(|(lacking, _)| *lacking == feature).map(|(_, why)| *why)
    }

    pub fn require(&self, feature: Feature) -> Result<()> {
        match self.why_not(feature) {
            Some(why) => Err(err!("a {} guest cannot use {}: {why}", self.label, feature.name())),
            None => Ok(()),
        }
    }

    pub fn limits(&self) -> Option<String> {
        let names: Vec<&str> = self.lacks.iter().map(|(feature, _)| feature.name()).collect();
        (!names.is_empty()).then(|| format!("Not available in a {} guest: {}.", self.label, names.join(", ")))
    }

    pub fn helpers(&self) -> impl Iterator<Item = (&'static str, &'static str)> {
        let system = *self;
        HELPERS
            .into_iter()
            .filter(move |(_, _, serves)| serves.is_none_or(|feature| system.offers(feature)))
            .map(|(package, purpose, _)| (package, purpose))
    }
}

// A macro rather than only a constant, so that `concat!` can take it.
macro_rules! start_agent {
    () => {
        "sudo systemctl enable --now qemu-guest-agent"
    };
}
const START_AGENT: &str = start_agent!();

macro_rules! linux {
    ($id:literal, $label:literal, $install:literal) => {
        GuestSystem {
            id: $id,
            label: $label,
            family: Family::Linux,
            install_helpers: Some(concat!($install, " && ", start_agent!())),
            lacks: &[],
        }
    };
}

/// Any Linux roomy does not know by name; what an unknown identifier is taken for.
pub const LINUX: GuestSystem =
    GuestSystem { id: "linux", label: "Linux (other)", family: Family::Linux, install_helpers: None, lacks: &[] };
pub const ARCH_LINUX: GuestSystem =
    linux!("archlinux", "Arch Linux", "sudo pacman -S --needed qemu-guest-agent spice-vdagent");
pub const DEBIAN: GuestSystem = linux!("debian", "Debian", "sudo apt install qemu-guest-agent spice-vdagent");
pub const FEDORA: GuestSystem = linux!("fedora", "Fedora", "sudo dnf install qemu-guest-agent spice-vdagent");
pub const OPENSUSE: GuestSystem = linux!("opensuse", "openSUSE", "sudo zypper install qemu-guest-agent spice-vdagent");
pub const UBUNTU: GuestSystem = linux!("ubuntu", "Ubuntu", "sudo apt install qemu-guest-agent spice-vdagent");

pub const FREEBSD: GuestSystem = GuestSystem {
    id: "freebsd",
    label: "FreeBSD",
    family: Family::Bsd,
    // `su`: FreeBSD comes without sudo.
    install_helpers: Some(
        "su -l root -c 'pkg install -y qemu-guest-agent && sysrc qemu_guest_agent_enable=YES && \
         sysrc -f /boot/loader.conf virtio_console_load=YES && kldload -n virtio_console && \
         service qemu-guest-agent start'",
    ),
    lacks: &[
        (Feature::GpuAcceleration, "FreeBSD has no driver for a virtual machine's accelerated graphics"),
        (Feature::Clipboard, "FreeBSD has no version of the helper that carries the clipboard (spice-vdagent)"),
        (Feature::SharedFolders, "FreeBSD has no driver for them yet (virtiofs)"),
        (
            Feature::ScreenResizing,
            "The screen keeps the size the guest started with, and its picture is scaled to fit the window",
        ),
    ],
};

pub const SYSTEMS: [GuestSystem; 7] = [LINUX, ARCH_LINUX, DEBIAN, FEDORA, OPENSUSE, UBUNTU, FREEBSD];

pub fn known(id: &str) -> Option<GuestSystem> {
    SYSTEMS.into_iter().find(|system| system.id == id)
}

/// For an identifier a user gave: an unknown one is an error naming those there are.
pub fn chosen(id: &str) -> Result<GuestSystem> {
    known(id).ok_or_else(|| err!("roomy does not know a guest system called '{id}'; it knows: {}", ids()))
}

pub fn ids() -> String {
    SYSTEMS.map(|system| system.id).join(", ")
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
        assert_eq!(known("archlinux"), Some(ARCH_LINUX));
        let command = ARCH_LINUX.install_helpers.unwrap();
        assert!(command.starts_with("sudo pacman -S --needed qemu-guest-agent spice-vdagent"), "{command}");
        assert!(command.ends_with(START_AGENT), "{command}");
        assert_eq!(known("plan9"), None);
        assert_eq!(
            chosen("plan9").unwrap_err().message(),
            "roomy does not know a guest system called 'plan9'; it knows: linux, archlinux, debian, fedora, \
             opensuse, ubuntu, freebsd"
        );
        assert_eq!(LINUX.install_helpers, None);
        assert!(generic_install_advice().contains("spice-vdagent"));
        for known in &SYSTEMS[1..] {
            let command = known.install_helpers.unwrap();
            assert!(known.helpers().all(|(package, _)| command.contains(package)), "{command}");
        }
    }

    #[test]
    fn a_linux_guest_offers_everything_and_a_freebsd_guest_says_what_it_cannot_do() {
        assert_eq!((FEDORA.family, FEDORA.limits()), (Family::Linux, None));
        assert_eq!(FEDORA.helpers().count(), 2);
        assert!(FEDORA.require(Feature::SharedFolders).is_ok());

        assert_eq!(FREEBSD.family, Family::Bsd);
        assert_eq!(
            FREEBSD.limits().unwrap(),
            "Not available in a FreeBSD guest: GPU acceleration, clipboard sharing, shared folders, screen resizing."
        );
        assert_eq!(
            FREEBSD.require(Feature::SharedFolders).unwrap_err().message(),
            "a FreeBSD guest cannot use shared folders: FreeBSD has no driver for them yet (virtiofs)"
        );
        assert_eq!(Feature::Clipboard.title(), "Clipboard sharing");
        assert_eq!(FREEBSD.helpers().map(|(package, _)| package).collect::<Vec<_>>(), ["qemu-guest-agent"]);
        assert!(!FREEBSD.install_helpers.unwrap().contains("spice-vdagent"));
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
