//! The systems roomy offers to download, and where each project announces its images.
//!
//! Only editions that are useful in a virtual machine are listed (no handheld or
//! graphics-driver builds). An edition roomy cannot check itself is listed with its project's
//! download page instead.

use super::Group::{FirstChoices, Popular, Servers, Special, Tinkerers};
use super::{Distro, Edition, Source};
use crate::catalog::{self as guest, ARCH_LINUX, DEBIAN, FEDORA, FREEBSD, GuestSystem, LINUX, OPENSUSE};

const EDITIONS: &str = "Editions";

const fn edition(
    name: &'static str,
    slug: &'static str,
    note: &'static str,
    guest: GuestSystem,
    source: Source,
) -> Edition {
    Edition { section: EDITIONS, name, slug, note, guest, source }
}

const fn in_section(
    section: &'static str,
    name: &'static str,
    slug: &'static str,
    note: &'static str,
    guest: GuestSystem,
    source: Source,
) -> Edition {
    Edition { section, name, slug, note, guest, source }
}

const fn website(page: &'static str) -> Source {
    Source::Website { page }
}

const fn sums(list: &'static str, pattern: &'static str) -> Source {
    Source::Sums { list, pattern, fixed: None }
}

const fn ubuntu(list: &'static str, pattern: &'static str) -> Source {
    Source::UbuntuLts { list, pattern }
}

const UBUNTU: &str = "https://releases.ubuntu.com/{series}/SHA256SUMS";
const FLAVOURS: &str = "Official flavours";

const fn flavour(
    name: &'static str,
    slug: &'static str,
    note: &'static str,
    list: &'static str,
    pattern: &'static str,
) -> Edition {
    in_section(FLAVOURS, name, slug, note, guest::UBUNTU, ubuntu(list, pattern))
}

const fn fedora(
    section: &'static str,
    name: &'static str,
    slug: &'static str,
    note: &'static str,
    guest: GuestSystem,
    variant: &'static str,
    subvariant: &'static str,
) -> Edition {
    in_section(section, name, slug, note, guest, Source::Fedora { variant, subvariant, link: "" })
}

const SPINS: &str = "Spins";
const ATOMIC: &str = "Atomic desktops";
const LABS: &str = "Labs";

const fn spin(name: &'static str, slug: &'static str, note: &'static str, subvariant: &'static str) -> Edition {
    fedora(SPINS, name, slug, note, FEDORA, "Spins", subvariant)
}

const fn lab(name: &'static str, slug: &'static str, note: &'static str, subvariant: &'static str) -> Edition {
    fedora(LABS, name, slug, note, FEDORA, "Labs", subvariant)
}

// Atomic systems are updated as a whole image, so the usual package managers cannot add the
// guest helpers: they are marked as a generic Linux guest.
const fn atomic(name: &'static str, slug: &'static str, note: &'static str, variant: &'static str) -> Edition {
    fedora(ATOMIC, name, slug, note, LINUX, variant, variant)
}

const DEBIAN_LIVE: &str = "https://cdimage.debian.org/debian-cd/current-live/amd64/iso-hybrid/SHA256SUMS";

const fn debian_live(name: &'static str, slug: &'static str, note: &'static str, pattern: &'static str) -> Edition {
    in_section("Live desktops", name, slug, note, DEBIAN, sums(DEBIAN_LIVE, pattern))
}

const MANJARO: &str = "https://manjaro.org/products/download/x86";

const fn manjaro(name: &'static str, slug: &'static str, pattern: &'static str) -> Edition {
    edition(name, slug, "", ARCH_LINUX, Source::PageLink { page: MANJARO, pattern })
}

const fn garuda(
    name: &'static str,
    slug: &'static str,
    note: &'static str,
    list: &'static str,
    pattern: &'static str,
    fixed: &'static str,
) -> Edition {
    edition(name, slug, note, ARCH_LINUX, Source::Sums { list, pattern, fixed: Some(fixed) })
}

const MINT: &str = "https://mirrors.edge.kernel.org/linuxmint/stable/";

const fn mint(name: &'static str, slug: &'static str, note: &'static str, pattern: &'static str) -> Edition {
    edition(name, slug, note, guest::UBUNTU, Source::NewestFolder { index: MINT, list: "sha256sum.txt", pattern })
}

