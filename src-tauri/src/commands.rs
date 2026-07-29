use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::{
    credentials::CredentialStore,
    harness::{HarnessAdapter, HarnessConnection, LOCAL_PROXY_URL, PiAdapter, WiringReceipt},
    orchestrator::{LaunchEvent, LaunchOrchestrator, LaunchStage, RunningSession},
    presets::{GPU_TIERS, GpuTierView, Preset, PresetView, verified_gpu_tier},
    proxy::LocalProxy,
    runpod::RunpodClient,
    settings::{SettingsStore, SettingsView, VERIFIED_STORAGE_REGIONS},
    state::{AppState, SessionView},
};

const HOBBY_RANGE_WARNING: &str = "outside default hobby range, continue anyway?";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CustomPresetInput {
    ollama_tag: String,
    size_gb: f64,
    min_vram_gb: u16,
    gpu_type_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddPresetResult {
    requires_confirmation: bool,
    warning: Option<&'static str>,
    preset: Option<PresetView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedModel {
    volume_id: String,
    preset_id: String,
    label: String,
    ollama_tag: String,
    model_size_gb: f64,
    allocated_gb: u16,
    data_center_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheSummary {
    models: Vec<CachedModel>,
    total_allocated_gb: u32,
}

#[tauri::command]
pub fn list_presets(state: State<'_, AppState>) -> Result<Vec<PresetView>, String> {
    state
        .presets()
        .map(|catalog| catalog.list())
        .map_err(|_| "preset catalog lock is poisoned".to_owned())
}

#[tauri::command]
pub fn list_gpu_tiers() -> Vec<GpuTierView> {
    GPU_TIERS.to_vec()
}

#[tauri::command]
pub async fn add_custom_preset(
    input: CustomPresetInput,
    confirm_outside_range: bool,
    state: State<'_, AppState>,
) -> Result<AddPresetResult, String> {
    let outside_range = input.size_gb > 16.0 || verified_gpu_tier(&input.gpu_type_ids).is_none();
    if outside_range && !confirm_outside_range {
        return Ok(AddPresetResult {
            requires_confirmation: true,
            warning: Some(HOBBY_RANGE_WARNING),
            preset: None,
        });
    }
    let estimated_cost = verified_gpu_tier(&input.gpu_type_ids)
        .map(|tier| tier.est_cost_per_hr)
        .unwrap_or(0.44);
    let preset = Preset {
        id: String::new(),
        label: input.ollama_tag.clone(),
        ollama_tag: input.ollama_tag.trim().to_owned(),
        size_gb: input.size_gb,
        min_vram_gb: input.min_vram_gb,
        gpu_type_ids: input.gpu_type_ids,
        est_cost_per_hr: estimated_cost,
        tags: vec!["coding".to_owned(), "custom".to_owned()],
    };
    let mut catalog = state
        .presets_mut()
        .map_err(|_| "preset catalog lock is poisoned".to_owned())?;
    let created = catalog
        .add_user_preset(&state.user_presets_path, preset)
        .map_err(|error| error.to_string())?;
    Ok(AddPresetResult {
        requires_confirmation: false,
        warning: None,
        preset: Some(created),
    })
}

#[tauri::command]
pub async fn list_cached_models(state: State<'_, AppState>) -> Result<CacheSummary, String> {
    let api_key = CredentialStore::read_key()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "RunPod API key is not configured".to_owned())?;
    let volumes = RunpodClient::new(api_key)
        .map_err(|error| error.to_string())?
        .list_network_volumes()
        .await
        .map_err(|error| error.to_string())?;
    let catalog = state
        .presets()
        .map_err(|_| "preset catalog lock is poisoned".to_owned())?;
    let mut models = volumes
        .into_iter()
        .filter_map(|volume| {
            let preset_id = volume.name.strip_prefix("podpilot-")?.to_owned();
            let preset = catalog.find(&preset_id);
            Some(CachedModel {
                volume_id: volume.id,
                preset_id: preset_id.clone(),
                label: preset
                    .as_ref()
                    .map(|preset| preset.label.clone())
                    .unwrap_or_else(|| preset_id.clone()),
                ollama_tag: preset
                    .as_ref()
                    .map(|preset| preset.ollama_tag.clone())
                    .unwrap_or_else(|| "unknown model".to_owned()),
                model_size_gb: preset
                    .as_ref()
                    .map(|preset| preset.size_gb)
                    .unwrap_or(volume.size as f64),
                allocated_gb: volume.size,
                data_center_id: volume.data_center_id,
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.label.cmp(&right.label));
    let total_allocated_gb = models
        .iter()
        .map(|model| u32::from(model.allocated_gb))
        .sum();
    Ok(CacheSummary {
        models,
        total_allocated_gb,
    })
}

#[tauri::command]
pub async fn delete_cached_model(
    volume_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state
        .require_idle()
        .await
        .map_err(|error| error.to_string())?;
    let api_key = CredentialStore::read_key()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "RunPod API key is not configured".to_owned())?;
    let runpod = RunpodClient::new(api_key).map_err(|error| error.to_string())?;
    let volume = runpod
        .list_network_volumes()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|volume| volume.id == volume_id && volume.name.starts_with("podpilot-"))
        .ok_or_else(|| "cached model volume was not found".to_owned())?;
    runpod
        .delete_network_volume(&volume.id)
        .await
        .map_err(|error| error.to_string())
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
