use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use keyring::v1::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const SERVICE: &str = "dev.mintpod";
const LEGACY_SERVICE: &str = "dev.podpilot.desktop";
const LEGACY_ID: &str = "legacy";
const LEGACY_USER: &str = "runpod-api-key";
const USER_PREFIX: &str = "runpod-api-key-";
const RUNTIME_USER_PREFIX: &str = "runtime-token-";
const MAX_LABEL_CHARS: usize = 32;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialProfile {
    pub id: String,
    pub label: String,
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredProfile {
    id: String,
    label: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialIndex {
    active_id: Option<String>,
    profiles: Vec<StoredProfile>,
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("the operating-system keychain is unavailable: {0}")]
    Keychain(String),
    #[error("API key label must contain 1 to {MAX_LABEL_CHARS} characters")]
    InvalidLabel,
    #[error("an API key profile named '{0}' already exists")]
    DuplicateLabel(String),
    #[error("API key profile '{0}' does not exist")]
    MissingProfile(String),
    #[error("the API key is empty")]
    EmptyKey,
    #[error("could not read {path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("credential index {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("could not write {path}: {message}")]
    Write { path: PathBuf, message: String },
    #[error("could not generate a credential identifier: {0}")]
    Random(String),
}

pub struct CredentialStore;

impl CredentialStore {
    pub fn list_profiles(path: &Path) -> Result<Vec<CredentialProfile>, CredentialError> {
        let index = load_with_legacy(path)?;
        Ok(profile_views(&index))
    }

    pub fn read_active_key(path: &Path) -> Result<Option<String>, CredentialError> {
        let index = load_with_legacy(path)?;
        let Some(active_id) = index.active_id else {
            return Ok(None);
        };
        read_entry(&active_id).map(Some)
    }

    pub fn read_active(
        path: &Path,
    ) -> Result<Option<(CredentialProfile, String)>, CredentialError> {
        let index = load_with_legacy(path)?;
        let Some(active_id) = index.active_id.as_deref() else {
            return Ok(None);
        };
        let profile = require_profile(&index, active_id)?;
        Ok(Some((
            CredentialProfile {
                id: profile.id.clone(),
                label: profile.label.clone(),
                active: true,
            },
            read_entry(active_id)?,
        )))
    }

    pub fn read_profile_key(path: &Path, profile_id: &str) -> Result<String, CredentialError> {
        let index = load_with_legacy(path)?;
        require_profile(&index, profile_id)?;
        read_entry(profile_id)
    }

    pub fn store_runtime_token(launch_id: &str, token: &str) -> Result<(), CredentialError> {
        runtime_entry(launch_id)?
            .set_password(normalize_key(token)?)
            .map_err(keychain_error)
    }

    pub fn delete_runtime_token(launch_id: &str) -> Result<(), CredentialError> {
        match runtime_entry(launch_id)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => Err(keychain_error(error)),
        }
    }

    pub fn add_profile(
        path: &Path,
        label: &str,
        key: &str,
    ) -> Result<CredentialProfile, CredentialError> {
        let label = normalize_label(label)?;
        let key = normalize_key(key)?;
        let mut index = load_with_legacy(path)?;
        if index
            .profiles
            .iter()
            .any(|profile| profile.label.eq_ignore_ascii_case(&label))
        {
            return Err(CredentialError::DuplicateLabel(label));
        }

        let id = generate_id(&index)?;
        entry(&id)?.set_password(key).map_err(keychain_error)?;
        index.profiles.push(StoredProfile {
            id: id.clone(),
            label: label.clone(),
        });
        index.active_id = Some(id.clone());
        if let Err(error) = write_index(path, &index) {
            let _ = delete_entry(&id);
            return Err(error);
        }

        Ok(CredentialProfile {
            id,
            label,
            active: true,
        })
    }

    pub fn replace_profile(
        path: &Path,
        profile_id: &str,
        key: &str,
    ) -> Result<(), CredentialError> {
        let key = normalize_key(key)?;
        let index = load_with_legacy(path)?;
        require_profile(&index, profile_id)?;
        entry(profile_id)?.set_password(key).map_err(keychain_error)
    }

    pub fn select_profile(path: &Path, profile_id: &str) -> Result<(), CredentialError> {
        let mut index = load_with_legacy(path)?;
        require_profile(&index, profile_id)?;
        read_entry(profile_id)?;
        index.active_id = Some(profile_id.to_owned());
        write_index(path, &index)
    }

    pub fn delete_profile(path: &Path, profile_id: &str) -> Result<(), CredentialError> {
        let original = load_with_legacy(path)?;
        require_profile(&original, profile_id)?;
        let mut updated = original.clone();
        updated.profiles.retain(|profile| profile.id != profile_id);
        if updated.active_id.as_deref() == Some(profile_id) {
            updated.active_id = updated.profiles.first().map(|profile| profile.id.clone());
        }

        write_index(path, &updated)?;
        if let Err(error) = delete_entry(profile_id) {
            let _ = write_index(path, &original);
            return Err(error);
        }
        Ok(())
    }
}

