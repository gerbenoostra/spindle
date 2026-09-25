//! A throwaway directory, removed when the guard drops.
//!
//! Not `tempfile`: the crate's whole dependency list is two lines long, and
//! this is twenty lines of it. Each directory is unique per process and per
//! call, because `cargo test` runs its tests in threads.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        Self::new_in(&std::env::temp_dir(), label)
    }

    /// As [`new`], under a chosen root: unix socket paths cap out near a
    /// hundred bytes, which the default temp root mostly spends by itself.
    pub fn new_in(root: &Path, label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = root.join(format!(
            "agent-sessions-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("a temp directory can be created");
        // The macOS temp directory is itself a symlink chain, and this whole
        // module is about paths that resolve: a test comparing against an
        // unresolved path would fail for a reason that is not the test's.
        let path = path.canonicalize().expect("the temp directory resolves");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }

    /// Write a file under this directory and return its path.
    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("a parent directory can be created");
        }
        fs::write(&path, contents).expect("the file can be written");
        path
    }

    /// The names in this directory, sorted, so an assertion about what a run
    /// left behind is stable.
    pub fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.path)
            .expect("the directory can be read")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A test that made something read-only has to be able to clean up
        // after itself, so put the modes back before removing the tree.
        restore_modes(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn restore_modes(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            restore_modes(&child);
        } else {
            let _ = fs::set_permissions(&child, fs::Permissions::from_mode(0o644));
        }
    }
}
