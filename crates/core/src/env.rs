//! Where roomy looks for things on the host.
//!
//! All host lookups go through [`Env`] so tests can point them at a fake
//! filesystem root and a fake `PATH`.

use crate::{Result, err, host};
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
        let search_path = process_search_path();
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
        sorted_entries(&self.path(absolute_dir))
    }

    /// Finds a program on the search path.
    pub fn find_program(&self, name: &str) -> Option<PathBuf> {
        self.search_path.iter().map(|dir| dir.join(name)).find(|p| is_executable(p))
    }

    /// Finds a program roomy cannot do without; the error tells the user how to install it.
    pub fn require(&self, program: &str) -> Result<PathBuf> {
        // QEMU's program name means little to most people.
        let called = if program == host::QEMU_PROGRAM { "QEMU" } else { program };
        self.find_program(program).ok_or_else(|| err!("{called} is not installed (run: {} setup)", roomy_command()))
    }

    /// The graphics card accelerated VMs are drawn on: the one the caller named, else the one
    /// the desktop is shown with (see [`host::display_gpu`]).
    pub fn vm_render_node(&self) -> Option<PathBuf> {
        self.render_node.clone().or_else(|| host::display_gpu(self).map(|gpu| gpu.render_node))
    }
}

/// Everything in a directory, sorted by name. A directory that cannot be read is empty.
pub(crate) fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> =
        std::fs::read_dir(dir).map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect()).unwrap_or_default();
    entries.sort();
    entries
}

/// The directories this process searches for programs (`PATH`).
fn process_search_path() -> Vec<PathBuf> {
    std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default()
}

/// A directory named by an environment variable, when it is set to an absolute path.
pub(crate) fn dir_from_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).filter(|path| path.is_absolute())
}

/// The user's home folder (`HOME`).
pub(crate) fn home_dir() -> Result<PathBuf> {
    dir_from_env("HOME").ok_or_else(|| err!("cannot tell where your home folder is (HOME is not set)"))
}

/// The user's Downloads folder: the one their desktop names in `user-dirs.dirs`, else `~/Downloads`.
pub fn downloads_dir() -> Result<PathBuf> {
    let home = home_dir()?;
    let config = dir_from_env("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    let named =
        std::fs::read_to_string(config.join("user-dirs.dirs")).ok().and_then(|text| download_dir_in(&text, &home));
    Ok(named.unwrap_or_else(|| home.join("Downloads")))
}

/// Where roomy keeps files it can always fetch again: `$XDG_CACHE_HOME/roomy`, by default `~/.cache/roomy`.
pub fn cache_dir() -> Result<PathBuf> {
    let cache = match dir_from_env("XDG_CACHE_HOME") {
        Some(dir) => dir,
        None => home_dir()?.join(".cache"),
    };
    Ok(cache.join("roomy"))
}

/// `XDG_DOWNLOAD_DIR` in the text of a `user-dirs.dirs` file.
fn download_dir_in(user_dirs: &str, home: &Path) -> Option<PathBuf> {
    let value = user_dirs.lines().find_map(|line| line.trim().strip_prefix("XDG_DOWNLOAD_DIR="))?;
    let value = value.trim().trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    // A folder pointed at the home folder itself is one the user switched off.
    (path.is_absolute() && path != home).then_some(path)
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
        command_for(file.as_deref(), &process_search_path())
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
    use crate::test_support::TempDir;

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
    fn the_downloads_folder_is_the_one_the_desktop_names() {
        let home = Path::new("/home/me");
        let named = |text: &str| download_dir_in(text, home);
        assert_eq!(
            named("XDG_DESKTOP_DIR=\"$HOME/Desktop\"\nXDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n"),
            Some(home.join("Téléchargements"))
        );
        assert_eq!(named("XDG_DOWNLOAD_DIR=\"/data/incoming\""), Some(PathBuf::from("/data/incoming")));
        assert_eq!(named("XDG_DOWNLOAD_DIR=\"$HOME/\""), None);
        assert_eq!(named("XDG_MUSIC_DIR=\"$HOME/Music\""), None);
    }

    #[test]
    fn messages_name_a_command_the_user_can_actually_type() {
        let dir = TempDir::new("command");
        let downloads = dir.join("My Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let file = downloads.join("roomy-x86_64.AppImage");
        std::fs::copy(std::env::current_exe().unwrap(), &file).unwrap();

        assert_eq!(command_for(None, &[]), "roomy");
        // Saved somewhere the shell does not search: only the whole path works, quoted where needed.
        assert_eq!(command_for(Some(&file), &[dir.to_path_buf()]), format!("'{}'", file.display()));
        // Saved where the shell searches: its name is enough.
        assert_eq!(command_for(Some(&file), &[dir.to_path_buf(), downloads.clone()]), "roomy-x86_64.AppImage");
        assert_eq!(shell_word("/home/me/it's here"), r"'/home/me/it'\''s here'");
    }
}
