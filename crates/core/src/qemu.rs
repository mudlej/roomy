//! How roomy configures QEMU.
//!
//! The device set here is the one validated in `lab/` on QEMU 11.1: it boots,
//! suspends to disk and restores with display, audio, clipboard and shared
//! folders attached.

use crate::error::Context;
use crate::host::Firmware;
use crate::process::run_capture;
use crate::vm::{Config, Folder};
use crate::{Result, catalog, err};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Device and backend identifiers that other parts of roomy refer to over QMP.
pub mod ids {
    /// The network card (for switching its link on and off).
    pub const NIC: &str = "nic0";
    /// The virtual disk's block node (for growing it).
    pub const DISK: &str = "disk0";
    /// The guest agent's character device.
    pub const AGENT_CHARDEV: &str = "qga0";
}

/// Free PCIe ports for devices added while the VM runs.
const HOTPLUG_PORTS: u32 = 4;

/// Sockets a running VM uses, all inside its runtime directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sockets {
    dir: PathBuf,
}

impl Sockets {
    pub fn in_dir(dir: &Path) -> Sockets {
        Sockets { dir: dir.to_owned() }
    }

    /// QEMU's control socket (QMP).
    pub fn qmp(&self) -> PathBuf {
        self.dir.join("qmp.sock")
    }

    /// The guest agent's channel.
    pub fn agent(&self) -> PathBuf {
        self.dir.join("agent.sock")
    }

    /// The daemon socket of the shared folder at `index` in the settings.
    pub fn share(&self, index: usize) -> PathBuf {
        self.dir.join(format!("share-{index}.sock"))
    }
}

pub struct Launch<'a> {
    pub config: &'a Config,
    pub folder: &'a Folder,
    pub sockets: &'a Sockets,
    pub firmware: &'a Firmware,
    /// Start empty and wait for a saved state to be loaded.
    pub restoring: bool,
    /// The graphics card accelerated graphics are drawn on (see `host::display_gpu`).
    /// Without one, QEMU picks a card itself.
    pub render_node: Option<&'a Path>,
}

