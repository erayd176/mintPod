use std::{
    path::PathBuf,
    sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use tokio::sync::Mutex;

use crate::{
    harness::WiringReceipt,
    lifecycle::{LaunchBudget, SessionTelemetry},
    orchestrator::RunningSession,
    presets::{PresetCatalog, PresetError},
    proxy::LocalProxy,
    runpod::RunpodClient,
    settings::{AppSettings, SettingsError, SettingsStore},
};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    #[serde(flatten)]
    pub session: RunningSession,
    pub wiring: WiringReceipt,
    pub budget: LaunchBudget,
    pub idle_timeout_minutes: u16,
    pub cost_per_hr_eur: f64,
}

pub struct ActiveSession {
    pub view: SessionView,
    pub proxy: LocalProxy,
    pub runpod: RunpodClient,
    pub telemetry: Option<SessionTelemetry>,
}

#[derive(Clone)]
pub struct GraceSession {
    pub view: SessionView,
    pub runpod: RunpodClient,
    pub stopped_at_epoch_ms: u64,
}

#[derive(Clone)]
pub struct SessionSample {
    pub last_request_epoch_ms: u64,
}

pub enum RuntimeState {
    Idle,
    Launching,
    Running(Box<ActiveSession>),
    Grace(Box<GraceSession>),
}

pub enum ExitAction {
    Exit,
    WaitForLaunch,
    Stop(String),
    Terminate(Box<GraceSession>),
}

pub struct AppState {
    presets: RwLock<PresetCatalog>,
    settings: RwLock<AppSettings>,
    runtime: Mutex<RuntimeState>,
    pub user_presets_path: PathBuf,
    pub settings_path: PathBuf,
    pub credential_index_path: PathBuf,
    pub history_path: PathBuf,
    pub fx_rate_path: PathBuf,
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
        let credential_index_path = config_dir.join("api-keys.json");
        let history_path = config_dir.join("session-history.json");
        let fx_rate_path = config_dir.join("fx-rate.json");
        let presets = PresetCatalog::load(&user_presets_path)?;
        let settings = SettingsStore::load(&settings_path)?;
        Ok(Self {
            presets: RwLock::new(presets),
            settings: RwLock::new(settings),
            runtime: Mutex::new(RuntimeState::Idle),
            user_presets_path,
            settings_path,
            credential_index_path,
            history_path,
            fx_rate_path,
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

    pub async fn begin_launch(&self) -> Result<Option<GraceSession>, StateError> {
        let mut runtime = self.runtime.lock().await;
        let previous = std::mem::replace(&mut *runtime, RuntimeState::Launching);
        match previous {
            RuntimeState::Idle => Ok(None),
            RuntimeState::Grace(grace) => Ok(Some(*grace)),
            active => {
                *runtime = active;
                Err(StateError::AlreadyActive)
            }
        }
    }

    pub async fn require_idle(&self) -> Result<(), StateError> {
        if matches!(*self.runtime.lock().await, RuntimeState::Idle) {
            Ok(())
        } else {
            Err(StateError::AlreadyActive)
        }
    }

    pub async fn begin_credential_change(&self) -> Result<Option<GraceSession>, StateError> {
        let mut runtime = self.runtime.lock().await;
        let previous = std::mem::replace(&mut *runtime, RuntimeState::Idle);
        match previous {
            RuntimeState::Idle => Ok(None),
            RuntimeState::Grace(grace) => Ok(Some(*grace)),
            active => {
                *runtime = active;
                Err(StateError::AlreadyActive)
            }
        }
    }

    pub async fn finish_launch(
        &self,
        view: SessionView,
        proxy: LocalProxy,
        runpod: RunpodClient,
    ) -> SessionView {
        *self.runtime.lock().await = RuntimeState::Running(Box::new(ActiveSession {
            view: view.clone(),
            proxy,
            runpod,
            telemetry: None,
        }));
        view
    }

    pub async fn fail_launch(&self) {
        *self.runtime.lock().await = RuntimeState::Idle;
    }

    pub async fn session_sample(&self, pod_id: &str) -> Option<SessionSample> {
        let runtime = self.runtime.lock().await;
        let RuntimeState::Running(active) = &*runtime else {
            return None;
        };
        (active.view.session.pod_id == pod_id).then(|| SessionSample {
            last_request_epoch_ms: active.proxy.last_request_epoch_ms(),
        })
    }

    pub async fn update_cost_per_hr(&self, pod_id: &str, cost_per_hr_eur: f64) {
        let mut runtime = self.runtime.lock().await;
        if let RuntimeState::Running(active) = &mut *runtime
            && active.view.session.pod_id == pod_id
        {
            active.view.cost_per_hr_eur = cost_per_hr_eur;
        }
    }

    pub async fn update_telemetry(&self, pod_id: &str, telemetry: SessionTelemetry) {
        let mut runtime = self.runtime.lock().await;
        if let RuntimeState::Running(active) = &mut *runtime
            && active.view.session.pod_id == pod_id
        {
            active.telemetry = Some(telemetry);
        }
    }

    pub async fn take_running(&self, pod_id: &str) -> Option<ActiveSession> {
        let mut runtime = self.runtime.lock().await;
        let previous = std::mem::replace(&mut *runtime, RuntimeState::Idle);
        match previous {
            RuntimeState::Running(active) if active.view.session.pod_id == pod_id => Some(*active),
            other => {
                *runtime = other;
                None
            }
        }
    }

    pub async fn restore_running(&self, active: ActiveSession) {
        *self.runtime.lock().await = RuntimeState::Running(Box::new(active));
    }

    pub async fn set_grace(&self, grace: GraceSession) {
        *self.runtime.lock().await = RuntimeState::Grace(Box::new(grace));
    }

    pub async fn take_grace(&self, pod_id: &str) -> Option<GraceSession> {
        let mut runtime = self.runtime.lock().await;
        let previous = std::mem::replace(&mut *runtime, RuntimeState::Idle);
        match previous {
            RuntimeState::Grace(grace) if grace.view.session.pod_id == pod_id => Some(*grace),
            other => {
                *runtime = other;
                None
            }
        }
    }

    pub async fn running_pod_id(&self) -> Option<String> {
        let runtime = self.runtime.lock().await;
        match &*runtime {
            RuntimeState::Running(active) => Some(active.view.session.pod_id.clone()),
            _ => None,
        }
    }

    pub async fn prepare_exit(&self) -> ExitAction {
        let mut runtime = self.runtime.lock().await;
        match &*runtime {
            RuntimeState::Idle => ExitAction::Exit,
            RuntimeState::Launching => ExitAction::WaitForLaunch,
            RuntimeState::Running(active) => ExitAction::Stop(active.view.session.pod_id.clone()),
            RuntimeState::Grace(_) => {
                let previous = std::mem::replace(&mut *runtime, RuntimeState::Idle);
                let RuntimeState::Grace(grace) = previous else {
                    unreachable!("runtime state was checked while locked")
                };
                ExitAction::Terminate(grace)
            }
        }
    }
}
