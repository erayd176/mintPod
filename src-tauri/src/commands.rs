use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{
    credentials::CredentialStore,
    fx,
    harness::{HarnessAdapter, HarnessConnection, LOCAL_PROXY_URL, PiAdapter, WiringReceipt},
    history::{self, SessionHistoryEntry},
    lifecycle::{BudgetTracker, LaunchBudget, SessionTelemetry, StopReason},
    orchestrator::{LaunchEvent, LaunchOrchestrator, LaunchStage, RunningSession},
    presets::{GPU_TIERS, GpuTierView, Preset, PresetView, verified_gpu_tier},
    proxy::LocalProxy,
    runpod::RunpodClient,
    settings::{SettingsStore, SettingsView, VERIFIED_STORAGE_REGIONS},
    state::{ActiveSession, AppState, ExitAction, GraceSession, SessionView},
};

const HOBBY_RANGE_WARNING: &str = "outside default hobby range, continue anyway?";
const TERMINATION_GRACE: Duration = Duration::from_secs(5 * 60);
const COST_RESYNC_INTERVAL_SECONDS: u64 = 30;

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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStoppedEvent {
    reason: StopReason,
    history_error: Option<String>,
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
pub fn session_history(state: State<'_, AppState>) -> Result<Vec<SessionHistoryEntry>, String> {
    history::recent(&state.history_path, 5).map_err(|error| error.to_string())
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
pub fn set_idle_timeout(minutes: u16, state: State<'_, AppState>) -> Result<(), String> {
    let mut settings = state
        .settings_mut()
        .map_err(|_| "settings lock is poisoned".to_owned())?;
    let mut updated = settings.clone();
    updated.idle_timeout_minutes = minutes;
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
    budget: LaunchBudget,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SessionView, String> {
    let budget = budget.validate().map_err(|error| error.to_string())?;
    let previous = state
        .begin_launch()
        .await
        .map_err(|error| error.to_string())?;
    let idle_timeout_minutes = state
        .settings()
        .map_err(|_| "settings lock is poisoned".to_owned())?
        .idle_timeout_minutes;
    let result = launch_preset_inner(&preset_id, previous, &app, &state).await;
    match result {
        Ok((session, wiring, proxy, runpod, usd_to_eur)) => {
            let cost_per_hr_eur = session.cost_per_hr_usd * usd_to_eur;
            let view = state
                .finish_launch(
                    session,
                    wiring,
                    budget,
                    idle_timeout_minutes,
                    cost_per_hr_eur,
                    proxy,
                    runpod.clone(),
                )
                .await;
            spawn_session_monitor(app, view.clone(), runpod, usd_to_eur);
            Ok(view)
        }
        Err(error) => {
            state.fail_launch().await;
            Err(error)
        }
    }
}

async fn launch_preset_inner(
    preset_id: &str,
    previous: Option<GraceSession>,
    app: &AppHandle,
    state: &State<'_, AppState>,
) -> Result<(RunningSession, WiringReceipt, LocalProxy, RunpodClient, f64), String> {
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
    let fx_path = state.fx_rate_path.clone();
    let fx_task = tokio::spawn(async move { fx::usd_to_eur(&fx_path).await });

    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
    let event_app = app.clone();
    let forward = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            let _ = event_app.emit("launch-progress", event);
        }
    });
    let orchestrator = LaunchOrchestrator::new(runpod.clone());
    let session = match previous {
        Some(grace)
            if grace.view.session.preset_id == preset.id
                && now_epoch_ms().saturating_sub(grace.stopped_at_epoch_ms)
                    < TERMINATION_GRACE.as_millis() as u64 =>
        {
            orchestrator
                .resume(grace.view.session.pod_id, preset.clone(), events_tx)
                .await
                .map_err(|error| error.to_string())?
        }
        Some(grace) => {
            terminate_with_retry(&grace.runpod, &grace.view.session.pod_id).await?;
            emit_stage(
                app,
                LaunchStage::RequestingPod,
                "Preparing persistent model volume",
            )?;
            let volume = runpod
                .ensure_model_volume(&preset.id, preset.volume_size_gb(), &region)
                .await
                .map_err(|error| error.to_string())?;
            orchestrator
                .launch(preset.clone(), volume, events_tx)
                .await
                .map_err(|error| error.to_string())?
        }
        None => {
            emit_stage(
                app,
                LaunchStage::RequestingPod,
                "Preparing persistent model volume",
            )?;
            let volume = runpod
                .ensure_model_volume(&preset.id, preset.volume_size_gb(), &region)
                .await
                .map_err(|error| error.to_string())?;
            orchestrator
                .launch(preset.clone(), volume, events_tx)
                .await
                .map_err(|error| error.to_string())?
        }
    };
    let _ = forward.await;
    let usd_to_eur = fx_task.await.unwrap_or(1.0);

    let proxy = match LocalProxy::start(&session.remote_url).await {
        Ok(proxy) => proxy,
        Err(error) => {
            cleanup_failed_launch(&runpod, &session.pod_id).await;
            return Err(error.to_string());
        }
    };
    let adapter = match PiAdapter::system() {
        Ok(adapter) => adapter,
        Err(error) => {
            proxy.shutdown().await;
            cleanup_failed_launch(&runpod, &session.pod_id).await;
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
            cleanup_failed_launch(&runpod, &session.pod_id).await;
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

    Ok((session, wiring, proxy, runpod, usd_to_eur))
}

#[tauri::command]
pub async fn stop_session(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let pod_id = state
        .running_pod_id()
        .await
        .ok_or_else(|| "no running session".to_owned())?;
    finalize_session(&app, &pod_id, StopReason::Manual).await
}

fn spawn_session_monitor(app: AppHandle, view: SessionView, runpod: RunpodClient, usd_to_eur: f64) {
    tauri::async_runtime::spawn(async move {
        let pod_id = view.session.pod_id.clone();
        let mut tracker = match BudgetTracker::new(
            view.budget,
            view.idle_timeout_minutes,
            view.session.started_at_epoch_ms,
            view.cost_per_hr_eur,
            now_epoch_ms(),
        ) {
            Ok(tracker) => tracker,
            Err(_) => {
                let _ = finalize_session(&app, &pod_id, StopReason::TimeBudget).await;
                return;
            }
        };
        let mut ticks = 0_u64;
        let mut resync = None;
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let state = app.state::<AppState>();
            let Some(sample) = state.session_sample(&pod_id).await else {
                return;
            };
            ticks += 1;
            if resync.as_ref().is_some_and(|task| task.is_finished()) {
                let result = resync.take().expect("finished resync task").await;
                if let Ok(Ok(pod)) = result {
                    if !pod.is_running() {
                        let _ = finalize_session(&app, &pod_id, StopReason::RemoteStopped).await;
                        return;
                    }
                    if let Some(cost_per_hr_usd) = pod.effective_cost_per_hr() {
                        let cost_per_hr_eur = cost_per_hr_usd * usd_to_eur;
                        if tracker.update_cost_per_hr(cost_per_hr_eur).is_ok() {
                            state.update_cost_per_hr(&pod_id, cost_per_hr_eur).await;
                        }
                    }
                }
            }
            if ticks % COST_RESYNC_INTERVAL_SECONDS == 0 && resync.is_none() {
                let resync_runpod = runpod.clone();
                let resync_pod_id = pod_id.clone();
                resync = Some(tokio::spawn(async move {
                    resync_runpod.get_pod(&resync_pod_id).await
                }));
            }
            let (telemetry, stop_reason) =
                tracker.tick(now_epoch_ms(), sample.last_request_epoch_ms);
            state.update_telemetry(&pod_id, telemetry.clone()).await;
            let _ = app.emit("session-telemetry", telemetry);
            if let Some(reason) = stop_reason {
                let _ = finalize_session(&app, &pod_id, reason).await;
                return;
            }
        }
    });
}

async fn finalize_session(app: &AppHandle, pod_id: &str, reason: StopReason) -> Result<(), String> {
    let state = app.state::<AppState>();
    let Some(active) = state.take_running(pod_id).await else {
        return Ok(());
    };
    if reason != StopReason::RemoteStopped
        && let Err(error) = stop_with_retry(&active.runpod, pod_id).await
    {
        state.restore_running(active).await;
        return Err(error);
    }

    let ActiveSession {
        view,
        proxy,
        runpod,
        telemetry,
    } = active;
    proxy.shutdown().await;
    let now = now_epoch_ms();
    let duration_seconds = now.saturating_sub(view.session.started_at_epoch_ms) / 1_000;
    let final_cost_eur = telemetry
        .as_ref()
        .map(|telemetry| telemetry.accrued_cost_eur)
        .unwrap_or(duration_seconds as f64 / 3_600.0 * view.cost_per_hr_eur);
    let history_error = history::append(
        &state.history_path,
        &view,
        duration_seconds,
        final_cost_eur,
        reason,
    )
    .err()
    .map(|error| error.to_string());
    state
        .set_grace(GraceSession {
            view,
            runpod: runpod.clone(),
            stopped_at_epoch_ms: now,
        })
        .await;
    let _ = app.emit(
        "session-stopped",
        SessionStoppedEvent {
            reason,
            history_error,
        },
    );
    schedule_termination(app.clone(), pod_id.to_owned(), runpod);
    Ok(())
}

fn schedule_termination(app: AppHandle, pod_id: String, runpod: RunpodClient) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(TERMINATION_GRACE).await;
        let state = app.state::<AppState>();
        if state.take_grace(&pod_id).await.is_none() {
            return;
        }
        if let Err(error) = terminate_with_retry(&runpod, &pod_id).await {
            let _ = app.emit("session-cleanup-error", error);
        }
    });
}

