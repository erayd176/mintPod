use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::{
    credentials::CredentialStore,
    history,
    presets::PresetCatalog,
    proxy::LocalGateway,
    settings::SettingsStore,
    state::{AppState, StateError},
};

pub const USER_PRESETS_FILE: &str = "presets.user.json";
pub const SETTINGS_FILE: &str = "settings.json";
pub const CREDENTIAL_INDEX_FILE: &str = "api-keys.json";
pub const HISTORY_FILE: &str = "session-history.json";
const RESETTABLE_FILES: &[&str] = &[
    USER_PRESETS_FILE,
    SETTINGS_FILE,
    CREDENTIAL_INDEX_FILE,
    HISTORY_FILE,
];

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StartupFailureKind {
    ConfigDirectory,
    Keychain,
    LocalPort,
    UserPresets,
    Settings,
    CredentialIndex,
    SessionHistory,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupFailure {
    pub kind: StartupFailureKind,
    pub message: String,
    pub remedy: &'static str,
    pub resettable_file: Option<&'static str>,
}

impl StartupFailure {
    fn new(kind: StartupFailureKind, message: String) -> Self {
        let (remedy, resettable_file) = match kind {
            StartupFailureKind::ConfigDirectory => (
                "mintPod could not prepare its own configuration directory. Check that your user \
                 configuration directory exists and is writable, then retry.",
                None,
            ),
            StartupFailureKind::Keychain => (
                "mintPod keeps API keys and session tokens in the operating-system keychain. On \
                 Linux, start and unlock a Secret Service provider such as gnome-keyring or \
                 KWallet; on macOS and Windows, unlock the login keychain. Then retry.",
                None,
            ),
            StartupFailureKind::LocalPort => (
                "The stable local endpoint 127.0.0.1:11435 is already taken. Close any other \
                 running mintPod instance, or stop whatever owns the port, then retry.",
                None,
            ),
            StartupFailureKind::UserPresets => (
                "Your personal preset file could not be read. Reset it to drop your custom \
                 presets — the curated models are unaffected — or repair the file by hand and \
                 retry. A reset keeps the old file as presets.user.json.broken.",
                Some(USER_PRESETS_FILE),
            ),
            StartupFailureKind::Settings => (
                "Your settings file could not be read. Reset it to restore the defaults, or \
                 repair the file by hand and retry. A reset keeps the old file as \
                 settings.json.broken.",
                Some(SETTINGS_FILE),
            ),
            StartupFailureKind::CredentialIndex => (
                "The API-key profile index is damaged. Reset it to forget the profile list, then \
                 add your RunPod key again. The rejected file is preserved and secrets already in \
                 the operating-system keychain are not copied or deleted.",
                Some(CREDENTIAL_INDEX_FILE),
            ),
            StartupFailureKind::SessionHistory => (
                "The local session history is damaged. Reset it to start a new history. The \
                 rejected file is preserved as session-history.json.broken.",
                Some(HISTORY_FILE),
            ),
        };
        Self {
            kind,
            message,
            remedy,
            resettable_file,
        }
    }
}

impl From<StateError> for StartupFailure {
    fn from(error: StateError) -> Self {
        let kind = match &error {
            StateError::Presets(_) => StartupFailureKind::UserPresets,
            StateError::Settings(_) => StartupFailureKind::Settings,
            StateError::Journal(_) | StateError::AlreadyActive | StateError::RecoveryRequired => {
                StartupFailureKind::ConfigDirectory
            }
        };
        Self::new(kind, error.to_string())
    }
}

/// Always-managed startup outcome.
///
/// `AppState` is only managed once initialization succeeds, so a failure leaves
/// the window open on a blocked screen instead of taking the process down. The
/// user can fix the environment — or reset a local file — and retry in place.
pub struct StartupState {
    failure: Mutex<Option<StartupFailure>>,
}

impl StartupState {
    pub fn new() -> Self {
        Self {
            failure: Mutex::new(None),
        }
    }

    pub fn failure(&self) -> Option<StartupFailure> {
        self.failure.lock().ok().and_then(|failure| failure.clone())
    }

    fn record(&self, failure: Option<StartupFailure>) {
        if let Ok(mut slot) = self.failure.lock() {
            *slot = failure;
        }
    }
}

pub fn initialize(app: &AppHandle) -> Option<StartupFailure> {
    let failure = attempt(app).err();
    if let Some(startup) = app.try_state::<StartupState>() {
        startup.record(failure.clone());
    }
    failure
}

fn attempt(app: &AppHandle) -> Result<(), StartupFailure> {
    if app.try_state::<AppState>().is_some() {
        return Ok(());
    }

    let config_dir = prepare_config_dir(app)?;
    // Validate the stored documents before binding the local port. A broken
    // configuration then never consumes a listener that a retry would have to
    // rebind.
    PresetCatalog::load(&config_dir.join(USER_PRESETS_FILE))
        .map_err(|error| StartupFailure::new(StartupFailureKind::UserPresets, error.to_string()))?;
    SettingsStore::load(&config_dir.join(SETTINGS_FILE))
        .map_err(|error| StartupFailure::new(StartupFailureKind::Settings, error.to_string()))?;
    CredentialStore::validate_index_file(&config_dir.join(CREDENTIAL_INDEX_FILE)).map_err(
        |error| StartupFailure::new(StartupFailureKind::CredentialIndex, error.to_string()),
    )?;
    history::recent(&config_dir.join(HISTORY_FILE), 1).map_err(|error| {
        StartupFailure::new(StartupFailureKind::SessionHistory, error.to_string())
    })?;

    let gateway_token = CredentialStore::local_gateway_token()
        .map_err(|error| StartupFailure::new(StartupFailureKind::Keychain, error.to_string()))?;
    let gateway = tauri::async_runtime::block_on(LocalGateway::start(gateway_token))
        .map_err(|error| StartupFailure::new(StartupFailureKind::LocalPort, error.to_string()))?;

    app.manage(AppState::load(config_dir, gateway)?);
    Ok(())
}

fn prepare_config_dir(app: &AppHandle) -> Result<PathBuf, StartupFailure> {
    let config_dir = app.path().app_config_dir().map_err(|error| {
        StartupFailure::new(StartupFailureKind::ConfigDirectory, error.to_string())
    })?;
    crate::migrate_legacy_config(&config_dir)
        .and_then(|()| fs::create_dir_all(&config_dir))
        .map_err(|error| {
            StartupFailure::new(StartupFailureKind::ConfigDirectory, error.to_string())
        })?;
    Ok(config_dir)
}

/// Moves a rejected local document aside so the next start uses defaults.
///
/// The old contents are kept as `<file>.broken` rather than deleted; a
/// hand-edited preset list is user data even when mintPod cannot parse it.
pub fn reset_local_config(app: &AppHandle, file: &str) -> Result<(), String> {
    if !RESETTABLE_FILES.contains(&file) {
        return Err(format!("{file} is not a resettable mintPod file"));
    }
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    move_aside(&config_dir.join(file)).map_err(|error| error.to_string())
}

fn move_aside(path: &Path) -> Result<(), std::io::Error> {
    if !path.exists() {
        return Ok(());
    }
    fs::rename(path, available_backup_path(path))
}

fn available_backup_path(path: &Path) -> PathBuf {
    for suffix in 0_u32.. {
        let mut backup = path.as_os_str().to_owned();
        if suffix == 0 {
            backup.push(".broken");
        } else {
            backup.push(format!(".broken.{suffix}"));
        }
        let backup = PathBuf::from(backup);
        if !backup.exists() {
            return backup;
        }
    }
    unreachable!("the backup suffix space cannot be exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PresetError;

    #[test]
    fn broken_local_documents_are_reported_as_resettable() {
        let failure = StartupFailure::from(StateError::Presets(PresetError::UserFileNotArray));

        assert_eq!(failure.kind, StartupFailureKind::UserPresets);
        assert_eq!(failure.resettable_file, Some(USER_PRESETS_FILE));
    }

    #[test]
    fn environment_failures_are_not_resettable() {
        let failure = StartupFailure::new(StartupFailureKind::Keychain, "no keychain".to_owned());

        assert!(failure.resettable_file.is_none());
        assert!(failure.remedy.contains("keychain"));
    }

    #[test]
    fn only_known_documents_can_be_reset() {
        assert!(RESETTABLE_FILES.contains(&SETTINGS_FILE));
        assert!(RESETTABLE_FILES.contains(&CREDENTIAL_INDEX_FILE));
        assert!(RESETTABLE_FILES.contains(&HISTORY_FILE));
        assert!(!RESETTABLE_FILES.contains(&"active-session.json"));
        assert!(!RESETTABLE_FILES.contains(&"../settings.json"));
    }

    #[test]
    fn resetting_preserves_the_rejected_document() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("mintpod-startup-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(SETTINGS_FILE);
        fs::write(&path, "not json").unwrap();

        move_aside(&path).unwrap();

        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(directory.join("settings.json.broken")).unwrap(),
            "not json"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resetting_never_overwrites_an_earlier_backup() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("mintpod-backup-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(HISTORY_FILE);
        fs::write(&path, "new broken history").unwrap();
        fs::write(
            directory.join("session-history.json.broken"),
            "older backup",
        )
        .unwrap();

        move_aside(&path).unwrap();

        assert_eq!(
            fs::read_to_string(directory.join("session-history.json.broken")).unwrap(),
            "older backup"
        );
        assert_eq!(
            fs::read_to_string(directory.join("session-history.json.broken.1")).unwrap(),
            "new broken history"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
