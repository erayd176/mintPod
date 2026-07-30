use std::{
    path::PathBuf,
    sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    harness::WiringReceipt,
    journal::{JournalError, load_or_create_install_id},
    lifecycle::{LaunchBudget, SessionTelemetry},
    orchestrator::RunningSession,
    presets::{PresetCatalog, PresetError},
    proxy::LocalGateway,
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
    pub runpod: RunpodClient,
    pub telemetry: Option<SessionTelemetry>,
}

#[derive(Clone)]
pub struct SessionSample {
    pub last_request_epoch_ms: u64,
}

pub enum RuntimeState {
    Idle,
    Launching(CancellationToken),
    Running(Box<ActiveSession>),
}

pub enum ExitAction {
    Exit,
    CancelLaunch,
    Stop(String),
}

pub struct AppState {
    presets: RwLock<PresetCatalog>,
    settings: RwLock<AppSettings>,
    runtime: Mutex<RuntimeState>,
    pub gateway: LocalGateway,
    pub user_presets_path: PathBuf,
    pub settings_path: PathBuf,
    pub credential_index_path: PathBuf,
    pub history_path: PathBuf,
    pub fx_rate_path: PathBuf,
    pub session_journal_path: PathBuf,
    pub install_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error(transparent)]
    Presets(#[from] PresetError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("a launch or session is already active")]
    AlreadyActive,
    #[error("a previous mintPod session needs recovery before launching again")]
    RecoveryRequired,
}

impl AppState {
    pub fn load(config_dir: PathBuf, gateway: LocalGateway) -> Result<Self, StateError> {
        let user_presets_path = config_dir.join("presets.user.json");
        let settings_path = config_dir.join("settings.json");
        let credential_index_path = config_dir.join("api-keys.json");
        let history_path = config_dir.join("session-history.json");
        let fx_rate_path = config_dir.join("fx-rate.json");
        let session_journal_path = config_dir.join("active-session.json");
        let install_id = load_or_create_install_id(&config_dir.join("install-id"))?;
        let presets = PresetCatalog::load(&user_presets_path)?;
        let settings = SettingsStore::load(&settings_path)?;
        Ok(Self {
            presets: RwLock::new(presets),
            settings: RwLock::new(settings),
            runtime: Mutex::new(RuntimeState::Idle),
            gateway,
            user_presets_path,
            settings_path,
            credential_index_path,
            history_path,
            fx_rate_path,
            session_journal_path,
            install_id,
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

    pub async fn begin_launch(&self) -> Result<CancellationToken, StateError> {
        let mut runtime = self.runtime.lock().await;
        if !matches!(*runtime, RuntimeState::Idle) {
            return Err(StateError::AlreadyActive);
        }
        if self.session_journal_path.exists() {
            return Err(StateError::RecoveryRequired);
        }
        let cancellation = CancellationToken::new();
        *runtime = RuntimeState::Launching(cancellation.clone());
        Ok(cancellation)
    }

    pub async fn begin_recovery(&self) -> Result<CancellationToken, StateError> {
        let mut runtime = self.runtime.lock().await;
        if !matches!(*runtime, RuntimeState::Idle) {
            return Err(StateError::AlreadyActive);
        }
        let cancellation = CancellationToken::new();
        *runtime = RuntimeState::Launching(cancellation.clone());
        Ok(cancellation)
    }

    pub async fn require_idle(&self) -> Result<(), StateError> {
        if matches!(*self.runtime.lock().await, RuntimeState::Idle) {
            Ok(())
        } else {
            Err(StateError::AlreadyActive)
        }
    }

    pub async fn finish_launch(&self, view: SessionView, runpod: RunpodClient) -> SessionView {
        *self.runtime.lock().await = RuntimeState::Running(Box::new(ActiveSession {
            view: view.clone(),
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
            last_request_epoch_ms: self.gateway.last_inference_epoch_ms(),
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

    pub async fn running_pod_id(&self) -> Option<String> {
        let runtime = self.runtime.lock().await;
        match &*runtime {
            RuntimeState::Running(active) => Some(active.view.session.pod_id.clone()),
            _ => None,
        }
    }

    pub async fn prepare_exit(&self) -> ExitAction {
        let runtime = self.runtime.lock().await;
        match &*runtime {
            RuntimeState::Idle => ExitAction::Exit,
            RuntimeState::Launching(cancellation) => {
                cancellation.cancel();
                ExitAction::CancelLaunch
            }
            RuntimeState::Running(active) => ExitAction::Stop(active.view.session.pod_id.clone()),
        }
    }

    pub async fn cancel_launch(&self) -> bool {
        let runtime = self.runtime.lock().await;
        let RuntimeState::Launching(cancellation) = &*runtime else {
            return false;
        };
        cancellation.cancel();
        true
    }
}
