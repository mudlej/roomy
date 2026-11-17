//! Where roomy looks for things on the host.
//!
//! All host lookups go through [`Env`] so tests can point them at a fake
//! filesystem root and a fake `PATH`.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// The program to start for another `roomy` process that must outlive this one, such as a VM's
/// supervisor: normally this very program.
///
/// Shipped as a single file (an AppImage), this program runs from a mount that belongs to the
/// process that opened the file and goes away with it. The file itself is named instead, so the
/// new process gets a mount of its own for as long as it lives.
pub fn own_program() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(single_file(&exe).unwrap_or(exe))
}

/// The single file this program was started from, when it was shipped as one.
pub fn single_file_release() -> Option<PathBuf> {
    single_file(&std::env::current_exe().ok()?)
}

fn single_file(exe: &Path) -> Option<PathBuf> {
    let from_env = |name: &str| std::env::var_os(name).map(PathBuf::from);
    single_file_of(exe, from_env("APPIMAGE"), from_env("APPDIR"))
}

/// What the user types in a terminal to run roomy, for messages that tell them to.
///
/// Installed, that is `roomy`. Shipped as a single file there is no such command: the file is
/// run from wherever it was saved, so the file is named, by its bare name when the search path
/// leads to it.
pub fn roomy_command() -> &'static str {
    static COMMAND: OnceLock<String> = OnceLock::new();
    COMMAND.get_or_init(|| {
        let file = std::env::current_exe().ok().and_then(|exe| single_file(&exe));
        let search_path: Vec<PathBuf> =
            std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
        command_for(file.as_deref(), &search_path)
    })
}

fn command_for(single_file: Option<&Path>, search_path: &[PathBuf]) -> String {
    let Some(file) = single_file else { return "roomy".to_owned() };
    let reached_by_name = file.file_name().filter(|name| {
        let found = search_path.iter().map(|dir| dir.join(name)).find(|candidate| is_executable(candidate));
        found.is_some_and(|found| found.canonicalize().ok() == file.canonicalize().ok())
    });
    shell_word(&reached_by_name.map(Path::new).unwrap_or(file).to_string_lossy())
}

/// `text` as one word of a shell command line.
pub fn shell_word(text: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The single file `exe` was started from, going by what its launcher put in the environment.
/// Every program started from such a file inherits those variables, including unrelated ones
/// (a terminal shipped the same way), so they only count when `exe` really is inside the mount.
fn single_file_of(exe: &Path, file: Option<PathBuf>, mount: Option<PathBuf>) -> Option<PathBuf> {
    let (file, mount) = (file?, mount?);
    (mount.is_absolute() && exe.starts_with(&mount) && is_executable(&file)).then_some(file)
}

pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_single_file_is_only_used_by_a_program_that_runs_from_it() {
        let file = std::env::current_exe().unwrap(); // any executable file will do
        let mount = PathBuf::from("/tmp/.mount_roomyAbCdEf");
        let inside = mount.join("usr/bin/roomy");
        let some = |path: &Path| Some(path.to_path_buf());
        assert_eq!(single_file_of(&inside, some(&file), some(&mount)), Some(file.clone()));
        // Started from someone else's single file (a terminal, say): the variables are theirs.
        assert_eq!(single_file_of(Path::new("/usr/bin/roomy"), some(&file), some(&mount)), None);
        // Not started from a single file at all, or the file has gone.
        assert_eq!(single_file_of(&inside, None, None), None);
        assert_eq!(single_file_of(&inside, some(Path::new("/nonexistent/roomy.AppImage")), some(&mount)), None);
        assert_eq!(single_file_of(&inside, some(&file), some(Path::new("relative/mount"))), None);
    }

    #[test]
    fn messages_name_a_command_the_user_can_actually_type() {
        let dir = std::env::temp_dir().join(format!("roomy-command-{}", std::process::id()));
        let downloads = dir.join("My Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let file = downloads.join("roomy-x86_64.AppImage");
        std::fs::copy(std::env::current_exe().unwrap(), &file).unwrap();

        assert_eq!(command_for(None, &[]), "roomy");
        // Saved somewhere the shell does not search: only the whole path works, quoted where needed.
        assert_eq!(command_for(Some(&file), std::slice::from_ref(&dir)), format!("'{}'", file.display()));
        // Saved where the shell searches: its name is enough.
        assert_eq!(command_for(Some(&file), &[dir.clone(), downloads.clone()]), "roomy-x86_64.AppImage");
        assert_eq!(shell_word("/home/me/it's here"), r"'/home/me/it'\''s here'");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
