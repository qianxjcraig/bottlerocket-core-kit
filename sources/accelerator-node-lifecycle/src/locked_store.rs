use crate::{JsonFileStore, LifecycleState, StoreError};
use fs2::FileExt;
use snafu::{ResultExt, Snafu};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// A JSON lifecycle store with a stable advisory lock file for read-modify-write operations.
#[derive(Clone, Debug)]
pub struct LockedJsonFileStore {
    store: JsonFileStore,
    lock_path: PathBuf,
}

impl LockedJsonFileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut lock_path = OsString::from(path.as_os_str());
        lock_path.push(".lock");
        Self {
            store: JsonFileStore::new(path),
            lock_path: PathBuf::from(lock_path),
        }
    }

    pub fn state_path(&self) -> &Path {
        self.store.path()
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Acquires the store without waiting, so a stuck caller cannot block lifecycle progress.
    pub fn try_lock(&self) -> Result<LockedJsonFileStoreGuard<'_>, LockedStoreError> {
        let parent = self
            .lock_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).context(CreateLockDirectorySnafu { path: parent })?;
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .context(OpenLockFileSnafu {
                path: &self.lock_path,
            })?;
        FileExt::try_lock_exclusive(&lock_file).context(AcquireLockSnafu {
            path: &self.lock_path,
        })?;

        Ok(LockedJsonFileStoreGuard {
            store: &self.store,
            _lock_file: lock_file,
        })
    }
}

pub struct LockedJsonFileStoreGuard<'a> {
    store: &'a JsonFileStore,
    _lock_file: File,
}

impl LockedJsonFileStoreGuard<'_> {
    pub fn load(&self) -> Result<LifecycleState, StoreError> {
        self.store.load()
    }

    pub fn save(&self, state: &LifecycleState) -> Result<(), StoreError> {
        self.store.save(state)
    }
}

#[derive(Debug, Snafu)]
pub enum LockedStoreError {
    #[snafu(display("failed to create lock directory '{}': {}", path.display(), source))]
    CreateLockDirectory { path: PathBuf, source: io::Error },

    #[snafu(display("failed to open lifecycle lock '{}': {}", path.display(), source))]
    OpenLockFile { path: PathBuf, source: io::Error },

    #[snafu(display("lifecycle state is already locked at '{}': {}", path.display(), source))]
    AcquireLock { path: PathBuf, source: io::Error },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lock_uses_a_stable_sibling_file() {
        let directory = TempDir::new().unwrap();
        let state_path = directory.path().join("state.json");
        let store = LockedJsonFileStore::new(&state_path);

        assert_eq!(store.state_path(), state_path);
        assert_eq!(
            store.lock_path(),
            directory.path().join("state.json.lock").as_path()
        );
    }

    #[test]
    fn concurrent_writer_is_rejected() {
        let directory = TempDir::new().unwrap();
        let store = LockedJsonFileStore::new(directory.path().join("state.json"));
        let first = store.try_lock().unwrap();

        assert!(matches!(
            store.try_lock(),
            Err(LockedStoreError::AcquireLock { .. })
        ));

        drop(first);
        assert!(store.try_lock().is_ok());
    }

    #[test]
    fn guard_persists_state() {
        let directory = TempDir::new().unwrap();
        let store = LockedJsonFileStore::new(directory.path().join("state.json"));
        let guard = store.try_lock().unwrap();
        let expected = LifecycleState::default();

        guard.save(&expected).unwrap();
        assert_eq!(guard.load().unwrap(), expected);
    }
}
