//! `roomy doctor` against fake hosts (see `common`). `/dev/kvm` cannot be
//! faked, so the KVM access check fails everywhere; the tests assert on the
//! other checks.

mod common;

use common::FakeHost;
use roomy_core::doctor::{self, Check, Report, Status};
use std::fs;

fn check<'a>(report: &'a Report, id: &str) -> &'a Check {
    report.checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no check '{id}' in:\n{}", report.to_text()))
}

fn statuses(report: &Report) -> Vec<(&'static str, Status)> {
    report.checks.iter().map(|c| (c.id, c.status)).collect()
}

#[test]
fn a_bare_host_fails_cleanly_and_says_what_to_do() {
    let host = FakeHost::new("bare");
    let report = doctor::run(&host.env());
    assert_eq!(
        statuses(&report),
        [
            ("cpu", Status::Failed),
            ("kvm", Status::Failed),
            ("qemu", Status::Failed),
            ("qemu-img", Status::Failed),
            ("vm-start", Status::Skipped),
            ("devices", Status::Skipped),
            ("firmware", Status::Skipped),
            ("shared-folders", Status::Skipped),
            ("gpu", Status::Skipped),
        ]
    );
    assert!(!report.can_run_vms());
    assert_eq!(check(&report, "qemu").fix.as_deref(), Some("Run: roomy setup"));
    assert!(check(&report, "kvm").fix.as_deref().unwrap().contains("modprobe kvm_amd or kvm_intel"));
    assert!(report.to_text().contains("roomy cannot run virtual machines yet: 4 problems to fix."));
}

#[test]
fn a_healthy_host_passes_every_trial() {
    let host = FakeHost::healthy("ok");
    let report = doctor::run(&host.env());
    let text = report.to_text();
    for id in ["cpu", "qemu", "qemu-img", "vm-start", "devices", "firmware", "shared-folders", "gpu"] {
        assert_eq!(check(&report, id).status, Status::Ok, "{id} in:\n{text}");
    }
    assert!(text.contains("System:       Test Linux\n"), "{text}");
    assert!(text.contains("Kernel:       7.2.10-test\n"), "{text}");
    assert!(text.contains("QEMU:         11.1.3 ("), "{text}");
    assert!(text.contains("Machine type: pc-q35-11.1\n"), "{text}");
    // The KVM module name follows the CPU vendor found in /proc/cpuinfo.
    assert!(check(&report, "kvm").fix.as_deref().unwrap().contains("sudo modprobe kvm_amd\n"));
    // Nothing is left behind in the runtime directory.
    assert_eq!(fs::read_dir(host.base.join("run")).unwrap().count(), 0);
}

#[test]
fn a_qemu_that_is_too_old_is_refused_before_any_trial() {
    let host = FakeHost::healthy("old");
    host.configure_qemu(r#"{"version": "10.0.3 (Debian 1:10.0.3+ds-0+deb13u1)"}"#);
    let report = doctor::run(&host.env());
    let qemu = check(&report, "qemu");
    assert_eq!(qemu.status, Status::Failed);
    assert_eq!(qemu.summary, "version 10.0.3 is too old; roomy needs 10.1.0 or newer");
    assert_eq!(check(&report, "vm-start").status, Status::Skipped);
    assert_eq!(check(&report, "gpu").status, Status::Skipped);
}

#[test]
fn missing_gpu_acceleration_is_a_warning_with_qemus_message() {
    let host = FakeHost::healthy("gl");
    host.configure_qemu(r#"{"reject": ["gl=on"]}"#);
    let report = doctor::run(&host.env());
    let gpu = check(&report, "gpu");
    assert_eq!(gpu.status, Status::Warning);
    assert_eq!(gpu.log.as_deref(), Some("qemu-system-x86_64: fake refusal of 'gl=on'"));
    assert_eq!(check(&report, "devices").status, Status::Ok);
    assert_eq!(report.count(Status::Warning), 1);
}

#[test]
fn a_rejected_display_configuration_blocks_and_shows_qemus_message() {
    let host = FakeHost::healthy("dbus");
    host.configure_qemu(r#"{"reject": ["dbus,p2p=yes,audiodev"]}"#);
    let report = doctor::run(&host.env());
    let devices = check(&report, "devices");
    assert_eq!(devices.status, Status::Failed);
    assert!(devices.log.as_deref().unwrap().contains("fake refusal"));
    assert!(devices.fix.as_deref().unwrap().contains("roomy setup"));
}

#[test]
fn a_qemu_without_its_display_module_is_one_problem_named_for_what_it_is() {
    let host = FakeHost::healthy("nodisplay");
    host.configure_qemu(r#"{"not_installed": ["ui-dbus"]}"#);
    let report = doctor::run(&host.env());
    let devices = check(&report, "devices");
    assert_eq!(devices.status, Status::Failed);
    assert_eq!(
        devices.summary,
        "QEMU is installed without its D-Bus display module, which roomy shows the screen through"
    );
    assert_eq!(devices.fix.as_deref(), Some("That is a package of its own on this system. Run: roomy setup"));
    assert_eq!(devices.log.as_deref(), Some("qemu-system-x86_64: Display 'dbus' is not available."));
    // Accelerated graphics need the same display: not a second problem, and not a missing graphics driver.
    assert_eq!(check(&report, "gpu").status, Status::Skipped);
    assert_eq!(report.count(Status::Warning), 0);
}

#[test]
fn a_missing_device_is_named() {
    let host = FakeHost::healthy("nodevice");
    host.configure_qemu(r#"{"not_installed": ["virtio-vga-gl"]}"#);
    let report = doctor::run(&host.env());
    assert_eq!(check(&report, "devices").status, Status::Ok);
    let gpu = check(&report, "gpu");
    assert_eq!(gpu.status, Status::Warning);
    assert_eq!(
        gpu.summary,
        "QEMU is installed without the 'virtio-vga-gl' device; the GPU acceleration switch will be unavailable"
    );
    assert_eq!(report.count(Status::Warning), 1);
}

#[test]
fn when_no_vm_can_start_the_remaining_trials_are_skipped() {
    let host = FakeHost::healthy("nokvm");
    host.configure_qemu(r#"{"reject": ["kvm"]}"#);
    let report = doctor::run(&host.env());
    let start = check(&report, "vm-start");
    assert_eq!(start.status, Status::Failed);
    assert!(start.fix.as_deref().unwrap().contains("KVM access check"));
    for id in ["devices", "firmware", "shared-folders", "gpu"] {
        assert_eq!(check(&report, id).status, Status::Skipped, "{id}");
    }
}

#[test]
fn software_emulation_does_not_count_as_a_working_vm() {
    let host = FakeHost::healthy("tcg");
    host.configure_qemu(r#"{"kvm": false}"#);
    let report = doctor::run(&host.env());
    let start = check(&report, "vm-start");
    assert_eq!(start.status, Status::Failed);
    assert_eq!(start.summary, "QEMU started, but without hardware acceleration");
}

#[test]
fn shared_folders_are_optional_and_firmware_is_not() {
    let host = FakeHost::healthy("opt");
    fs::remove_file(host.base.join("root/usr/lib/virtiofsd")).unwrap();
    fs::remove_file(host.base.join("root/usr/share/qemu/firmware/60-edk2-ovmf-x86_64-4m.json")).unwrap();
    let report = doctor::run(&host.env());
    assert_eq!(check(&report, "shared-folders").status, Status::Warning);
    assert_eq!(check(&report, "firmware").status, Status::Failed);
}