/// The QEMU command line for a VM. The VM always starts paused (`-S`); the
/// caller applies runtime switches such as the network link and then lets it run.
pub fn args(launch: &Launch) -> Vec<String> {
    let Launch { config, folder, sockets, firmware, restoring, render_node } = launch;
    let mut a = ArgList::default();

    a.flag("-nodefaults");
    a.flag("-no-user-config");
    a.flag("-S");
    a.opt("-name", format!("guest={}", esc_str(&config.name)));
    a.opt("-uuid", &config.id);
    // Guest memory is a shared memfd: required by the shared-folder daemon.
    // vmport=off: with it on, a Linux guest finds a second, VMware-style pointer device and QEMU sends
    // the pointer there instead of to the USB tablet. One pointer device, the same for every guest.
    a.opt("-machine", format!("{},memory-backend=mem,vmport=off", config.machine));
    a.opt("-accel", "kvm");
    a.opt("-cpu", "host");
    a.opt("-smp", config.cpus.to_string());
    a.opt("-m", config.memory_mb.to_string());
    a.opt("-object", format!("memory-backend-memfd,id=mem,size={}M,share=on", config.memory_mb));
    a.opt("-rtc", "base=utc");
    // Guest-initiated sleep would leave a VM that looks hung; roomy's own
    // Pause and Suspend replace it.
    a.opt("-global", "ICH9-LPC.disable_s3=1");
    a.opt("-global", "ICH9-LPC.disable_s4=1");
    a.opt("-qmp", format!("unix:{},server=on,wait=off", esc(&sockets.qmp())));

    // Firmware: shared read-only code plus this VM's own variable store.
    a.opt(
        "-drive",
        format!("if=pflash,unit=0,readonly=on,format={},file={}", firmware.code_format, esc(&firmware.code)),
    );
    // The variable store was copied from whatever firmware was installed when the VM was made;
    // its own format counts, not that of the firmware installed now.
    let vars_format = folder.firmware_vars_format().unwrap_or(firmware.vars_format.as_str());
    a.opt("-drive", format!("if=pflash,unit=1,format={vars_format},file={}", esc(&folder.firmware_vars())));

    for port in 1..=HOTPLUG_PORTS {
        a.opt("-device", format!("pcie-root-port,id=hotplug{port},chassis={port},port={}", 0x10 + port));
    }

    // Display, input, sound. The display is served over D-Bus to roomy's window.
    let gl = config.features.gpu_acceleration;
    let acceleration = match (gl, render_node) {
        (false, _) => String::new(),
        (true, None) => ",gl=on".to_owned(),
        (true, Some(node)) => format!(",gl=on,rendernode={}", node.display()),
    };
    a.opt("-display", format!("dbus,p2p=yes,audiodev=snd0{acceleration}"));
    a.opt("-device", if gl { "virtio-vga-gl" } else { "virtio-vga" });
    a.opt("-audiodev", "dbus,id=snd0");
    a.opt("-device", "ich9-intel-hda");
    a.opt("-device", "hda-duplex,audiodev=snd0");
    a.opt("-device", "qemu-xhci,id=usb");
    a.opt("-device", "usb-tablet");

    // Storage.
    a.opt(
        "-blockdev",
        format!(
            "driver=qcow2,node-name={},discard=unmap,file.driver=file,file.filename={}",
            ids::DISK,
            esc(&folder.disk_path(config))
        ),
    );
    a.opt("-device", format!("virtio-blk-pci,drive={},bootindex=1", ids::DISK));
    if let Some(image) = &config.install_image {
        a.opt(
            "-blockdev",
            format!("driver=raw,node-name=install0,read-only=on,file.driver=file,file.filename={}", esc(image)),
        );
        a.opt("-device", "ide-cd,drive=install0,bus=ide.0,bootindex=2");
    }

    // Network: private NAT that needs no host setup. The card is always
    // present; "network off" unplugs its virtual cable.
    let forwards: String =
        config.port_forwards.iter().map(|f| format!(",hostfwd=tcp:127.0.0.1:{}-:{}", f.host, f.guest)).collect();
    a.opt("-netdev", format!("user,id=net0{forwards}"));
    a.opt("-device", format!("virtio-net-pci,netdev=net0,id={},mac={}", ids::NIC, config.mac));

    a.opt("-device", "virtio-rng-pci");

    // Guest integration channels: the guest agent and the clipboard agent.
    a.opt("-device", "virtio-serial-pci,id=channels");
    a.opt("-chardev", format!("socket,id={},path={},server=on,wait=off", ids::AGENT_CHARDEV, esc(&sockets.agent())));
    a.opt("-device", format!("virtserialport,chardev={},name=org.qemu.guest_agent.0", ids::AGENT_CHARDEV));
    // Clipboard only. This channel can also carry the pointer, and QEMU then hands it every pointer
    // event, which is lost unless the guest runs the matching helper. The pointer always goes
    // through the USB tablet instead, which needs nothing in the guest.
    a.opt("-chardev", "qemu-vdagent,id=vdagent,clipboard=on,mouse=off");
    a.opt("-device", "virtserialport,chardev=vdagent,name=com.redhat.spice.0");

    for (index, share) in config.shared_folders.iter().enumerate() {
        a.opt("-chardev", format!("socket,id=share{index},path={}", esc(&sockets.share(index))));
        a.opt("-device", format!("vhost-user-fs-pci,chardev=share{index},tag={}", share.name));
    }
    if !config.shared_folders.is_empty() {
        // Has the guest mount the folders by itself when it boots. The list travels in the
        // machine's firmware tables, where systemd (254 and newer) looks for mounts to add to
        // the guest's own; a guest that does not know of this simply finds nothing mounted.
        let mounts = catalog::fstab(config.shared_folders.iter().map(|share| share.name.as_str()));
        a.opt(
            "-smbios",
            format!("type=11,value=io.systemd.credential.binary:fstab.extra={}", base64(mounts.as_bytes())),
        );
    }

    if *restoring {
        a.opt("-incoming", "defer");
    }
    a.0
}

#[derive(Default)]
struct ArgList(Vec<String>);

impl ArgList {
    fn flag(&mut self, name: &str) {
        self.0.push(name.to_owned());
    }

    fn opt(&mut self, name: &str, value: impl Into<String>) {
        self.0.push(name.to_owned());
        self.0.push(value.into());
    }
}

/// Escapes a path for use inside a QEMU `key=value,...` option.
fn esc(path: &Path) -> String {
    esc_str(&path.to_string_lossy())
}

fn esc_str(value: &str) -> String {
    value.replace(',', ",,")
}

/// The versioned machine type that this QEMU's `q35` alias stands for, e.g. `pc-q35-11.1`.
pub fn default_machine(qemu: &Path) -> Result<String> {
    let out = run_capture(qemu, &["-machine", "help"], Duration::from_secs(10)).context("cannot run QEMU")?;
    parse_default_machine(&out.stdout).ok_or_else(|| err!("QEMU did not report a q35 machine type"))
}

fn parse_default_machine(machine_help: &str) -> Option<String> {
    // Either "q35  Standard PC (...) (alias of pc-q35-11.1)" or, on some
    // builds, the versioned line carries "(alias of q35)"-style info reversed.
    let line = machine_help.lines().find(|l| l.split_whitespace().next() == Some("q35"))?;
    let (_, rest) = line.split_once("(alias of ")?;
    Some(rest.split(')').next()?.trim().to_owned())
}