async fn stop_with_retry(runpod: &RunpodClient, pod_id: &str) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 0..3 {
        match runpod.stop_pod(pod_id).await {
            Ok(_) => return Ok(()),
            Err(error) => last_error = Some(error.to_string()),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    Err(last_error.unwrap_or_else(|| "RunPod stop failed".to_owned()))
}

async fn terminate_with_retry(runpod: &RunpodClient, pod_id: &str) -> Result<(), String> {
    let mut last_error = None;
    for attempt in 0..3 {
        match runpod.terminate_pod(pod_id).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error.to_string()),
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    Err(last_error.unwrap_or_else(|| "RunPod termination failed".to_owned()))
}

async fn cleanup_failed_launch(runpod: &RunpodClient, pod_id: &str) {
    let _ = stop_with_retry(runpod, pod_id).await;
    let _ = terminate_with_retry(runpod, pod_id).await;
}

fn emit_stage(app: &AppHandle, stage: LaunchStage, detail: &str) -> Result<(), String> {
    app.emit(
        "launch-progress",
        LaunchEvent {
            stage,
            detail: detail.to_owned(),
            completed_bytes: None,
            total_bytes: None,
            skipped: false,
        },
    )
    .map_err(|error| error.to_string())
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) async fn shutdown_for_exit(app: AppHandle) -> bool {
    loop {
        match app.state::<AppState>().prepare_exit().await {
            ExitAction::Exit => {
                app.exit(0);
                return true;
            }
            ExitAction::WaitForLaunch => tokio::time::sleep(Duration::from_secs(1)).await,
            ExitAction::Stop(pod_id) => {
                if let Err(error) = finalize_session(&app, &pod_id, StopReason::Manual).await {
                    let _ = app.emit("session-cleanup-error", error);
                    return false;
                }
            }
            ExitAction::Terminate(grace) => {
                if let Err(error) =
                    terminate_with_retry(&grace.runpod, &grace.view.session.pod_id).await
                {
                    let _ = app.emit("session-cleanup-error", error);
                    return false;
                }
            }
        }
    }
}