const ROCKY: &str = "https://download.rockylinux.org/pub/rocky/10/isos/x86_64/CHECKSUM";
const ALMA: &str = "https://repo.almalinux.org/almalinux/10/isos/x86_64/CHECKSUM";
const KALI: &str = "https://cdimage.kali.org/current/SHA256SUMS";
const VOID: &str = "https://repo-default.voidlinux.org/live/current/sha256sum.txt";
const BY_NETWORK: &str = "Downloads the rest while installing";
const NO_DESKTOP: &str = "No desktop";

const fn freebsd(name: &'static str, slug: &'static str, note: &'static str, pattern: &'static str) -> Edition {
    edition(
        name,
        slug,
        note,
        FREEBSD,
        Source::NewestFolder {
            index: "https://download.freebsd.org/releases/amd64/amd64/ISO-IMAGES/",
            list: "CHECKSUM.SHA256-FreeBSD-{version}-RELEASE-amd64",
            pattern,
        },
    )
}

pub static DISTROS: &[Distro] = &[
    Distro {
        id: "ubuntu",
        name: "Ubuntu",
        group: FirstChoices,
        tagline: "The most widely used desktop Linux",
        about: "Made by Canonical. Long-term support releases get five years of updates, and most guides online assume Ubuntu.",
        desktop: "GNOME",
        site: "https://ubuntu.com",
        editions: &[
            edition("Desktop", "desktop", "", guest::UBUNTU, ubuntu(UBUNTU, "ubuntu-*-desktop-amd64.iso")),
            edition("Server", "server", NO_DESKTOP, guest::UBUNTU, ubuntu(UBUNTU, "ubuntu-*-live-server-amd64.iso")),
            flavour(
                "Kubuntu",
                "kubuntu",
                "KDE Plasma",
                "https://cdimage.ubuntu.com/kubuntu/releases/{series}/release/SHA256SUMS",
                "kubuntu-*-desktop-amd64.iso",
            ),
            flavour(
                "Xubuntu",
                "xubuntu",
                "Xfce",
                "https://cdimage.ubuntu.com/xubuntu/releases/{series}/release/SHA256SUMS",
                "xubuntu-*-desktop-amd64.iso",
            ),
            flavour(
                "Xubuntu Minimal",
                "xubuntu-minimal",
                "Xfce with few apps",
                "https://cdimage.ubuntu.com/xubuntu/releases/{series}/release/SHA256SUMS",
                "xubuntu-*-minimal-amd64.iso",
            ),
            flavour(
                "Lubuntu",
                "lubuntu",
                "LXQt, light",
                "https://cdimage.ubuntu.com/lubuntu/releases/{series}/release/SHA256SUMS",
                "lubuntu-*-desktop-amd64.iso",
            ),
            flavour(
                "Ubuntu Budgie",
                "budgie",
                "Budgie",
                "https://cdimage.ubuntu.com/ubuntu-budgie/releases/{series}/release/SHA256SUMS",
                "ubuntu-budgie-*-desktop-amd64.iso",
            ),
            flavour(
                "Ubuntu Cinnamon",
                "cinnamon",
                "Cinnamon",
                "https://cdimage.ubuntu.com/ubuntucinnamon/releases/{series}/release/SHA256SUMS",
                "ubuntucinnamon-*-desktop-amd64.iso",
            ),
            flavour(
                "Ubuntu Unity",
                "unity",
                "Unity",
                "https://cdimage.ubuntu.com/ubuntu-unity/releases/{series}/release/SHA256SUMS",
                "ubuntu-unity-*-desktop-amd64.iso",
            ),
            flavour(
                "Ubuntu Studio",
                "studio",
                "KDE Plasma, for audio, video and graphics work",
                "https://cdimage.ubuntu.com/ubuntustudio/releases/{series}/release/SHA256SUMS",
                "ubuntustudio-*-desktop-amd64.iso",
            ),
            flavour(
                "Edubuntu",
                "edubuntu",
                "For schools",
                "https://cdimage.ubuntu.com/edubuntu/releases/{series}/release/SHA256SUMS",
                "edubuntu-*-desktop-amd64.iso",
            ),
        ],
    },
    Distro {
        id: "fedora",
        name: "Fedora",
        group: FirstChoices,
        tagline: "Recent software, close to how upstream made it",
        about: "Sponsored by Red Hat. A new release every six months, with recent software and few changes to how each desktop looks.",
        desktop: "GNOME, KDE Plasma and many more",
        site: "https://fedoraproject.org",
        editions: &[
            fedora(EDITIONS, "Workstation", "workstation", "GNOME", FEDORA, "Workstation", "Workstation"),
            fedora(EDITIONS, "KDE Plasma Desktop", "kde", "KDE Plasma", FEDORA, "KDE", "KDE"),
            in_section(
                EDITIONS,
                "Server",
                "server",
                NO_DESKTOP,
                FEDORA,
                Source::Fedora { variant: "Server", subvariant: "Server", link: "-dvd-" },
            ),
            fedora(
                EDITIONS,
                "Everything",
                "everything",
                "A small network installer: choose everything while installing",
                FEDORA,
                "Everything",
                "Everything",
            ),
            atomic("Silverblue", "silverblue", "GNOME", "Silverblue"),
            atomic("Kinoite", "kinoite", "KDE Plasma", "Kinoite"),
            atomic("COSMIC Atomic", "cosmic-atomic", "COSMIC", "COSMIC-Atomic"),
            atomic("Budgie Atomic", "budgie-atomic", "Budgie", "Budgie-Atomic"),
            atomic("Sway Atomic", "sway-atomic", "Sway, tiling", "Sway-Atomic"),
            spin("Xfce", "xfce", "Light and traditional", "Xfce"),
            spin("Cinnamon", "cinnamon", "Familiar for Windows users", "Cinnamon"),
            spin("COSMIC", "cosmic", "System76’s new desktop", "COSMIC"),
            spin("MATE-Compiz", "mate", "In the style of GNOME 2", "Mate"),
            spin("Budgie", "budgie", "Simple and modern", "Budgie"),
            spin("LXQt", "lxqt", "Very light", "LXQt"),
            spin("LXDE", "lxde", "Very light, older", "LXDE"),
            spin("Sway", "sway", "Tiling, driven from the keyboard", "Sway"),
            spin("i3", "i3", "Tiling, driven from the keyboard", "i3"),
            spin("Miracle", "miracle", "Tiling, on Mir", "MiracleWM"),
            spin("KDE Plasma Mobile", "plasma-mobile", "Plasma for phones and tablets", "KDE_Mobile"),
            spin("Sugar on a Stick", "soas", "A learning desktop for children", "SoaS"),
            lab("Games", "games", "", "Games"),
            lab("Design Suite", "design", "Graphics and publishing", "Design_suite"),
            lab("Jam", "jam", "Audio and music", "Jam_KDE"),
            lab("Astronomy", "astronomy", "", "Astronomy_KDE"),
            lab("Scientific", "scientific", "", "Scientific_KDE"),
            lab("Security Lab", "security", "Security testing tools", "Security"),
            lab("Python Classroom", "python", "For teaching Python", "Python_Classroom"),
        ],
    },
    Distro {
        id: "linuxmint",
        name: "Linux Mint",
        group: FirstChoices,
        tagline: "Familiar for people coming from Windows",
        about: "Based on Ubuntu, with a taskbar, a start menu and careful updates. LMDE is the same desktop on Debian instead.",
        desktop: "Cinnamon, MATE or Xfce",
        site: "https://linuxmint.com",
        editions: &[
            mint("Cinnamon", "cinnamon", "The main one", "linuxmint-*-cinnamon-64bit.iso"),
            mint("MATE", "mate", "", "linuxmint-*-mate-64bit.iso"),
            mint("Xfce", "xfce", "The lightest", "linuxmint-*-xfce-64bit.iso"),
            edition(
                "LMDE",
                "lmde",
                "Cinnamon on Debian",
                DEBIAN,
                sums("https://mirrors.edge.kernel.org/linuxmint/debian/sha256sum.txt", "lmde-*-cinnamon-64bit.iso"),
            ),
        ],
    },
    Distro {
        id: "zorin",
        name: "Zorin OS",
        group: FirstChoices,
        tagline: "Polished, with layouts like Windows or macOS",
        about: "Based on Ubuntu. Offers a desktop layout that looks like the system you came from.",
        desktop: "GNOME, reshaped",
        site: "https://zorin.com/os/",
        editions: &[edition("Core", "core", "", guest::UBUNTU, website("https://zorin.com/os/download/"))],
    },
    Distro {
        id: "elementary",
        name: "elementary OS",
        group: FirstChoices,
        tagline: "Calm and consistent, in the style of macOS",
        about: "Its own Pantheon desktop and apps, designed together, with a store of curated apps.",
        desktop: "Pantheon",
        site: "https://elementary.io",
        editions: &[edition("Standard", "standard", "", guest::UBUNTU, website("https://elementary.io"))],
    },
    Distro {
        id: "popos",
        name: "Pop!_OS",
        group: FirstChoices,
        tagline: "The COSMIC desktop from System76",
        about: "Based on Ubuntu, with System76’s new desktop COSMIC and window tiling built in.",
        desktop: "COSMIC",
        site: "https://system76.com/pop/",
        editions: &[edition(
            "Standard",
            "standard",
            "",
            guest::UBUNTU,
            Source::Pop { release: "24.04", channel: "intel" },
        )],
    },
    Distro {
        id: "debian",
        name: "Debian",
        group: Popular,
        tagline: "Stable and dependable; Ubuntu and Mint build on it",
        about: "Run by volunteers. A new release every two years or so, which changes little once it is out.",
        desktop: "Your choice",
        site: "https://www.debian.org",
        editions: &[
            debian_live("GNOME", "gnome", "", "debian-live-*-amd64-gnome.iso"),
            debian_live("KDE Plasma", "kde", "", "debian-live-*-amd64-kde.iso"),
            debian_live("Xfce", "xfce", "", "debian-live-*-amd64-xfce.iso"),
            debian_live("Cinnamon", "cinnamon", "", "debian-live-*-amd64-cinnamon.iso"),
            debian_live("MATE", "mate", "", "debian-live-*-amd64-mate.iso"),
            debian_live("LXQt", "lxqt", "", "debian-live-*-amd64-lxqt.iso"),
            debian_live("LXDE", "lxde", "", "debian-live-*-amd64-lxde.iso"),
            debian_live("Standard", "standard", NO_DESKTOP, "debian-live-*-amd64-standard.iso"),
            in_section(
                "Installers",
                "Network installer",
                "netinst",
                BY_NETWORK,
                DEBIAN,
                sums(
                    "https://cdimage.debian.org/debian-cd/current/amd64/iso-cd/SHA256SUMS",
                    "debian-*-amd64-netinst.iso",
                ),
            ),
        ],
    },
    Distro {
        id: "opensuse",
        name: "openSUSE",
        group: Popular,
        tagline: "Rolling Tumbleweed or steady Leap",
        about: "Tumbleweed updates continuously, but each snapshot passes automated tests first; Leap is a fixed release. Both can roll back to an earlier snapshot.",
        desktop: "KDE Plasma or GNOME",
        site: "https://www.opensuse.org",
        editions: &[
            in_section(
                "Tumbleweed",
                "Tumbleweed",
                "tumbleweed",
                "",
                OPENSUSE,
                Source::Sums {
                    list: "https://download.opensuse.org/tumbleweed/iso/openSUSE-Tumbleweed-DVD-x86_64-Current.iso.sha256",
                    pattern: "openSUSE-Tumbleweed-DVD-x86_64-*-Media.iso",
                    fixed: Some(
                        "https://download.opensuse.org/tumbleweed/iso/openSUSE-Tumbleweed-DVD-x86_64-Current.iso",
                    ),
                },
            ),
            in_section(
                "Tumbleweed",
                "Tumbleweed network installer",
                "tumbleweed-net",
                BY_NETWORK,
                OPENSUSE,
                Source::Sums {
                    list: "https://download.opensuse.org/tumbleweed/iso/openSUSE-Tumbleweed-NET-x86_64-Current.iso.sha256",
                    pattern: "openSUSE-Tumbleweed-NET-x86_64-*-Media.iso",
                    fixed: Some(
                        "https://download.opensuse.org/tumbleweed/iso/openSUSE-Tumbleweed-NET-x86_64-Current.iso",
                    ),
                },
            ),
            in_section(
                "Leap",
                "Leap",
                "leap",
                "A new release every year or so",
                OPENSUSE,
                website("https://get.opensuse.org/leap/"),
            ),
        ],
    },
    Distro {
        id: "cachyos",
        name: "CachyOS",
        group: Popular,
        tagline: "Arch, tuned for speed",
        about: "Based on Arch, with kernels and packages built for newer processors, and a graphical installer that offers many desktops.",
        desktop: "KDE Plasma by default; many offered",
        site: "https://cachyos.org",
        editions: &[edition(
            "Desktop",
            "desktop",
            "Choose the desktop while installing",
            ARCH_LINUX,
            Source::PageLink {
                page: "https://cachyos.org/download/",
                pattern: "https://iso.cachyos.org/desktop/*/cachyos-desktop-linux-*.iso",
            },
        )],
    },
    Distro {
        id: "bazzite",
        name: "Bazzite",
        group: Popular,
        tagline: "Ready for games, built on Fedora Atomic",
        about: "Steam, drivers and gaming tools set up from the start; the system updates as a whole and can roll back. Games in a virtual machine run slower than on the computer itself.",
        desktop: "KDE Plasma or GNOME",
        site: "https://bazzite.gg",
        editions: &[edition(
            "Bazzite",
            "standard",
            "Pick the edition on the website",
            LINUX,
            website("https://bazzite.gg/#image-picker"),
        )],
    },
    Distro {
        id: "kdeneon",
        name: "KDE neon",
        group: Popular,
        tagline: "The newest Plasma, straight from KDE",
        about: "An Ubuntu base with KDE’s own builds of Plasma and its apps, as soon as they are released.",
        desktop: "KDE Plasma",
        site: "https://neon.kde.org",
        editions: &[edition(
            "User Edition",
            "user",
            "",
            guest::UBUNTU,
            sums(
                "https://files.kde.org/neon/images/desk/user/current/neon-user-desk-current.sha256sum",
                "neon-user-desk-*-*.iso",
            ),
        )],
    },
    Distro {
        id: "manjaro",
        name: "Manjaro",
        group: Popular,
        tagline: "Arch, with an installer and slower updates",
        about: "Based on Arch, with graphical tools and updates held back a little for testing.",
        desktop: "KDE Plasma, GNOME or Xfce",
        site: "https://manjaro.org",
        editions: &[
            manjaro("KDE Plasma", "kde", "https://download.manjaro.org/kde/*/manjaro-kde-*-*-*.iso"),
            manjaro("GNOME", "gnome", "https://download.manjaro.org/gnome/*/manjaro-gnome-*-*-*.iso"),
            manjaro("Xfce", "xfce", "https://download.manjaro.org/xfce/*/manjaro-xfce-*-*-*.iso"),
        ],
    },
    Distro {
        id: "garudalinux",
        name: "Garuda Linux",
        group: Popular,
        tagline: "Arch, colourful and ready for games",
        about: "Based on Arch, with a heavily themed desktop, Btrfs snapshots to roll back, and gaming tools set up.",
        desktop: "KDE Plasma and others",
        site: "https://garudalinux.org",
        editions: &[
            garuda(
                "Dr460nized",
                "dr460nized",
                "KDE Plasma, heavily themed",
                "https://iso.builds.garudalinux.org/iso/latest/garuda/dr460nized/latest.iso.sha256",
                "garuda-dr460nized-linux-*-*.iso",
                "https://iso.builds.garudalinux.org/iso/latest/garuda/dr460nized/latest.iso",
            ),
            garuda(
                "Mokka",
                "mokka",
                "KDE Plasma, calmer",
                "https://iso.builds.garudalinux.org/iso/latest/garuda/mokka/latest.iso.sha256",
                "garuda-mokka-linux-*-*.iso",
                "https://iso.builds.garudalinux.org/iso/latest/garuda/mokka/latest.iso",
            ),
            garuda(
                "GNOME",
                "gnome",
                "",
                "https://iso.builds.garudalinux.org/iso/latest/garuda/gnome/latest.iso.sha256",
                "garuda-gnome-linux-*-*.iso",
                "https://iso.builds.garudalinux.org/iso/latest/garuda/gnome/latest.iso",
            ),
            garuda(
                "Cinnamon",
                "cinnamon",
                "",
                "https://iso.builds.garudalinux.org/iso/latest/garuda/cinnamon/latest.iso.sha256",
                "garuda-cinnamon-linux-*-*.iso",
                "https://iso.builds.garudalinux.org/iso/latest/garuda/cinnamon/latest.iso",
            ),
        ],
    },
    Distro {
        id: "mxlinux",
        name: "MX Linux",
        group: Popular,
        tagline: "Light and quick, based on Debian",
        about: "Debian stable with a set of handy tools and desktops that run well on little memory.",
        desktop: "Xfce, KDE Plasma or Fluxbox",
        site: "https://mxlinux.org",
        editions: &[edition(
            "MX Linux",
            "standard",
            "Pick the desktop on the website",
            DEBIAN,
            website("https://mxlinux.org/download-links/"),
        )],
    },
    Distro {
        id: "vanillaos",
        name: "Vanilla OS",
        group: Popular,
        tagline: "An unchangeable Debian with apps from any system",
        about: "The system updates as a whole and can roll back; apps from other distributions run in containers next to it.",
        desktop: "GNOME",
        site: "https://vanillaos.org",
        editions: &[edition(
            "Standard",
            "standard",
            "",
            LINUX,
            Source::GitHub { repo: "Vanilla-OS/live-iso", pattern: "Vanilla-OS-*-stable-amd64.*.iso" },
        )],
    },
    Distro {
        id: "solus",
        name: "Solus",
        group: Popular,
        tagline: "Made from scratch for the desktop",
        about: "Not based on another system. A curated set of desktop software, updated weekly.",
        desktop: "Budgie, GNOME, KDE Plasma or Xfce",
        site: "https://getsol.us",
        editions: &[edition(
            "Solus",
            "standard",
            "Pick the desktop on the website",
            LINUX,
            website("https://getsol.us/download/"),
        )],
    },
    Distro {
        id: "archlinux",
        name: "Arch Linux",
        group: Tinkerers,
        tagline: "Simple and rolling; you put it together",
        about: "Starts at a text console, and you install and set up everything yourself. The Arch Wiki is famously good.",
        desktop: "None; you choose",
        site: "https://archlinux.org",
        editions: &[edition(
            "Standard",
            "standard",
            "",
            ARCH_LINUX,
            sums("https://geo.mirror.pkgbuild.com/iso/latest/sha256sums.txt", "archlinux-*-x86_64.iso"),
        )],
    },
    Distro {
        id: "endeavouros",
        name: "EndeavourOS",
        group: Tinkerers,
        tagline: "Arch with a friendly installer",
        about: "Arch Linux itself, installed with a graphical installer and a few helpful tools.",
        desktop: "You choose while installing",
        site: "https://endeavouros.com",
        editions: &[edition("Standard", "standard", "", ARCH_LINUX, website("https://endeavouros.com/#Download"))],
    },
    Distro {
        id: "nixos",
        name: "NixOS",
        group: Tinkerers,
        tagline: "The whole system described in one file",
        about: "Everything is declared in a configuration file, so a setup can be rebuilt exactly or rolled back.",
        desktop: "GNOME or KDE Plasma",
        site: "https://nixos.org",
        editions: &[
            edition("Graphical", "graphical", "", LINUX, Source::NixOs { kind: "graphical" }),
            edition("Minimal", "minimal", NO_DESKTOP, LINUX, Source::NixOs { kind: "minimal" }),
        ],
    },
    Distro {
        id: "gentoo",
        name: "Gentoo",
        group: Tinkerers,
        tagline: "Built from source for your own computer",
        about: "Software is compiled on your computer with the options you choose. Installing takes a while.",
        desktop: "None; you choose",
        site: "https://www.gentoo.org",
        editions: &[
            edition("Minimal", "minimal", NO_DESKTOP, LINUX, Source::Gentoo { prefix: "install-amd64-minimal-" }),
            edition(
                "LiveGUI",
                "livegui",
                "KDE Plasma, to try it first",
                LINUX,
                Source::Gentoo { prefix: "livegui-amd64-" },
            ),
        ],
    },
    Distro {
        id: "voidlinux",
        name: "Void Linux",
        group: Tinkerers,
        tagline: "Independent, small and quick",
        about: "Its own package manager, and the runit init system instead of systemd.",
        desktop: "Xfce or none",
        site: "https://voidlinux.org",
        editions: &[
            edition("Base", "base", NO_DESKTOP, LINUX, sums(VOID, "void-live-x86_64-*-base.iso")),
            edition("Xfce", "xfce", "", LINUX, sums(VOID, "void-live-x86_64-*-xfce.iso")),
        ],
    },
    Distro {
        id: "alpinelinux",
        name: "Alpine Linux",
        group: Tinkerers,
        tagline: "Tiny and security-minded",
        about: "Popular for servers and containers. Uses musl and BusyBox, and installs in a few hundred megabytes.",
        desktop: "None",
        site: "https://alpinelinux.org",
        editions: &[
            edition("Virtual", "virtual", "Made for virtual machines", LINUX, Source::Alpine { flavor: "alpine-virt" }),
            edition("Standard", "standard", "", LINUX, Source::Alpine { flavor: "alpine-standard" }),
            edition(
                "Extended",
                "extended",
                "More packages, no network needed",
                LINUX,
                Source::Alpine { flavor: "alpine-extended" },
            ),
        ],
    },
    Distro {
        id: "rockylinux",
        name: "Rocky Linux",
        group: Servers,
        tagline: "A free rebuild of Red Hat Enterprise Linux",
        about: "Matches RHEL, for trying what will run on servers at work. Ten years of updates per release.",
        desktop: "GNOME or none",
        site: "https://rockylinux.org",
        editions: &[
            edition("Boot", "boot", BY_NETWORK, FEDORA, sums(ROCKY, "Rocky-*-x86_64-boot.iso")),
            edition("Minimal", "minimal", "", FEDORA, sums(ROCKY, "Rocky-*-x86_64-minimal.iso")),
            edition("DVD", "dvd", "Everything, no network needed", FEDORA, sums(ROCKY, "Rocky-*-x86_64-dvd1.iso")),
        ],
    },
    Distro {
        id: "almalinux",
        name: "AlmaLinux",
        group: Servers,
        tagline: "Another free system that works like RHEL",
        about: "Run by a foundation; works like RHEL and is supported for ten years per release.",
        desktop: "GNOME or none",
        site: "https://almalinux.org",
        editions: &[
            edition("Boot", "boot", BY_NETWORK, FEDORA, sums(ALMA, "AlmaLinux-*-x86_64-boot.iso")),
            edition("Minimal", "minimal", "", FEDORA, sums(ALMA, "AlmaLinux-*-x86_64-minimal.iso")),
            edition("DVD", "dvd", "Everything, no network needed", FEDORA, sums(ALMA, "AlmaLinux-*-x86_64-dvd.iso")),
        ],
    },
    Distro {
        id: "kalilinux",
        name: "Kali Linux",
        group: Special,
        tagline: "Tools for security testing",
        about: "Based on Debian, with hundreds of tools for penetration testing already installed.",
        desktop: "Xfce",
        site: "https://www.kali.org",
        editions: &[edition("Installer", "installer", "", DEBIAN, sums(KALI, "kali-linux-*-installer-amd64.iso"))],
    },
    Distro {
        id: "freebsd",
        name: "FreeBSD",
        group: Tinkerers,
        tagline: "The best-known BSD, made as one complete system",
        about: "Not a Linux: its kernel, tools and manuals are developed together as one system. It starts at a text console, and a desktop is yours to add from its packages.",
        desktop: "None; you choose",
        site: "https://www.freebsd.org",
        editions: &[
            freebsd("Installer", "disc1", "", "FreeBSD-*-RELEASE-amd64-disc1.iso"),
            freebsd("Installer with packages", "dvd1", "No network needed", "FreeBSD-*-RELEASE-amd64-dvd1.iso"),
            freebsd("Network installer", "bootonly", BY_NETWORK, "FreeBSD-*-RELEASE-amd64-bootonly.iso"),
        ],
    },
];
