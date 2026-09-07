use crate::LifecycleState;
use serde_json::Error as SerdeJsonError;
use snafu::{ResultExt, Snafu};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Atomic JSON persistence for the lifecycle transaction state.
#[derive(Clone, Debug)]
pub struct JsonFileStore {
    path: PathBuf,
    notification_path: PathBuf,
}

impl JsonFileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut notification_path = OsString::from(path.as_os_str());
        notification_path.push(".notify");
        Self {
            path,
            notification_path: PathBuf::from(notification_path),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn notification_path(&self) -> &Path {
        &self.notification_path
    }

    pub fn load(&self) -> Result<LifecycleState, StoreError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(LifecycleState::default());
            }
            Err(source) => {
                return Err(StoreError::OpenState {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        let state: LifecycleState =
            serde_json::from_reader(file).context(ParseStateSnafu { path: &self.path })?;
        if state.schema_version() != LifecycleState::SCHEMA_VERSION {
            return UnsupportedSchemaSnafu {
                path: self.path.clone(),
                actual: state.schema_version(),
                supported: LifecycleState::SCHEMA_VERSION,
            }
            .fail();
        }
        Ok(state)
    }

    pub fn save(&self, state: &LifecycleState) -> Result<(), StoreError> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        fs::create_dir_all(parent).context(CreateStateDirectorySnafu { path: parent })?;
        let mut temporary =
            NamedTempFile::new_in(parent).context(CreateTemporaryStateSnafu { path: parent })?;
        serde_json::to_writer_pretty(temporary.as_file_mut(), state)
            .context(SerializeStateSnafu { path: &self.path })?;
        temporary
            .as_file()
            .sync_all()
            .context(SyncTemporaryStateSnafu {
                path: temporary.path(),
            })?;
        temporary
            .persist(&self.path)
            .map_err(|error| StoreError::PersistState {
                path: self.path.clone(),
                source: error.error,
            })?;

        let parent_directory =
            File::open(parent).context(OpenStateDirectorySnafu { path: parent })?;
        parent_directory
            .sync_all()
            .context(SyncStateDirectorySnafu { path: parent })?;

        let mut notification = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(&self.notification_path)
            .context(OpenNotificationSnafu {
                path: &self.notification_path,
            })?;
        notification
            .write_all(b"\n")
            .context(WriteNotificationSnafu {
                path: &self.notification_path,
            })?;
        notification.sync_all().context(SyncNotificationSnafu {
            path: &self.notification_path,
        })?;
        parent_directory
            .sync_all()
            .context(SyncStateDirectorySnafu { path: parent })?;
        Ok(())
    }
}

#[derive(Debug, Snafu)]
pub enum StoreError {
    #[snafu(display("failed to create state directory '{}': {}", path.display(), source))]
    CreateStateDirectory { path: PathBuf, source: io::Error },

    #[snafu(display("failed to open lifecycle state '{}': {}", path.display(), source))]
    OpenState { path: PathBuf, source: io::Error },

    #[snafu(display("failed to parse lifecycle state '{}': {}", path.display(), source))]
    ParseState {
        path: PathBuf,
        source: SerdeJsonError,
    },

