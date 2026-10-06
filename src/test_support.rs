//! Test-only cleanup guards for temporary paths created by tests.
//!
//! Every temporary file or directory a test creates is owned by a [`TestTempPath`]
//! and removed when the guard drops -- also when the test panics -- so a test run
//! leaves no litter behind in `TMPDIR`.

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Owns a temporary path and removes it (file, socket, or directory tree) on drop.
pub(crate) struct TestTempPath {
    /// The path handed to the test.
    path: PathBuf,
    /// Removed on drop: `path` itself, or the directory tree that contains it.
    cleanup: PathBuf,
}

impl TestTempPath {
    /// Guards a standalone path (a socket, file, or directory): removed on drop.
    pub(crate) fn owns(path: PathBuf) -> Self {
        Self {
            cleanup: path.clone(),
            path,
        }
    }

    /// Guards `path` inside `root`: the whole `root` tree is removed on drop.
    pub(crate) fn owns_root(root: PathBuf, path: PathBuf) -> Self {
        Self {
            path,
            cleanup: root,
        }
    }

    /// A sibling file inside the same guarded root, removed when either guard drops.
    pub(crate) fn sibling(&self, file_name: &str) -> Self {
        Self {
            path: self.path.with_file_name(file_name),
            cleanup: self.cleanup.clone(),
        }
    }
}

impl Deref for TestTempPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestTempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestTempPath {
    fn drop(&mut self) {
        // A guard may own a bare file (a socket) or a directory tree; try both.
        // Second call is a no-op; errors are ignored (the path may already be gone).
        let _ = std::fs::remove_file(&self.cleanup);
        let _ = std::fs::remove_dir_all(&self.cleanup);
    }
}
