use tauri::{AppHandle, Emitter, State};

use crate::{
    credentials::CredentialStore,
    harness::{HarnessAdapter, HarnessConnection, LOCAL_PROXY_URL, PiAdapter, WiringReceipt},
    orchestrator::{LaunchEvent, LaunchOrchestrator, LaunchStage, RunningSession},
    presets::PresetView,
    proxy::LocalProxy,
    runpod::RunpodClient,
    settings::{SettingsStore, SettingsView, VERIFIED_STORAGE_REGIONS},
    state::{AppState, SessionView},
};

#[tauri::command]
pub fn list_presets(state: State<'_, AppState>) -> Result<Vec<PresetView>, String> {
    state
        .presets()
        .map(|catalog| catalog.list())
        .map_err(|_| "preset catalog lock is poisoned".to_owned())
}

#[tauri::command]
pub fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let settings = state
        .settings()
        .map_err(|_| "settings lock is poisoned".to_owned())?
        .clone();
    Ok(SettingsView {
        settings,
        verified_storage_regions: VERIFIED_STORAGE_REGIONS,
    })
}

#[tauri::command]
pub fn set_storage_region(region: String, state: State<'_, AppState>) -> Result<(), String> {
    let mut settings = state
        .settings_mut()
        .map_err(|_| "settings lock is poisoned".to_owned())?;
    let mut updated = settings.clone();
    updated.storage_region = region;
    SettingsStore::save(&state.settings_path, &updated).map_err(|error| error.to_string())?;
    *settings = updated;
    Ok(())
}

#[tauri::command]
pub async fn api_key_status() -> Result<bool, String> {
    CredentialStore::contains_key().map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn save_api_key(api_key: String) -> Result<(), String> {
    let api_key = api_key.trim();
    let client = RunpodClient::new(api_key).map_err(|error| error.to_string())?;
    client
        .validate_key()
        .await
        .map_err(|error| error.to_string())?;
    CredentialStore::write_key(api_key).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn remove_api_key() -> Result<(), String> {
    CredentialStore::delete_key().map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn launch_preset(
    preset_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SessionView, String> {
    state
        .begin_launch()
        .await
        .map_err(|error| error.to_string())?;

    let result = launch_preset_inner(&preset_id, &app, &state).await;
    match result {
        Ok((session, wiring, proxy)) => {
            state
                .finish_launch(session.clone(), wiring.clone(), proxy)
                .await;
            Ok(SessionView { session, wiring })
        }
        Err(error) => {
            state.fail_launch().await;
            Err(error)
        }
    }
}

async fn launch_preset_inner(
    preset_id: &str,
    app: &AppHandle,
    state: &State<'_, AppState>,
) -> Result<(RunningSession, WiringReceipt, LocalProxy), String> {
    let preset = state
        .presets()
        .map_err(|_| "preset catalog lock is poisoned".to_owned())?
        .find(preset_id)
        .ok_or_else(|| format!("unknown preset: {preset_id}"))?;
    let region = state
        .settings()
        .map_err(|_| "settings lock is poisoned".to_owned())?
        .storage_region
        .clone();
    let api_key = CredentialStore::read_key()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "RunPod API key is not configured".to_owned())?;
    let runpod = RunpodClient::new(api_key).map_err(|error| error.to_string())?;

    app.emit(
        "launch-progress",
        LaunchEvent {
            stage: LaunchStage::RequestingPod,
            detail: "Preparing persistent model volume".to_owned(),
            completed_bytes: None,
            total_bytes: None,
            skipped: false,
        },
    )
    .map_err(|error| error.to_string())?;
    let volume = runpod
        .ensure_model_volume(&preset.id, preset.volume_size_gb(), &region)
        .await
        .map_err(|error| error.to_string())?;

    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_app = app.clone();
    let forward = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            let _ = event_app.emit("launch-progress", event);
        }
    });
    let session = LaunchOrchestrator::new(runpod.clone())
        .launch(preset.clone(), volume, events_tx)
        .await
        .map_err(|error| error.to_string())?;
    let _ = forward.await;

    let proxy = match LocalProxy::start(&session.remote_url).await {
        Ok(proxy) => proxy,
        Err(error) => {
            let _ = runpod.stop_pod(&session.pod_id).await;
            return Err(error.to_string());
        }
    };
    let adapter = match PiAdapter::system() {
        Ok(adapter) => adapter,
        Err(error) => {
            proxy.shutdown().await;
            let _ = runpod.stop_pod(&session.pod_id).await;
            return Err(error.to_string());
        }
    };
    let wiring = match adapter.wire(&HarnessConnection {
        url: LOCAL_PROXY_URL,
        api_key: proxy.token(),
        preset: &preset,
    }) {
        Ok(receipt) => receipt,
        Err(error) => {
            proxy.shutdown().await;
            let _ = runpod.stop_pod(&session.pod_id).await;
            return Err(error.to_string());
        }
    };
    let _ = app.emit(
        "launch-progress",
        LaunchEvent {
            stage: LaunchStage::Ready,
            detail: "Wired into Pi".to_owned(),
            completed_bytes: None,
            total_bytes: None,
            skipped: false,
        },
    );

    Ok((session, wiring, proxy))
}
