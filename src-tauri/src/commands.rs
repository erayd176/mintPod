use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{
    credentials::{CredentialProfile, CredentialStore, generate_secret},
    fx,
    harness::{HarnessAdapter, HarnessConnection, LOCAL_PROXY_URL, PiAdapter, WiringReceipt},
    history::{self, SessionHistoryEntry},
    journal::{JournalStage, NewSessionJournal, SessionJournal, SessionJournalStore},
    lifecycle::{BudgetTracker, LaunchBudget, StopReason},
    ollama::OllamaClient,
    orchestrator::{LaunchEvent, LaunchOrchestrator, LaunchSpec, LaunchStage, RunningSession},
    presets::{GPU_TIERS, GpuTierView, Preset, PresetView, verified_gpu_tier},
    runpod::{Pod, RunpodClient, RunpodError, model_volume_preset_id},
    settings::{SettingsStore, SettingsView, VERIFIED_STORAGE_REGIONS},
    state::{ActiveSession, AppState, ExitAction, SessionView},
};

const HOBBY_RANGE_WARNING: &str = "outside default hobby range, continue anyway?";
const COST_RESYNC_INTERVAL_SECONDS: u64 = 30;
const DEFAULT_CONTEXT_LENGTH: u32 = 65_536;

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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryView {
    launch_id: String,
    pod_id: String,
    pod_name: String,
    preset_id: String,
    stage: JournalStage,
    created_at_epoch_ms: u64,
    remote_status: String,
    cost_per_hr_usd: Option<f64>,
    last_error: Option<String>,
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
    let api_key = CredentialStore::read_active_key(&state.credential_index_path)
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
            let preset_id = model_volume_preset_id(&volume.name)?.to_owned();
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
    let api_key = CredentialStore::read_active_key(&state.credential_index_path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "RunPod API key is not configured".to_owned())?;
    let runpod = RunpodClient::new(api_key).map_err(|error| error.to_string())?;
    let volume = runpod
        .list_network_volumes()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|volume| volume.id == volume_id && model_volume_preset_id(&volume.name).is_some())
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
pub async fn list_api_keys(state: State<'_, AppState>) -> Result<Vec<CredentialProfile>, String> {
    CredentialStore::list_profiles(&state.credential_index_path).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn add_api_key(
    label: String,
    api_key: String,
    state: State<'_, AppState>,
) -> Result<CredentialProfile, String> {
    let api_key = validated_api_key(&api_key).await?;
    prepare_credential_change(&state).await?;
    CredentialStore::add_profile(&state.credential_index_path, &label, &api_key)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn replace_api_key(
    profile_id: String,
    api_key: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let api_key = validated_api_key(&api_key).await?;
    prepare_credential_change(&state).await?;
    CredentialStore::replace_profile(&state.credential_index_path, &profile_id, &api_key)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn select_api_key(profile_id: String, state: State<'_, AppState>) -> Result<(), String> {
    prepare_credential_change(&state).await?;
    CredentialStore::select_profile(&state.credential_index_path, &profile_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn remove_api_key(profile_id: String, state: State<'_, AppState>) -> Result<(), String> {
    prepare_credential_change(&state).await?;
    CredentialStore::delete_profile(&state.credential_index_path, &profile_id)
        .map_err(|error| error.to_string())
}

async fn prepare_credential_change(state: &State<'_, AppState>) -> Result<(), String> {
    state
        .require_idle()
        .await
        .map_err(|error| error.to_string())?;
    if state.session_journal_path.exists() {
        return Err("clean up the recovered mintPod session before changing API keys".to_owned());
    }
    Ok(())
}

async fn validated_api_key(api_key: &str) -> Result<String, String> {
    let api_key = api_key.trim();
    let client = RunpodClient::new(api_key).map_err(|error| error.to_string())?;
    client
        .validate_key()
        .await
        .map_err(|error| error.to_string())?;
    Ok(api_key.to_owned())
}

#[tauri::command]
pub async fn launch_preset(
    preset_id: String,
    budget: LaunchBudget,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SessionView, String> {
    let budget = budget.validate().map_err(|error| error.to_string())?;
    let cancellation = state
        .begin_launch()
        .await
        .map_err(|error| error.to_string())?;
    let idle_timeout_minutes = state
        .settings()
        .map_err(|_| "settings lock is poisoned".to_owned())?
        .idle_timeout_minutes;
    let result = launch_preset_inner(
        &preset_id,
        budget,
        idle_timeout_minutes,
        cancellation,
        &app,
        &state,
    )
    .await;
    match result {
        Ok((session, wiring, runpod, usd_to_eur)) => {
            let cost_per_hr_eur = session.cost_per_hr_usd * usd_to_eur;
            let view = SessionView {
                session,
                wiring,
                budget,
                idle_timeout_minutes,
                cost_per_hr_eur,
            };
            let view = state.finish_launch(view, runpod.clone()).await;
            spawn_session_monitor(app, view.clone(), runpod, usd_to_eur);
            Ok(view)
        }
        Err(error) => {
            let _ = state.gateway.disconnect();
            let cleanup_error = cleanup_recorded_session(&state, false).await.err();
            state.fail_launch().await;
            Err(match cleanup_error {
                Some(cleanup) => format!("{error}; cleanup still required: {cleanup}"),
                None => error,
            })
        }
    }
}

async fn launch_preset_inner(
    preset_id: &str,
    budget: LaunchBudget,
    idle_timeout_minutes: u16,
    cancellation: tokio_util::sync::CancellationToken,
    app: &AppHandle,
    state: &State<'_, AppState>,
) -> Result<(RunningSession, WiringReceipt, RunpodClient, f64), String> {
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
    let (profile, api_key) = CredentialStore::read_active(&state.credential_index_path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "RunPod API key is not configured".to_owned())?;
    let runpod = RunpodClient::new(api_key).map_err(|error| error.to_string())?;
    let mut journal = SessionJournalStore::prepare(
        &state.session_journal_path,
        &state.install_id,
        NewSessionJournal {
            credential_profile_id: profile.id,
            preset_id: preset.id.clone(),
            data_center_id: region.clone(),
            budget,
            idle_timeout_minutes,
            created_at_epoch_ms: now_epoch_ms(),
        },
    )
    .map_err(|error| error.to_string())?;
    let runtime_token = generate_secret().map_err(|error| error.to_string())?;
    if let Err(error) = CredentialStore::store_runtime_token(&journal.launch_id, &runtime_token) {
        let _ = SessionJournalStore::clear(&state.session_journal_path);
        return Err(error.to_string());
    }
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
    emit_stage(
        app,
        LaunchStage::RequestingPod,
        "Preparing persistent model volume",
    )?;
    let volume = runpod
        .ensure_model_volume(&preset.id, preset.volume_size_gb(), &region)
        .await
        .map_err(|error| error.to_string())?;
    journal.volume_id = Some(volume.id.clone());
    journal.stage = JournalStage::VolumeReady;
    SessionJournalStore::save(&state.session_journal_path, &journal)
        .map_err(|error| error.to_string())?;
    if cancellation.is_cancelled() {
        return Err("launch cancelled".to_owned());
    }
    let pod_name = journal.pod_name.clone();
    let journal_path = state.session_journal_path.clone();
    let launch_budget_cancellation = cancellation.clone();
    let estimated_cost_per_hr = preset.est_cost_per_hr;
    journal.stage = JournalStage::PodRequested;
    SessionJournalStore::save(&journal_path, &journal).map_err(|error| error.to_string())?;
    let session = orchestrator
        .launch(
            LaunchSpec {
                pod_name,
                preset: preset.clone(),
                network_volume: volume,
                remote_token: runtime_token,
                context_length: DEFAULT_CONTEXT_LENGTH,
                cancellation,
            },
            |pod, pod_created_at_epoch_ms| {
                journal.pod_id = Some(pod.id.clone());
                journal.pod_created_at_epoch_ms = Some(pod_created_at_epoch_ms);
                journal.stage = JournalStage::PodCreated;
                SessionJournalStore::save(&journal_path, &journal)
                    .map_err(|error| error.to_string())?;
                let launch_cost_per_hr = pod
                    .effective_cost_per_hr()
                    .filter(|rate| rate.is_finite() && *rate > 0.0)
                    .unwrap_or(estimated_cost_per_hr)
                    .max(estimated_cost_per_hr);
                spawn_launch_budget_guard(
                    launch_budget_cancellation,
                    budget,
                    pod_created_at_epoch_ms,
                    launch_cost_per_hr,
                );
                Ok(())
            },
            events_tx,
        )
        .await
        .map_err(|error| error.to_string())?;
    let _ = forward.await;
    let usd_to_eur = fx_task.await.unwrap_or(1.0);

    state
        .gateway
        .connect(&session.remote_url, &session.remote_token)
        .map_err(|error| error.to_string())?;
    let adapter = match PiAdapter::system() {
        Ok(adapter) => adapter,
        Err(error) => {
            let _ = state.gateway.disconnect();
            return Err(error.to_string());
        }
    };
    let wiring = match adapter.wire(&HarnessConnection {
        url: LOCAL_PROXY_URL,
        api_key: state.gateway.token(),
        preset: &preset,
    }) {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = state.gateway.disconnect();
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
    journal.stage = JournalStage::Ready;
    journal.last_error = None;
    SessionJournalStore::save(&state.session_journal_path, &journal)
        .map_err(|error| error.to_string())?;

    Ok((session, wiring, runpod, usd_to_eur))
}

fn spawn_launch_budget_guard(
    cancellation: tokio_util::sync::CancellationToken,
    budget: LaunchBudget,
    started_at_epoch_ms: u64,
    conservative_cost_per_hr: f64,
) {
    let limit_ms = match budget {
        LaunchBudget::Time { minutes } => u64::from(minutes) * 60_000,
        LaunchBudget::Cost { eur } => {
            (eur / conservative_cost_per_hr * 3_600_000.0).max(0.0) as u64
        }
    };
    tauri::async_runtime::spawn(async move {
        let elapsed_ms = now_epoch_ms().saturating_sub(started_at_epoch_ms);
        tokio::time::sleep(Duration::from_millis(limit_ms.saturating_sub(elapsed_ms))).await;
        cancellation.cancel();
    });
}

#[tauri::command]
pub async fn cancel_launch(state: State<'_, AppState>) -> Result<(), String> {
    if state.cancel_launch().await {
        Ok(())
    } else {
        Err("no launch is active".to_owned())
    }
}

#[tauri::command]
pub async fn recovery_status(state: State<'_, AppState>) -> Result<Option<RecoveryView>, String> {
    state
        .require_idle()
        .await
        .map_err(|error| error.to_string())?;
    let Some(mut journal) = SessionJournalStore::load(&state.session_journal_path)
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let runpod = runpod_for_journal(&state, &journal)?;
    let Some(pod) = resolve_journal_pod(&runpod, &mut journal, &state.session_journal_path).await?
    else {
        if matches!(
            journal.stage,
            JournalStage::PodRequested | JournalStage::CleanupPending
        ) {
            return Ok(Some(RecoveryView {
                launch_id: journal.launch_id,
                pod_id: String::new(),
                pod_name: journal.pod_name,
                preset_id: journal.preset_id,
                stage: journal.stage,
                created_at_epoch_ms: journal.created_at_epoch_ms,
                remote_status: "NOT_FOUND".to_owned(),
                cost_per_hr_usd: None,
                last_error: journal.last_error,
            }));
        }
        clear_local_ownership(&state, &journal)?;
        return Ok(None);
    };
    let cost_per_hr_usd = pod.effective_cost_per_hr();
    Ok(Some(RecoveryView {
        launch_id: journal.launch_id,
        pod_id: pod.id,
        pod_name: journal.pod_name,
        preset_id: journal.preset_id,
        stage: journal.stage,
        created_at_epoch_ms: journal.created_at_epoch_ms,
        remote_status: pod.desired_status.unwrap_or_else(|| "UNKNOWN".to_owned()),
        cost_per_hr_usd,
        last_error: journal.last_error,
    }))
}

#[tauri::command]
pub async fn cleanup_recovery(state: State<'_, AppState>) -> Result<(), String> {
    state
        .require_idle()
        .await
        .map_err(|error| error.to_string())?;
    cleanup_recorded_session(&state, true).await
}

#[tauri::command]
pub async fn recover_session(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SessionView, String> {
    let cancellation = state
        .begin_recovery()
        .await
        .map_err(|error| error.to_string())?;
    let result = recover_session_inner(cancellation, &state).await;
    match result {
        Ok((view, runpod, usd_to_eur)) => {
            let view = state.finish_launch(view, runpod.clone()).await;
            spawn_session_monitor(app, view.clone(), runpod, usd_to_eur);
            Ok(view)
        }
        Err(error) => {
            let _ = state.gateway.disconnect();
            if let Ok(Some(mut journal)) = SessionJournalStore::load(&state.session_journal_path) {
                journal.last_error = Some(error.clone());
                let _ = SessionJournalStore::save(&state.session_journal_path, &journal);
            }
            state.fail_launch().await;
            Err(error)
        }
    }
}

async fn recover_session_inner(
    cancellation: tokio_util::sync::CancellationToken,
    state: &State<'_, AppState>,
) -> Result<(SessionView, RunpodClient, f64), String> {
    let mut journal = SessionJournalStore::load(&state.session_journal_path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "no mintPod session needs recovery".to_owned())?;
    let preset = state
        .presets()
        .map_err(|_| "preset catalog lock is poisoned".to_owned())?
        .find(&journal.preset_id)
        .ok_or_else(|| {
            format!(
                "recovery model '{}' is no longer configured",
                journal.preset_id
            )
        })?;
    let runpod = runpod_for_journal(state, &journal)?;
    let pod = resolve_journal_pod(&runpod, &mut journal, &state.session_journal_path)
        .await?
        .ok_or_else(|| "the recovered pod no longer exists".to_owned())?;
    if !pod.is_running() {
        return Err(format!(
            "the recovered pod is {}; end it before launching again",
            pod.desired_status.as_deref().unwrap_or("not running")
        ));
    }
    let remote_token = CredentialStore::read_runtime_token(&journal.launch_id)
        .map_err(|error| error.to_string())?;
    let remote_url = format!("https://{}-8000.proxy.runpod.net", pod.id);
    let ollama =
        OllamaClient::new(&remote_url, &remote_token).map_err(|error| error.to_string())?;
    tokio::select! {
        _ = cancellation.cancelled() => return Err("recovery cancelled".to_owned()),
        result = ollama.wait_until_healthy(5) => result.map_err(|error| error.to_string())?,
    }
    if !ollama
        .has_model(&preset.ollama_tag)
        .await
        .map_err(|error| error.to_string())?
    {
        return Err("the recovered runtime is healthy but the model is not ready".to_owned());
    }
    state
        .gateway
        .connect(&remote_url, &remote_token)
        .map_err(|error| error.to_string())?;
    let adapter = PiAdapter::system().map_err(|error| error.to_string())?;
    let wiring = adapter
        .wire(&HarnessConnection {
            url: LOCAL_PROXY_URL,
            api_key: state.gateway.token(),
            preset: &preset,
        })
        .map_err(|error| error.to_string())?;
    let cost_per_hr_usd = pod
        .effective_cost_per_hr()
        .ok_or_else(|| "RunPod did not return a cost for the recovered pod".to_owned())?;
    let usd_to_eur = fx::usd_to_eur(&state.fx_rate_path).await;
    let gpu_name = pod
        .allocated_gpu()
        .unwrap_or("GPU details unavailable")
        .to_owned();
    let data_center_id = pod
        .data_center_id()
        .unwrap_or(&journal.data_center_id)
        .to_owned();
    let session = RunningSession {
        pod_id: pod.id,
        preset_id: preset.id,
        model_label: preset.label,
        ollama_tag: preset.ollama_tag,
        gpu_name,
        data_center_id,
        remote_url,
        started_at_epoch_ms: journal
            .pod_created_at_epoch_ms
            .unwrap_or(journal.created_at_epoch_ms),
        cost_per_hr_usd,
        remote_token,
    };
    journal.stage = JournalStage::Ready;
    journal.last_error = None;
    SessionJournalStore::save(&state.session_journal_path, &journal)
        .map_err(|error| error.to_string())?;
    Ok((
        SessionView {
            cost_per_hr_eur: cost_per_hr_usd * usd_to_eur,
            session,
            wiring,
            budget: journal.budget,
            idle_timeout_minutes: journal.idle_timeout_minutes,
        },
        runpod,
        usd_to_eur,
    ))
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
        let mut resync: Option<tokio::task::JoinHandle<Result<Pod, RunpodError>>> = None;
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
            if ticks.is_multiple_of(COST_RESYNC_INTERVAL_SECONDS) && resync.is_none() {
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
    if let Err(error) = terminate_with_retry(&active.runpod, pod_id).await {
        state.restore_running(active).await;
        return Err(error);
    }

    let ActiveSession {
        view,
        runpod: _,
        telemetry,
    } = active;
    let gateway_error = state
        .gateway
        .disconnect()
        .err()
        .map(|error| error.to_string());
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
    if let Ok(Some(journal)) = SessionJournalStore::load(&state.session_journal_path) {
        let _ = CredentialStore::delete_runtime_token(&journal.launch_id);
    }
    let journal_cleanup_error = SessionJournalStore::clear(&state.session_journal_path)
        .err()
        .map(|error| error.to_string());
    let _ = app.emit(
        "session-stopped",
        SessionStoppedEvent {
            reason,
            history_error: history_error.or(journal_cleanup_error).or(gateway_error),
        },
    );
    Ok(())
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

async fn cleanup_recorded_session(
    state: &AppState,
    confirm_absent_pod: bool,
) -> Result<(), String> {
    let Some(mut journal) = SessionJournalStore::load(&state.session_journal_path)
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let runpod = runpod_for_journal(state, &journal)?;
    let creation_outcome_uncertain = journal.pod_id.is_none()
        && matches!(
            journal.stage,
            JournalStage::PodRequested | JournalStage::CleanupPending
        );
    journal.stage = JournalStage::CleanupPending;
    SessionJournalStore::save(&state.session_journal_path, &journal)
        .map_err(|error| error.to_string())?;
    match resolve_journal_pod(&runpod, &mut journal, &state.session_journal_path).await? {
        Some(pod) => {
            if let Err(error) = terminate_with_retry(&runpod, &pod.id).await {
                journal.last_error = Some(error.clone());
                let _ = SessionJournalStore::save(&state.session_journal_path, &journal);
                return Err(error);
            }
        }
        None if creation_outcome_uncertain && !confirm_absent_pod => {
            let error =
                "pod creation outcome is uncertain; use recovery cleanup to confirm no pod exists"
                    .to_owned();
            journal.last_error = Some(error.clone());
            let _ = SessionJournalStore::save(&state.session_journal_path, &journal);
            return Err(error);
        }
        None => {}
    }
    clear_local_ownership(state, &journal)
}

fn runpod_for_journal(state: &AppState, journal: &SessionJournal) -> Result<RunpodClient, String> {
    let api_key = CredentialStore::read_profile_key(
        &state.credential_index_path,
        &journal.credential_profile_id,
    )
    .map_err(|error| error.to_string())?;
    RunpodClient::new(api_key).map_err(|error| error.to_string())
}

async fn resolve_journal_pod(
    runpod: &RunpodClient,
    journal: &mut SessionJournal,
    journal_path: &std::path::Path,
) -> Result<Option<Pod>, String> {
    if let Some(pod_id) = journal.pod_id.as_deref() {
        match runpod.get_pod(pod_id).await {
            Ok(pod) => return Ok(Some(pod)),
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.to_string()),
        }
    }
    let pod = runpod
        .list_pods()
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|pod| pod.name == journal.pod_name);
    if let Some(pod) = &pod {
        journal.pod_id = Some(pod.id.clone());
        journal.stage = JournalStage::PodCreated;
        SessionJournalStore::save(journal_path, journal).map_err(|error| error.to_string())?;
    }
    Ok(pod)
}

fn clear_local_ownership(state: &AppState, journal: &SessionJournal) -> Result<(), String> {
    CredentialStore::delete_runtime_token(&journal.launch_id).map_err(|error| error.to_string())?;
    SessionJournalStore::clear(&state.session_journal_path).map_err(|error| error.to_string())
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
            ExitAction::CancelLaunch => tokio::time::sleep(Duration::from_millis(250)).await,
            ExitAction::Stop(pod_id) => {
                if let Err(error) = finalize_session(&app, &pod_id, StopReason::Manual).await {
                    let _ = app.emit("session-cleanup-error", error);
                    return false;
                }
            }
        }
    }
}