/// Standard base64, with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk.iter().fold(0u32, |group, byte| group << 8 | u32::from(*byte)) << (8 * (3 - chunk.len()));
        for position in 0..4 {
            if position <= chunk.len() {
                text.push(ALPHABET[(group >> (18 - 6 * position) & 63) as usize] as char);
            } else {
                text.push('=');
            }
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{PortForward, SharedFolder, example_config};

    fn launch_args(config: &Config, restoring: bool) -> Vec<String> {
        launch_args_on(config, restoring, None)
    }

    thread_local! {
        /// What the next VM folder's firmware variables file holds, if it is to have one.
        static VARS_FILE: std::cell::RefCell<Option<Vec<u8>>> = const { std::cell::RefCell::new(None) };
    }

    #[test]
    fn the_firmware_variables_keep_the_format_they_were_made_in() {
        let config = example_config();
        let vars_format = |content: Option<&[u8]>| {
            VARS_FILE.with(|vars| vars.replace(content.map(<[u8]>::to_vec)));
            let args = launch_args(&config, false);
            VARS_FILE.with(|vars| vars.replace(None));
            let drive = args.iter().find(|arg| arg.starts_with("if=pflash,unit=1")).unwrap().clone();
            drive.split(',').find_map(|part| part.strip_prefix("format=").map(str::to_owned)).unwrap()
        };
        // The firmware installed here says "raw" (see `launch_args_on`); a VM brought from a
        // distribution that ships qcow2 firmware is still opened as qcow2, and the other way round.
        assert_eq!(vars_format(Some(b"QFI\xfb\0\0\0\x03 and so on")), "qcow2");
        assert_eq!(vars_format(Some(&[0u8; 64])), "raw");
        // No file yet (it is copied in before the first start): what the installed firmware says.
        assert_eq!(vars_format(None), "raw");
    }

    fn launch_args_on(config: &Config, restoring: bool, render_node: Option<&Path>) -> Vec<String> {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("roomy-qemu-args-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(crate::vm::CONFIG_FILE), config.to_toml()).unwrap();
        let folder = Folder::open(&dir).unwrap();
        if let Some(vars) = VARS_FILE.with(|vars| vars.borrow().clone()) {
            std::fs::write(folder.firmware_vars(), vars).unwrap();
        }
        let firmware = Firmware {
            code: "/fw/CODE.fd".into(),
            code_format: "raw".into(),
            vars_template: "/fw/VARS.fd".into(),
            vars_format: "raw".into(),
        };
        let sockets = Sockets::in_dir(Path::new("/run/vm"));
        let args =
            args(&Launch { config, folder: &folder, sockets: &sockets, firmware: &firmware, restoring, render_node });
        let dir_text = folder.dir().to_string_lossy().into_owned();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(args.iter().all(|a| !a.is_empty()), "an empty argument would be read by QEMU as a disk image");
        args.into_iter().map(|a| a.replace(&dir_text, "<vm>")).collect()
    }

    fn has(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|pair| pair[0] == flag && pair[1] == value)
    }

    #[test]
    fn a_plain_vm_gets_the_validated_device_set() {
        let args = launch_args(&example_config(), false);
        let expected = "\
-nodefaults -no-user-config -S -name guest=Arch test -uuid 0b7c2d5e-8f1a-4c3b-9d2e-6a5f4e3d2c1b \
-machine pc-q35-11.1,memory-backend=mem,vmport=off -accel kvm -cpu host -smp 4 -m 4096 \
-object memory-backend-memfd,id=mem,size=4096M,share=on -rtc base=utc \
-global ICH9-LPC.disable_s3=1 -global ICH9-LPC.disable_s4=1 \
-qmp unix:/run/vm/qmp.sock,server=on,wait=off \
-drive if=pflash,unit=0,readonly=on,format=raw,file=/fw/CODE.fd \
-drive if=pflash,unit=1,format=raw,file=<vm>/firmware-vars.fd \
-device pcie-root-port,id=hotplug1,chassis=1,port=17 -device pcie-root-port,id=hotplug2,chassis=2,port=18 \
-device pcie-root-port,id=hotplug3,chassis=3,port=19 -device pcie-root-port,id=hotplug4,chassis=4,port=20 \
-display dbus,p2p=yes,audiodev=snd0 -device virtio-vga -audiodev dbus,id=snd0 \
-device ich9-intel-hda -device hda-duplex,audiodev=snd0 -device qemu-xhci,id=usb -device usb-tablet \
-blockdev driver=qcow2,node-name=disk0,discard=unmap,file.driver=file,file.filename=<vm>/disk.qcow2 \
-device virtio-blk-pci,drive=disk0,bootindex=1 \
-netdev user,id=net0 -device virtio-net-pci,netdev=net0,id=nic0,mac=52:54:00:12:34:56 \
-device virtio-rng-pci -device virtio-serial-pci,id=channels \
-chardev socket,id=qga0,path=/run/vm/agent.sock,server=on,wait=off \
-device virtserialport,chardev=qga0,name=org.qemu.guest_agent.0 \
-chardev qemu-vdagent,id=vdagent,clipboard=on,mouse=off -device virtserialport,chardev=vdagent,name=com.redhat.spice.0";
        assert_eq!(args.join(" "), expected);
    }

    #[test]
    fn base64_matches_the_standard() {
        let encoded = ["", "Zg==", "Zm8=", "Zm9v", "Zm9vYg==", "Zm9vYmE=", "Zm9vYmFy"];
        for (length, expected) in encoded.iter().enumerate() {
            assert_eq!(base64(&b"foobar"[..length]), *expected);
        }
        assert_eq!(base64(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    #[test]
    fn gpu_acceleration_switches_display_and_graphics_card_together() {
        let mut config = example_config();
        config.features.gpu_acceleration = true;
        let args = launch_args(&config, false);
        assert!(has(&args, "-display", "dbus,p2p=yes,audiodev=snd0,gl=on"));
        // On a computer with several graphics cards the chosen one is named, never left to QEMU.
        let chosen = launch_args_on(&config, false, Some(Path::new("/dev/dri/renderD129")));
        assert!(has(&chosen, "-display", "dbus,p2p=yes,audiodev=snd0,gl=on,rendernode=/dev/dri/renderD129"));
        assert!(has(&args, "-device", "virtio-vga-gl"));
        assert!(!has(&args, "-device", "virtio-vga"));
    }

    #[test]
    fn optional_parts_appear_only_when_configured() {
        let mut config = example_config();
        config.install_image = Some("/isos/arch,2026.iso".into());
        config.shared_folders = vec![
            SharedFolder { name: "projects".into(), path: "/home/me/projects".into() },
            SharedFolder { name: "music".into(), path: "/home/me/Music".into() },
        ];
        config.port_forwards = vec![PortForward { host: 2222, guest: 22 }, PortForward { host: 8080, guest: 80 }];
        let args = launch_args(&config, true);
        // Commas in paths are doubled so QEMU does not read them as option separators.
        assert!(has(
            &args,
            "-blockdev",
            "driver=raw,node-name=install0,read-only=on,file.driver=file,file.filename=/isos/arch,,2026.iso"
        ));
        assert!(has(&args, "-device", "ide-cd,drive=install0,bus=ide.0,bootindex=2"));
        assert!(has(&args, "-netdev", "user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22,hostfwd=tcp:127.0.0.1:8080-:80"));
        assert!(has(&args, "-chardev", "socket,id=share0,path=/run/vm/share-0.sock"));
        assert!(has(&args, "-device", "vhost-user-fs-pci,chardev=share0,tag=projects"));
        assert!(has(&args, "-device", "vhost-user-fs-pci,chardev=share1,tag=music"));
        // The guest is told where to mount them: the two lines of catalog::fstab, encoded.
        let mounts = "projects /mnt/projects virtiofs defaults,nofail,x-gvfs-show 0 0\n\
                      music /mnt/music virtiofs defaults,nofail,x-gvfs-show 0 0\n";
        assert!(has(
            &args,
            "-smbios",
            &format!("type=11,value=io.systemd.credential.binary:fstab.extra={}", base64(mounts.as_bytes()))
        ));
        assert!(!launch_args(&example_config(), false).contains(&"-smbios".to_owned()));
        assert_eq!(&args[args.len() - 2..], ["-incoming", "defer"]);
        assert!(!launch_args(&example_config(), false).contains(&"-incoming".to_owned()));
    }

    #[test]
    fn reads_the_versioned_machine_type_from_qemus_help() {
        let help = "Supported machines are:\n\
            microvm              microvm (i386)\n\
            pc                   Standard PC (i440FX + PIIX, 1996) (alias of pc-i440fx-11.1)\n\
            pc-i440fx-11.1       Standard PC (i440FX + PIIX, 1996) (default)\n\
            q35                  Standard PC (Q35 + ICH9, 2009) (alias of pc-q35-11.1)\n\
            pc-q35-11.1          Standard PC (Q35 + ICH9, 2009)\n";
        assert_eq!(parse_default_machine(help).as_deref(), Some("pc-q35-11.1"));
        assert_eq!(parse_default_machine("none  empty machine\n"), None);
    }
}