    #[snafu(display(
        "lifecycle state '{}' uses schema version {}, but this binary supports {}",
        path.display(),
        actual,
        supported
    ))]
    UnsupportedSchema {
        path: PathBuf,
        actual: u32,
        supported: u32,
    },

    #[snafu(display("failed to create temporary state in '{}': {}", path.display(), source))]
    CreateTemporaryState { path: PathBuf, source: io::Error },

    #[snafu(display("failed to serialize lifecycle state '{}': {}", path.display(), source))]
    SerializeState {
        path: PathBuf,
        source: SerdeJsonError,
    },

    #[snafu(display("failed to sync temporary state '{}': {}", path.display(), source))]
    SyncTemporaryState { path: PathBuf, source: io::Error },

    #[snafu(display("failed to persist lifecycle state '{}': {}", path.display(), source))]
    PersistState { path: PathBuf, source: io::Error },

    #[snafu(display("failed to open state directory '{}': {}", path.display(), source))]
    OpenStateDirectory { path: PathBuf, source: io::Error },

    #[snafu(display("failed to sync state directory '{}': {}", path.display(), source))]
    SyncStateDirectory { path: PathBuf, source: io::Error },

    #[snafu(display("failed to open lifecycle notification '{}': {}", path.display(), source))]
    OpenNotification { path: PathBuf, source: io::Error },

    #[snafu(display(
        "failed to write lifecycle notification '{}': {}",
        path.display(),
        source
    ))]
    WriteNotification { path: PathBuf, source: io::Error },

    #[snafu(display("failed to sync lifecycle notification '{}': {}", path.display(), source))]
    SyncNotification { path: PathBuf, source: io::Error },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcceleratorProfile, NextAction};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tempfile::TempDir;

    const TOKEN: &str = "transition-persisted";

    #[test]
    fn missing_state_file_loads_default_state() {
        let directory = TempDir::new().unwrap();
        let store = JsonFileStore::new(directory.path().join("state.json"));

        let state = store.load().unwrap();
        assert_eq!(state, LifecycleState::default());
        assert_eq!(state.schema_version(), LifecycleState::SCHEMA_VERSION);
    }

    #[test]
    fn active_transition_survives_round_trip() {
        let directory = TempDir::new().unwrap();
        let store = JsonFileStore::new(directory.path().join("nested/state.json"));
        let mut expected = LifecycleState::new(Some(AcceleratorProfile::SharedInference));
        let generation = expected.generation();
        let id = expected
            .begin(TOKEN, AcceleratorProfile::DistributedTraining, generation)
            .unwrap();
        expected.record_node_drained(&id).unwrap();
        expected.record_advertisement_withdrawn(&id).unwrap();

        store.save(&expected).unwrap();
        let actual = store.load().unwrap();

        assert_eq!(actual, expected);
        assert_eq!(actual.active_transition_id(), Some(&id));
        assert_eq!(
            actual.next_action(),
            Some(NextAction::NodeApplyTargetProfile)
        );
    }

    #[test]
    fn save_atomically_replaces_previous_state() {
        let directory = TempDir::new().unwrap();
        let store = JsonFileStore::new(directory.path().join("state.json"));
        let first = LifecycleState::new(Some(AcceleratorProfile::SharedInference));
        let second = LifecycleState::new(Some(AcceleratorProfile::DistributedTraining));

        store.save(&first).unwrap();
        let first_notification = fs::metadata(store.notification_path()).unwrap();
        store.save(&second).unwrap();
        let second_notification = fs::metadata(store.notification_path()).unwrap();

        assert_eq!(store.load().unwrap(), second);
        assert_eq!(
            first_notification.ino(),
            second_notification.ino(),
            "notification inode must remain stable across atomic state replacement"
        );
        assert_eq!(second_notification.len(), 1);
        assert_eq!(second_notification.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("state.json");
        fs::write(
            &path,
            r#"{"schema_version":1,"generation":0,"committed_profile":null,"active_transition":null,"last_result":null,"unexpected":true}"#,
        )
        .unwrap();
        let store = JsonFileStore::new(path);

        assert!(matches!(store.load(), Err(StoreError::ParseState { .. })));
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("state.json");
        fs::write(
            &path,
            r#"{"schema_version":99,"generation":0,"committed_profile":null,"active_transition":null,"last_result":null}"#,
        )
        .unwrap();
        let store = JsonFileStore::new(path);

        assert!(matches!(
            store.load(),
            Err(StoreError::UnsupportedSchema {
                actual: 99,
                supported: LifecycleState::SCHEMA_VERSION,
                ..
            })
        ));
    }
}
