use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const VERIFIED_STORAGE_REGIONS: &[&str] = &[
    "EU-RO-1", "EU-CZ-1", "EU-NL-1", "US-GA-2", "US-IL-1", "US-WA-1",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppSettings {
    pub storage_region: String,
    pub idle_timeout_minutes: u16,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            storage_region: "EU-RO-1".to_owned(),
            idle_timeout_minutes: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    #[serde(flatten)]
    pub settings: AppSettings,
    pub verified_storage_regions: &'static [&'static str],
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("could not read {path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("settings file {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("could not write {path}: {message}")]
    Write { path: PathBuf, message: String },
    #[error("unsupported storage region: {0}")]
    UnsupportedRegion(String),
}

pub struct SettingsStore;

impl SettingsStore {
    pub fn load(path: &Path) -> Result<AppSettings, SettingsError> {
        if !path.exists() {
            let settings = AppSettings::default();
            Self::save(path, &settings)?;
            return Ok(settings);
        }

        let contents = fs::read_to_string(path).map_err(|error| SettingsError::Read {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        let settings: AppSettings =
            serde_json::from_str(&contents).map_err(|error| SettingsError::Invalid {
                path: path.to_owned(),
                message: error.to_string(),
            })?;
        validate(&settings)?;
        Ok(settings)
    }

    pub fn save(path: &Path, settings: &AppSettings) -> Result<(), SettingsError> {
        validate(settings)?;
        let contents =
            serde_json::to_string_pretty(settings).map_err(|error| SettingsError::Write {
                path: path.to_owned(),
                message: error.to_string(),
            })?;
        let mut file = AtomicWriteFile::open(path).map_err(|error| SettingsError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
        file.write_all(format!("{contents}\n").as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|error| SettingsError::Write {
                path: path.to_owned(),
                message: error.to_string(),
            })?;
        file.commit().map_err(|error| SettingsError::Write {
            path: path.to_owned(),
            message: error.to_string(),
        })
    }
}

fn validate(settings: &AppSettings) -> Result<(), SettingsError> {
    if !VERIFIED_STORAGE_REGIONS.contains(&settings.storage_region.as_str()) {
        return Err(SettingsError::UnsupportedRegion(
            settings.storage_region.clone(),
        ));
    }
    if !(1..=240).contains(&settings.idle_timeout_minutes) {
        return Err(SettingsError::Invalid {
            path: PathBuf::from("settings.json"),
            message: "idleTimeoutMinutes must be between 1 and 240".to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_regions() {
        let error = validate(&AppSettings {
            storage_region: "MARS-1".to_owned(),
            idle_timeout_minutes: 10,
        })
        .unwrap_err();

        assert!(error.to_string().contains("MARS-1"));
    }
}