fn load_with_legacy(path: &Path) -> Result<CredentialIndex, CredentialError> {
    let mut index = read_index(path)?;
    if index.profiles.is_empty() && entry_exists(LEGACY_ID)? {
        index.profiles.push(StoredProfile {
            id: LEGACY_ID.to_owned(),
            label: "Default".to_owned(),
        });
        index.active_id = Some(LEGACY_ID.to_owned());
    }
    Ok(index)
}

fn read_index(path: &Path) -> Result<CredentialIndex, CredentialError> {
    if !path.exists() {
        return Ok(CredentialIndex::default());
    }
    let contents = fs::read_to_string(path).map_err(|error| CredentialError::Read {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    let index = serde_json::from_str(&contents).map_err(|error| CredentialError::Invalid {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    validate_index(&index).map_err(|message| CredentialError::Invalid {
        path: path.to_owned(),
        message,
    })?;
    Ok(index)
}

fn write_index(path: &Path, index: &CredentialIndex) -> Result<(), CredentialError> {
    validate_index(index).map_err(|message| CredentialError::Invalid {
        path: path.to_owned(),
        message,
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| CredentialError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    }
    let mut contents =
        serde_json::to_vec_pretty(index).expect("a credential index always serializes");
    contents.push(b'\n');
    let mut file = AtomicWriteFile::open(path).map_err(|error| CredentialError::Write {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    secure_permissions(&file).map_err(|error| CredentialError::Write {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    file.write_all(&contents)
        .and_then(|_| file.sync_all())
        .map_err(|error| CredentialError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    file.commit().map_err(|error| CredentialError::Write {
        path: path.to_owned(),
        message: error.to_string(),
    })
}

fn validate_index(index: &CredentialIndex) -> Result<(), String> {
    if index.profiles.is_empty() != index.active_id.is_none() {
        return Err("activeId must be empty exactly when profiles is empty".to_owned());
    }
    let mut ids = HashSet::with_capacity(index.profiles.len());
    let mut labels = HashSet::with_capacity(index.profiles.len());
    for profile in &index.profiles {
        if !valid_id(&profile.id) {
            return Err(format!("invalid profile id '{}'", profile.id));
        }
        if !ids.insert(profile.id.as_str()) {
            return Err(format!("duplicate profile id '{}'", profile.id));
        }
        normalize_label(&profile.label).map_err(|error| error.to_string())?;
        if !labels.insert(profile.label.to_ascii_lowercase()) {
            return Err(format!("duplicate profile label '{}'", profile.label));
        }
    }
    if let Some(active_id) = &index.active_id
        && !ids.contains(active_id.as_str())
    {
        return Err("activeId does not reference a profile".to_owned());
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    id == LEGACY_ID || (id.len() == 24 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn normalize_label(label: &str) -> Result<String, CredentialError> {
    let label = label.trim();
    if label.is_empty()
        || label.chars().count() > MAX_LABEL_CHARS
        || label.chars().any(char::is_control)
    {
        return Err(CredentialError::InvalidLabel);
    }
    Ok(label.to_owned())
}

fn normalize_key(key: &str) -> Result<&str, CredentialError> {
    let key = key.trim();
    if key.is_empty() {
        Err(CredentialError::EmptyKey)
    } else {
        Ok(key)
    }
}

fn require_profile<'a>(
    index: &'a CredentialIndex,
    profile_id: &str,
) -> Result<&'a StoredProfile, CredentialError> {
    index
        .profiles
        .iter()
        .find(|profile| profile.id == profile_id)
        .ok_or_else(|| CredentialError::MissingProfile(profile_id.to_owned()))
}

fn profile_views(index: &CredentialIndex) -> Vec<CredentialProfile> {
    index
        .profiles
        .iter()
        .map(|profile| CredentialProfile {
            id: profile.id.clone(),
            label: profile.label.clone(),
            active: index.active_id.as_deref() == Some(profile.id.as_str()),
        })
        .collect()
}

fn generate_id(index: &CredentialIndex) -> Result<String, CredentialError> {
    loop {
        let mut bytes = [0_u8; 12];
        getrandom::fill(&mut bytes).map_err(|error| CredentialError::Random(error.to_string()))?;
        let id = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !index.profiles.iter().any(|profile| profile.id == id) {
            return Ok(id);
        }
    }
}

fn entry_exists(profile_id: &str) -> Result<bool, CredentialError> {
    match entry(profile_id)?.get_password() {
        Ok(_) => Ok(true),
        Err(KeyringError::NoEntry) => match legacy_entry(profile_id)?.get_password() {
            Ok(key) => {
                entry(profile_id)?
                    .set_password(&key)
                    .map_err(keychain_error)?;
                Ok(true)
            }
            Err(KeyringError::NoEntry) => Ok(false),
            Err(error) => Err(keychain_error(error)),
        },
        Err(error) => Err(keychain_error(error)),
    }
}

fn read_entry(profile_id: &str) -> Result<String, CredentialError> {
    match entry(profile_id)?.get_password() {
        Ok(key) => Ok(key),
        Err(KeyringError::NoEntry) => match legacy_entry(profile_id)?.get_password() {
            Ok(key) => {
                entry(profile_id)?
                    .set_password(&key)
                    .map_err(keychain_error)?;
                Ok(key)
            }
            Err(KeyringError::NoEntry) => {
                Err(CredentialError::MissingProfile(profile_id.to_owned()))
            }
            Err(error) => Err(keychain_error(error)),
        },
        Err(error) => Err(keychain_error(error)),
    }
}

fn delete_entry(profile_id: &str) -> Result<(), CredentialError> {
    for candidate in [entry(profile_id)?, legacy_entry(profile_id)?] {
        match candidate.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => {}
            Err(error) => return Err(keychain_error(error)),
        }
    }
    Ok(())
}

fn entry(profile_id: &str) -> Result<Entry, CredentialError> {
    service_entry(SERVICE, profile_id)
}

fn legacy_entry(profile_id: &str) -> Result<Entry, CredentialError> {
    service_entry(LEGACY_SERVICE, profile_id)
}

fn runtime_entry(launch_id: &str) -> Result<Entry, CredentialError> {
    Entry::new(SERVICE, &format!("{RUNTIME_USER_PREFIX}{launch_id}")).map_err(keychain_error)
}

fn service_entry(service: &str, profile_id: &str) -> Result<Entry, CredentialError> {
    let user = if profile_id == LEGACY_ID {
        LEGACY_USER.to_owned()
    } else {
        format!("{USER_PREFIX}{profile_id}")
    };
    Entry::new(service, &user).map_err(keychain_error)
}

fn keychain_error(error: KeyringError) -> CredentialError {
    CredentialError::Keychain(error.to_string())
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

    #[test]
    fn index_requires_active_profile_to_exist() {
        let index = CredentialIndex {
            active_id: Some("0123456789abcdef01234567".to_owned()),
            profiles: Vec::new(),
        };

        assert!(validate_index(&index).is_err());
    }

    #[test]
    fn index_rejects_case_insensitive_duplicate_labels() {
        let index = CredentialIndex {
            active_id: Some("0123456789abcdef01234567".to_owned()),
            profiles: vec![
                StoredProfile {
                    id: "0123456789abcdef01234567".to_owned(),
                    label: "Personal".to_owned(),
                },
                StoredProfile {
                    id: "abcdef0123456789abcdef01".to_owned(),
                    label: "personal".to_owned(),
                },
            ],
        };

        assert!(validate_index(&index).is_err());
    }

    #[test]
    fn profile_views_mark_only_the_active_profile() {
        let index = CredentialIndex {
            active_id: Some("abcdef0123456789abcdef01".to_owned()),
            profiles: vec![
                StoredProfile {
                    id: "0123456789abcdef01234567".to_owned(),
                    label: "Personal".to_owned(),
                },
                StoredProfile {
                    id: "abcdef0123456789abcdef01".to_owned(),
                    label: "Project".to_owned(),
                },
            ],
        };

        let profiles = profile_views(&index);
        assert!(!profiles[0].active);
        assert!(profiles[1].active);
    }
}
