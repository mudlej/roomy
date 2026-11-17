//! Helpers shared by the unit tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh scratch directory, removed again when the value is dropped.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// `name` only helps to tell directories apart while a test runs; each call gets its own.
    pub(crate) fn new(name: &str) -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        // Kept short: unix socket paths under it must stay below 108 bytes.
        let dir = std::env::temp_dir().join(format!("roomy-{name}-{}-{unique}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
