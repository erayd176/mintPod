use std::{
    path::PathBuf,
    sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use tokio::sync::Mutex;

use crate::{
    harness::WiringReceipt,
    orchestrator::RunningSession,
    presets::{PresetCatalog, PresetError},
    proxy::LocalProxy,
    settings::{AppSettings, SettingsError, SettingsStore},
};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    #[serde(flatten)]
    pub session: RunningSession,
    pub wiring: WiringReceipt,
}

pub struct ActiveSession {
    pub view: SessionView,
    pub proxy: LocalProxy,
}

pub enum RuntimeState {
    Idle,
    Launching,
    Running(Box<ActiveSession>),
}

pub struct AppState {
    presets: RwLock<PresetCatalog>,
    settings: RwLock<AppSettings>,
    runtime: Mutex<RuntimeState>,
    pub user_presets_path: PathBuf,
    pub settings_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Presets(#[from] PresetError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error("a launch or session is already active")]
    AlreadyActive,
}

impl AppState {
    pub fn load(config_dir: PathBuf) -> Result<Self, StateError> {
        let user_presets_path = config_dir.join("presets.user.json");
        let settings_path = config_dir.join("settings.json");
        let presets = PresetCatalog::load(&user_presets_path)?;
        let settings = SettingsStore::load(&settings_path)?;
        Ok(Self {
            presets: RwLock::new(presets),
            settings: RwLock::new(settings),
            runtime: Mutex::new(RuntimeState::Idle),
            user_presets_path,
            settings_path,
        })
    }

    pub fn presets(
        &self,
    ) -> Result<RwLockReadGuard<'_, PresetCatalog>, PoisonError<RwLockReadGuard<'_, PresetCatalog>>>
    {
        self.presets.read()
    }

    pub fn presets_mut(
        &self,
    ) -> Result<RwLockWriteGuard<'_, PresetCatalog>, PoisonError<RwLockWriteGuard<'_, PresetCatalog>>>
    {
        self.presets.write()
    }

    pub fn settings(
        &self,
    ) -> Result<RwLockReadGuard<'_, AppSettings>, PoisonError<RwLockReadGuard<'_, AppSettings>>>
    {
        self.settings.read()
    }

    pub fn settings_mut(
        &self,
    ) -> Result<RwLockWriteGuard<'_, AppSettings>, PoisonError<RwLockWriteGuard<'_, AppSettings>>>
    {
        self.settings.write()
    }

    pub async fn begin_launch(&self) -> Result<(), StateError> {
        let mut runtime = self.runtime.lock().await;
        if !matches!(*runtime, RuntimeState::Idle) {
            return Err(StateError::AlreadyActive);
        }
        *runtime = RuntimeState::Launching;
        Ok(())
    }

    pub async fn require_idle(&self) -> Result<(), StateError> {
        if matches!(*self.runtime.lock().await, RuntimeState::Idle) {
            Ok(())
        } else {
            Err(StateError::AlreadyActive)
        }
    }

    pub async fn finish_launch(
        &self,
        session: RunningSession,
        wiring: WiringReceipt,
        proxy: LocalProxy,
    ) {
        *self.runtime.lock().await = RuntimeState::Running(Box::new(ActiveSession {
            view: SessionView { session, wiring },
            proxy,
        }));
    }

    pub async fn fail_launch(&self) {
        *self.runtime.lock().await = RuntimeState::Idle;
    }
}
