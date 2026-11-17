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

`roomy doctor` checks all of this and says how to fix what is missing.
`roomy setup` installs the packages on Arch-based distributions.

## Installing (Arch)

    cd packaging/arch
    makepkg -si

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

### Settings

The name, network, clipboard sharing, microphone and disk size change immediately. Processors, memory, GPU
acceleration, shared folders and forwarded ports are hardware of the guest: changes are saved and take effect the
next time the virtual machine is shut down and started. roomy says which is which for every change.

### Inside the guest

Some features need a helper installed in the guest; the window's **Guest Setup** shows the exact commands.

- `qemu-guest-agent` keeps the guest's clock right after Pause and Suspend
- `spice-vdagent` provides copy and paste (needs a desktop in the guest)
- Shared folders are mounted with `mount -t virtiofs <name> /mnt/<name>`

## Building from source

    cargo build --release --workspace
    cargo test --workspace

`lab/` holds the scripts used to check roomy against real QEMU and real guests; see the comments at the top of each.
