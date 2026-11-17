# roomy

Simple, reliable virtual machines for the Linux desktop, on KVM and QEMU.

Each virtual machine is a folder of its own (settings, disk, saved state) that you can move, copy or back up.
The folder also holds `thumbnail.png`, a small picture of the screen as the window last showed it, which the
list of virtual machines displays.
roomy talks to QEMU directly; there is no background service and no libvirt.

## Requirements

- An x86-64 Linux computer with virtualization enabled, and access to `/dev/kvm`
- QEMU 10.1 or newer with its D-Bus display, virtiofsd, UEFI firmware (OVMF)
- GTK 4.14 or newer and libadwaita 1.5 or newer for the window; libpulse for sound

`roomy doctor` checks all of this and says how to fix what is missing. The window runs the same check when it
opens; when packages are missing it offers to install them, shows the commands first, and asks for the
administrator's password through the desktop's own dialog.
`roomy setup` installs the packages on Arch, Fedora, openSUSE, Debian and Ubuntu, and on distributions built on
them. Only the Arch installation has been run for real so far; the others were worked out from each distribution's
package definitions. On Arch-based systems the installation is a full system update as well (`pacman -Syu`),
the only way packages install reliably on a rolling distribution. Releases whose QEMU is too old are told so
instead: Ubuntu needs 26.04 or newer, Debian 13 needs its backports source enabled, and openSUSE Leap 16 lacks a
new enough virtiofsd for shared folders.

## One file for any computer

    just appimage

builds `work/dist/roomy-x86_64.AppImage`: the `roomy` command and its window in a single file. Copy it to
another computer, make it executable, and run it; every command works the same way:

    chmod +x roomy-x86_64.AppImage
    ./roomy-x86_64.AppImage doctor     check that computer
    ./roomy-x86_64.AppImage setup      install what is missing
    ./roomy-x86_64.AppImage            open the window

The file does not contain QEMU or the window toolkit; those come from the distribution, which is what `doctor`
and `setup` are for. It needs glibc 2.39 or newer (2024 and later distributions) when built in the lab box.
Virtual machines keep running after the window is closed, and after the file is replaced by a newer one.

The first time its window opens, the file adds itself to the application menu, which is also what gives the
window its icon: a launcher entry and an icon under `~/.local/share` (`applications/com.mudlej.Roomy.desktop` and
`icons/hicolor/scalable/apps/com.mudlej.Roomy.svg`). The entry follows the file when it is moved and started
again, and menus leave it out once the file is gone; delete those two files to remove it for good.

The file mounts itself, which needs the `fusermount3` helper that desktop systems normally have. Where it is
missing, run with `APPIMAGE_EXTRACT_AND_RUN=1` set: the file then unpacks itself on each start instead.

## Installing (Arch)

    just package-arch
    sudo pacman -U work/dist/roomy-*.pkg.tar.zst

This installs `roomy`, the window program `roomy-gui`, a launcher entry and an icon.

## Using it

    roomy                 open the window
    roomy doctor          check this computer; prints a report you can copy
    roomy setup           install what is missing, then check again
    roomy --help          every command, including `roomy vm ...` for use without the window

### Pause and Suspend

- **Pause** freezes the virtual machine in memory. Resuming is instant, but a paused machine is lost if this
  computer powers off.
- **Suspend** saves it to disk and stops it. It survives a restart of this computer, and Start continues exactly
  where it left off.
- Suspend is not available while **GPU acceleration** is on, because an accelerated graphics card cannot be saved
  to disk. Use Pause, or turn GPU acceleration off (applies at the next boot).

### Creating a virtual machine

**New Virtual Machine** asks for a name, the guest system and an installation image, and offers processors,
memory and disk as sliders over what this computer has (the disk: over what is free where the virtual machine
will be kept). Network, clipboard sharing, microphone, GPU acceleration and shared folders can be set right
there. Under **Advanced**: forwarded ports, and starting from a copy of an existing disk instead of an empty
one. That disk has to be a complete qcow2 image; roomy says how to convert one that is not.

### Settings

The name, network, clipboard sharing, microphone and disk size change immediately. Processors, memory, GPU
acceleration, shared folders and forwarded ports are hardware of the guest: changes are saved and take effect the
next time the virtual machine is shut down and started. roomy says which is which for every change.

### Inside the guest

Some features need a helper installed in the guest; the window's **Guest Setup** shows the exact commands.

- `qemu-guest-agent` keeps the guest's clock right after Pause and Suspend
- `spice-vdagent` provides copy and paste (needs a desktop in the guest)
- Shared folders need no helper: a Linux guest with systemd 254 or newer (2023) finds each one at `/mnt/<name>`
  when it starts. In an older guest, mount it with `mount -t virtiofs <name> /mnt/<name>`; the copy button next
  to each shared folder gives the full command.

## Building from source

Everything is done through `just`; the first-time setup installs it when it is missing.

    distrobox assemble create --file distrobox.ini    # an Arch box with QEMU, GTK 4 and Rust
    distrobox enter roomy-lab
    ./first_time_setup.sh
    just                    # lists every recipe

    just run                # build and open the window
    just check              # formatting, lints and all tests
    just appimage           # the single-file release, in work/dist
    just lab-gui quick      # drive the real window against real QEMU and a real guest

Outside the box the same commands work wherever Rust 1.92+, GTK 4.14+ and libadwaita 1.5+ development files are
installed. `just check-headless` is the gate for a machine without the window toolkit.

The `lab-*` recipes check roomy against real QEMU and real guests and write what they saw to
`work/lab/results/`; the comments at the top of each script in `lab/` say what it covers.

## Licence

roomy is free software under the GNU General Public License, version 3 or (at your option) any later version.
See [LICENSE](LICENSE).
