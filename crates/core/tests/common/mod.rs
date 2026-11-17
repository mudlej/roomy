//! A fake host for integration tests.
//!
//! It is a directory tree standing in for `/` plus a `bin` directory standing
//! in for `PATH`, holding stand-in programs from `tests/fixtures` (Python
//! scripts, so the test suite needs `python3`). `/dev/kvm` cannot be faked,
//! so the doctor's KVM access check fails on every fake host.

#![allow(dead_code)] // each test file uses a different part

use roomy_core::env::Env;
use std::fs;
use std::path::{Path, PathBuf};

pub struct FakeHost {
    pub base: PathBuf,
}

impl FakeHost {
    pub fn new(name: &str) -> FakeHost {
        // Kept short: unix socket paths under it must stay below 108 bytes.
        let base = PathBuf::from(format!("/tmp/roomy-t-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for dir in ["root", "bin", "run"] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        FakeHost { base }
    }

    pub fn env(&self) -> Env {
        Env::rooted(self.base.join("root"), vec![self.base.join("bin")], self.base.join("run"))
    }

    pub fn write(&self, absolute: &str, content: &str) -> PathBuf {
        let path = self.base.join("root").join(absolute.trim_start_matches('/'));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    pub fn install(&self, fixture: &str, destination: &Path) {
        // Works from any crate in the workspace: the fixtures live with roomy-core.
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures").join(fixture);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        // A link, not a copy. Copying means this process briefly holds the new program open for
        // writing; a test on another thread that starts a child at that moment passes the open
        // file on to it, and running the program then fails with "text file busy".
        let _ = fs::remove_file(destination);
        std::os::unix::fs::symlink(source.canonicalize().unwrap(), destination).unwrap();
    }

    /// Everything a working host has, apart from `/dev/kvm`.
    pub fn healthy(name: &str) -> FakeHost {
        let host = FakeHost::new(name);
        host.write("/etc/os-release", "PRETTY_NAME=\"Test Linux\"\nID=testlinux\nID_LIKE=arch\n");
        host.write("/proc/sys/kernel/osrelease", "7.2.10-test\n");
        host.write("/proc/cpuinfo", "processor\t: 0\nflags\t\t: fpu vme svm sse2\n");
        host.write("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "code");
        host.write("/usr/share/edk2/x64/OVMF_VARS.4m.fd", "vars");
        host.write(
            "/usr/share/qemu/firmware/60-edk2-ovmf-x86_64-4m.json",
            r#"{
                "interface-types": ["uefi"],
                "mapping": {
                    "device": "flash",
                    "executable": {"filename": "/usr/share/edk2/x64/OVMF_CODE.4m.fd", "format": "raw"},
                    "nvram-template": {"filename": "/usr/share/edk2/x64/OVMF_VARS.4m.fd", "format": "raw"}
                },
                "targets": [{"architecture": "x86_64", "machines": ["pc-q35-*"]}],
                "features": ["acpi-s3", "verbose-dynamic"]
            }"#,
        );
        host.install("fake-qemu", &host.base.join("bin/qemu-system-x86_64"));
        host.install("fake-qemu-img", &host.base.join("bin/qemu-img"));
        host.install("fake-virtiofsd", &host.base.join("root/usr/lib/virtiofsd"));
        host
    }

    /// Tunes the stand-in QEMU; see `tests/fixtures/fake-qemu` for the keys.
    pub fn configure_qemu(&self, json: &str) {
        fs::write(self.base.join("bin/fake-qemu.conf"), json).unwrap();
    }

    pub fn configure_qemu_img(&self, json: &str) {
        fs::write(self.base.join("bin/fake-qemu-img.conf"), json).unwrap();
    }

    /// What the stand-in QEMU was asked so far: `start ...`, `qmp ...` and `agent ...` lines.
    pub fn qemu_log(&self) -> Vec<String> {
        fs::read_to_string(self.base.join("bin/fake-qemu.log")).unwrap_or_default().lines().map(str::to_owned).collect()
    }
}

impl Drop for FakeHost {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}
