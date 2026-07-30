use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::lifecycle::LaunchBudget;

const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum JournalStage {
    Prepared,
    VolumeReady,
    PodRequested,
    PodCreated,
    Ready,
    CleanupPending,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionJournal {
    pub schema_version: u8,
    pub launch_id: String,
    pub pod_name: String,
    pub credential_profile_id: String,
    pub preset_id: String,
    pub data_center_id: String,
    pub budget: LaunchBudget,
    pub idle_timeout_minutes: u16,
    pub created_at_epoch_ms: u64,
    pub stage: JournalStage,
    pub volume_id: Option<String>,
    pub pod_id: Option<String>,
    pub last_error: Option<String>,
}

pub struct NewSessionJournal {
    pub credential_profile_id: String,
    pub preset_id: String,
    pub data_center_id: String,
    pub budget: LaunchBudget,
    pub idle_timeout_minutes: u16,
    pub created_at_epoch_ms: u64,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("could not generate a session identifier: {0}")]
    Random(String),
    #[error("could not read {path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("session journal {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("could not write {path}: {message}")]
    Write { path: PathBuf, message: String },
}

pub struct SessionJournalStore;

impl SessionJournalStore {
    pub fn prepare(
        path: &Path,
        install_id: &str,
        new_session: NewSessionJournal,
    ) -> Result<SessionJournal, JournalError> {
        let launch_id = random_hex(12)?;
        let journal = SessionJournal {
            schema_version: SCHEMA_VERSION,
            pod_name: format!(
                "mintpod-{}-{}",
                short_id(install_id, 8),
                short_id(&launch_id, 12)
            ),
            launch_id,
            credential_profile_id: new_session.credential_profile_id,
            preset_id: new_session.preset_id,
            data_center_id: new_session.data_center_id,
            budget: new_session.budget,
            idle_timeout_minutes: new_session.idle_timeout_minutes,
            created_at_epoch_ms: new_session.created_at_epoch_ms,
            stage: JournalStage::Prepared,
            volume_id: None,
            pod_id: None,
            last_error: None,
        };
        Self::save(path, &journal)?;
        Ok(journal)
    }

    pub fn load(path: &Path) -> Result<Option<SessionJournal>, JournalError> {
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read(path).map_err(|error| JournalError::Read {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        let journal: SessionJournal =
            serde_json::from_slice(&contents).map_err(|error| JournalError::Invalid {
                path: path.to_owned(),
                message: error.to_string(),
            })?;
        validate(path, &journal)?;
        Ok(Some(journal))
    }

    pub fn save(path: &Path, journal: &SessionJournal) -> Result<(), JournalError> {
        validate(path, journal)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| JournalError::Write {
                path: path.to_owned(),
                message: error.to_string(),
            })?;
        }
        let mut contents =
            serde_json::to_vec_pretty(journal).expect("a session journal always serializes");
        contents.push(b'\n');
        let mut file = AtomicWriteFile::open(path).map_err(|error| JournalError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        secure_permissions(&file).map_err(|error| JournalError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        file.write_all(&contents)
            .and_then(|_| file.sync_all())
            .map_err(|error| JournalError::Write {
                path: path.to_owned(),
                message: error.to_string(),
            })?;
        file.commit().map_err(|error| JournalError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })
    }

    pub fn clear(path: &Path) -> Result<(), JournalError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(JournalError::Write {
                path: path.to_owned(),
                message: error.to_string(),
            }),
        }
    }
}

pub fn load_or_create_install_id(path: &Path) -> Result<String, JournalError> {
    if path.exists() {
        let id = fs::read_to_string(path).map_err(|error| JournalError::Read {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        let id = id.trim();
        if valid_hex_id(id, 24) {
            return Ok(id.to_owned());
        }
        return Err(JournalError::Invalid {
            path: path.to_owned(),
            message: "install id must be 24 lowercase hexadecimal characters".to_owned(),
        });
    }

    let id = random_hex(12)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| JournalError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    }
    let mut file = AtomicWriteFile::open(path).map_err(|error| JournalError::Write {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    secure_permissions(&file).map_err(|error| JournalError::Write {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    file.write_all(format!("{id}\n").as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|error| JournalError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    file.commit().map_err(|error| JournalError::Write {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    Ok(id)
}

fn validate(path: &Path, journal: &SessionJournal) -> Result<(), JournalError> {
    if journal.schema_version != SCHEMA_VERSION {
        return Err(JournalError::Invalid {
            path: path.to_owned(),
            message: format!("unsupported schema version {}", journal.schema_version),
        });
    }
    if !valid_hex_id(&journal.launch_id, 24)
        || journal.pod_name.len() > 64
        || !journal.pod_name.starts_with("mintpod-")
        || journal.credential_profile_id.trim().is_empty()
        || journal.preset_id.trim().is_empty()
        || journal.data_center_id.trim().is_empty()
    {
        return Err(JournalError::Invalid {
            path: path.to_owned(),
            message: "journal contains invalid ownership fields".to_owned(),
        });
    }
    journal
        .budget
        .validate()
        .map_err(|error| JournalError::Invalid {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    if !(1..=240).contains(&journal.idle_timeout_minutes) {
        return Err(JournalError::Invalid {
            path: path.to_owned(),
            message: "idle timeout must be between 1 and 240 minutes".to_owned(),
        });
    }
    Ok(())
}

fn random_hex(byte_count: usize) -> Result<String, JournalError> {
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes).map_err(|error| JournalError::Random(error.to_string()))?;
    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

fn valid_hex_id(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn short_id(value: &str, length: usize) -> &str {
    &value[..value.len().min(length)]
}

#[cfg(unix)]
fn secure_permissions(file: &std::fs::File) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn secure_permissions(_file: &std::fs::File) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mintpod-journal-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn journal_is_written_before_remote_ids_exist() {
        let directory = test_dir();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("active-session.json");
        let journal = SessionJournalStore::prepare(
            &path,
            "0123456789abcdef01234567",
            NewSessionJournal {
                credential_profile_id: "profile".to_owned(),
                preset_id: "coder".to_owned(),
                data_center_id: "EU-RO-1".to_owned(),
                budget: LaunchBudget::Time { minutes: 30 },
                idle_timeout_minutes: 10,
                created_at_epoch_ms: 42,
            },
        )
        .unwrap();

        let loaded = SessionJournalStore::load(&path).unwrap().unwrap();
        assert_eq!(loaded.launch_id, journal.launch_id);
        assert!(loaded.pod_id.is_none());
        assert!(loaded.volume_id.is_none());
        assert_eq!(loaded.stage, JournalStage::Prepared);
        assert!(loaded.pod_name.starts_with("mintpod-01234567-"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn install_identity_is_stable() {
        let directory = test_dir();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("install-id");

        let first = load_or_create_install_id(&path).unwrap();
        let second = load_or_create_install_id(&path).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len(), 24);
        fs::remove_dir_all(directory).unwrap();
    }
}
