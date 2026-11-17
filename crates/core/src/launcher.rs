//! roomy's entry in the desktop's application menu, for a single-file release.
//!
//! An installed package brings a launcher entry and an icon. A single file has
//! nowhere to bring them to, and a desktop shows a window's icon by looking up
//! the launcher entry that carries the window's application name: without one
//! the window has no icon and roomy is not in the menu. So a single-file roomy
//! keeps an entry of its own in the user's data folder, pointing at the file.

use crate::env::{dir_from_env, home_dir};
use std::io;
use std::path::{Path, PathBuf};

/// The application's name towards the desktop; launcher entry and icon are filed under it.
pub const APP_ID: &str = "com.mudlej.Roomy";

const ICON: &str = include_str!("../../../packaging/com.mudlej.Roomy.svg");
const ENTRY: &str = include_str!("../../../packaging/com.mudlej.Roomy.desktop");
/// Marks an entry as written here, and so as one that may be rewritten here.
const OURS: &str = "X-Roomy-SingleFile=true";

/// Makes the user's application menu lead to the single file `file`, following the usual
/// environment variables for where such entries live.
pub fn keep_for_user(file: &Path) -> io::Result<()> {
    let data_home = dir_from_env("XDG_DATA_HOME")
        .or_else(|| home_dir().ok().map(|home| home.join(".local/share")))
        .ok_or_else(|| io::Error::other("no home folder"))?;
    let system = match std::env::var_os("XDG_DATA_DIRS").filter(|dirs| !dirs.is_empty()) {
        Some(dirs) => std::env::split_paths(&dirs).collect(),
        None => vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")],
    };
    keep(&data_home, &system, file)
}

/// Writes the launcher entry and icon under `data_home` unless they are there already.
///
/// Nothing is written where roomy is also installed as a package (an entry in `system_dirs`),
/// and an entry of the same name that the user made themselves is left as it is.
pub fn keep(data_home: &Path, system_dirs: &[PathBuf], file: &Path) -> io::Result<()> {
    let entry_in = |dir: &Path| dir.join("applications").join(format!("{APP_ID}.desktop"));
    if system_dirs.iter().any(|dir| dir != data_home && entry_in(dir).exists()) {
        return Ok(());
    }
    let entry = entry_in(data_home);
    if std::fs::read_to_string(&entry).is_ok_and(|existing| !existing.lines().any(|line| line == OURS)) {
        return Ok(());
    }
    write_if_different(&entry, &entry_for(file))?;
    write_if_different(&data_home.join("icons/hicolor/scalable/apps").join(format!("{APP_ID}.svg")), ICON)
}

/// The packaged entry, starting `file` instead of an installed `roomy`.
fn entry_for(file: &Path) -> String {
    let path = file.to_string_lossy();
    let mut text = String::new();
    for line in ENTRY.lines() {
        if line.starts_with("Exec=") {
            // Without the file the entry leads nowhere; TryExec lets menus leave it out then.
            text.push_str(&format!("TryExec={}\n", path.replace('\\', "\\\\")));
            text.push_str(&format!("Exec={}\n", exec_word(&path)));
        } else {
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push_str(OURS);
    text.push('\n');
    text
}

/// `path` as the program of an `Exec=` line: quoted as that line's own rules ask, and with
/// backslashes doubled once more, as in every value of a launcher entry.
fn exec_word(path: &str) -> String {
    let mut word = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' => word.extend(['\\', '\\', c]),
            '\\' => word.push_str("\\\\\\\\"),
            '%' => word.push_str("%%"),
            _ => word.push(c),
        }
    }
    word.push('"');
    word
}

fn write_if_different(path: &Path, content: &str) -> io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn the_menu_entry_follows_the_file_and_brings_the_icon() {
        let dir = TempDir::new("launcher");
        let (home, system) = (dir.join("home"), vec![dir.join("usr/share")]);
        let entry = home.join("applications/com.mudlej.Roomy.desktop");
        let icon = home.join("icons/hicolor/scalable/apps/com.mudlej.Roomy.svg");

        keep(&home, &system, Path::new("/home/me/Downloads/roomy-x86_64.AppImage")).unwrap();
        let text = std::fs::read_to_string(&entry).unwrap();
        assert!(text.contains("\nExec=\"/home/me/Downloads/roomy-x86_64.AppImage\"\n"), "{text}");
        assert!(text.contains("\nTryExec=/home/me/Downloads/roomy-x86_64.AppImage\n"), "{text}");
        assert!(text.contains("\nIcon=com.mudlej.Roomy\n"), "{text}");
        assert!(std::fs::read_to_string(&icon).unwrap().contains("<svg"));

        // The file was moved and started from its new place.
        keep(&home, &system, Path::new("/opt/my apps/roomy")).unwrap();
        let text = std::fs::read_to_string(&entry).unwrap();
        assert!(text.contains("\nExec=\"/opt/my apps/roomy\"\n"), "{text}");
        assert!(!text.contains("Downloads"), "{text}");
    }

    #[test]
    fn an_installed_roomy_or_an_entry_of_the_users_own_is_left_alone() {
        let dir = TempDir::new("launcher");
        let (home, system) = (dir.join("home"), vec![dir.join("usr/share")]);
        let entry = home.join("applications/com.mudlej.Roomy.desktop");
        let file = Path::new("/home/me/roomy.AppImage");

        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, "[Desktop Entry]\nExec=my-own-wrapper\n").unwrap();
        keep(&home, &system, file).unwrap();
        assert_eq!(std::fs::read_to_string(&entry).unwrap(), "[Desktop Entry]\nExec=my-own-wrapper\n");
        std::fs::remove_file(&entry).unwrap();

        let packaged = system[0].join("applications/com.mudlej.Roomy.desktop");
        std::fs::create_dir_all(packaged.parent().unwrap()).unwrap();
        std::fs::write(&packaged, "[Desktop Entry]\n").unwrap();
        keep(&home, &system, file).unwrap();
        assert!(!entry.exists());
    }

    #[test]
    fn unusual_file_names_survive_the_entrys_quoting() {
        assert_eq!(exec_word("/a b/50% \"off\" $x\\y"), r#""/a b/50%% \\"off\\" \\$x\\\\y""#);
    }
}
