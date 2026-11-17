//! Where roomy looks for things on the host.
//!
//! All host lookups go through [`Env`] so tests can point them at a fake
//! filesystem root and a fake `PATH`.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Env {
    root: PathBuf,
    /// Directories searched for programs, in order.
    pub search_path: Vec<PathBuf>,
    /// Per-user directory for sockets and other short-lived files.
    pub runtime_dir: PathBuf,
    /// The graphics card (render device) accelerated VMs should draw on, when
    /// the caller knows it: the window passes the card it is itself drawn
    /// with. Otherwise `host::display_gpu` decides.
    pub render_node: Option<PathBuf>,
}

impl Env {
    /// The real host.
    pub fn system() -> Env {
        let search_path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        // ROOMY_HOST_ROOT relocates host lookups; it exists for roomy's own end-to-end tests.
        let root = std::env::var_os("ROOMY_HOST_ROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        // ROOMY_RENDER_NODE names the graphics card for accelerated VMs; a window sets it for the VMs it starts.
        let render_node = std::env::var_os("ROOMY_RENDER_NODE").map(PathBuf::from).filter(|p| p.is_absolute());
        Env { root, search_path, runtime_dir, render_node }
    }

    /// A host whose filesystem lives under `root` (for tests).
    pub fn rooted(root: impl Into<PathBuf>, search_path: Vec<PathBuf>, runtime_dir: impl Into<PathBuf>) -> Env {
        Env { root: root.into(), search_path, runtime_dir: runtime_dir.into(), render_node: None }
    }

    /// Maps an absolute host path such as `/proc/cpuinfo` into this environment.
    pub fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches('/'))
    }

    pub fn read(&self, absolute: &str) -> io::Result<String> {
        std::fs::read_to_string(self.path(absolute))
    }

    /// Files in a host directory, sorted by name. A missing directory is empty.
    pub fn list(&self, absolute_dir: &str) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(self.path(absolute_dir))
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        entries.sort();
        entries
    }

    /// Finds a program on the search path.
    pub fn find_program(&self, name: &str) -> Option<PathBuf> {
        self.search_path.iter().map(|dir| dir.join(name)).find(|p| is_executable(p))
    }
}

pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}
