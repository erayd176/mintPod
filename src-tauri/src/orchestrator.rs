use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use thiserror::Error;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::{
    ollama::{OllamaClient, OllamaError, PullProgress},
    presets::Preset,
    runpod::{CreatePodRequest, RunpodClient, RunpodError},
};

const POD_READY_ATTEMPTS: u16 = 300;
const OLLAMA_HEALTH_ATTEMPTS: u16 = 300;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LaunchStage {
    RequestingPod,
    BootingContainer,
    PullingModel,
    WarmingUp,
    Ready,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchEvent {
    pub stage: LaunchStage,
    pub detail: String,
    pub completed_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub skipped: bool,
}

impl LaunchEvent {
    fn stage(stage: LaunchStage, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
            completed_bytes: None,
            total_bytes: None,
            skipped: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningSession {
    pub pod_id: String,
    pub preset_id: String,
    pub model_label: String,
    pub ollama_tag: String,
    pub gpu_name: String,
    pub data_center_id: String,
    pub remote_url: String,
    pub started_at_epoch_ms: u64,
    pub cost_per_hr_usd: f64,
    #[serde(skip_serializing)]
    pub remote_token: String,
}

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error(transparent)]
    Runpod(#[from] RunpodError),
    #[error(transparent)]
    Ollama(#[from] OllamaError),
    #[error("pod did not reach RUNNING in time")]
    PodTimeout,
    #[error("RunPod did not return a cost for the running pod")]
    MissingCost,
    #[error("launch cancelled")]
    Cancelled,
    #[error("could not persist pod ownership: {0}")]
    Persistence(String),
}

pub struct LaunchOrchestrator {
    runpod: RunpodClient,
}

pub struct LaunchSpec {
    pub pod_name: String,
    pub preset: Preset,
    pub network_volume: crate::runpod::NetworkVolume,
    pub remote_token: String,
    pub context_length: u32,
    pub cancellation: CancellationToken,
}

struct PendingSession {
    pod_id: String,
    preset: Preset,
    requested_data_center_id: String,
    started_at_epoch_ms: u64,
    remote_token: String,
    cancellation: CancellationToken,
}

impl LaunchOrchestrator {
    pub fn new(runpod: RunpodClient) -> Self {
        Self { runpod }
    }

    pub async fn launch<F>(
        &self,
        spec: LaunchSpec,
        on_pod_created: F,
        events: UnboundedSender<LaunchEvent>,
    ) -> Result<RunningSession, LaunchError>
    where
        F: FnOnce(&crate::runpod::Pod, u64) -> Result<(), String>,
    {
        send_stage(
            &events,
            LaunchStage::RequestingPod,
            "Requesting GPU capacity",
        );
        let request = CreatePodRequest::ollama(
            spec.pod_name,
            spec.preset.gpu_type_ids.clone(),
            &spec.network_volume,
            spec.preset.volume_size_gb(),
            &spec.remote_token,
            spec.context_length,
        );
        let started_at_epoch_ms = now_epoch_ms();
        let pod = self.runpod.create_pod(&request).await?;
        if let Err(error) = on_pod_created(&pod, started_at_epoch_ms) {
            let cleanup = self.runpod.terminate_pod(&pod.id).await.err();
            let message = match cleanup {
                Some(cleanup) => format!("{error}; immediate cleanup failed: {cleanup}"),
                None => error,
            };
            return Err(LaunchError::Persistence(message));
        }
        let pod_id = pod.id.clone();
        let data_center_id = spec.network_volume.data_center_id;
        if spec.cancellation.is_cancelled() {
            return Err(LaunchError::Cancelled);
        }

        self.finish_launch(
            PendingSession {
                pod_id,
                preset: spec.preset,
                requested_data_center_id: data_center_id,
                started_at_epoch_ms,
                remote_token: spec.remote_token,
                cancellation: spec.cancellation,
            },
            events,
        )
        .await
    }

    async fn finish_launch(
        &self,
        pending: PendingSession,
        events: UnboundedSender<LaunchEvent>,
    ) -> Result<RunningSession, LaunchError> {
        let PendingSession {
            pod_id,
            preset,
            requested_data_center_id,
            started_at_epoch_ms,
            remote_token,
            cancellation,
        } = pending;
        send_stage(
            &events,
            LaunchStage::BootingContainer,
            "Waiting for container",
        );
        let pod = self
            .wait_for_running(&pod_id, &cancellation, &events)
            .await?;
        let gpu_name = pod
            .allocated_gpu()
            .unwrap_or("GPU details unavailable")
            .to_owned();
        let data_center_id = pod
            .data_center_id()
            .unwrap_or(&requested_data_center_id)
            .to_owned();
        let cost_per_hr_usd = pod
            .effective_cost_per_hr()
            .ok_or(LaunchError::MissingCost)?;
        send_stage(
            &events,
            LaunchStage::BootingContainer,
            format!("{gpu_name} · {data_center_id} · ${cost_per_hr_usd:.2}/hr"),
        );
        let remote_url = format!("https://{pod_id}-8000.proxy.runpod.net");
        let ollama = OllamaClient::new(&remote_url, &remote_token)?;

        tokio::select! {
            _ = cancellation.cancelled() => return Err(LaunchError::Cancelled),
            result = ollama.wait_until_healthy(OLLAMA_HEALTH_ATTEMPTS) => result?,
        }

        send_stage(
            &events,
            LaunchStage::PullingModel,
            "Checking persistent cache",
        );
        if ollama.has_model(&preset.ollama_tag).await? {
            let mut event = LaunchEvent::stage(LaunchStage::PullingModel, "Already cached");
            event.skipped = true;
            let _ = events.send(event);
        } else {
            let (pull_tx, mut pull_rx) = mpsc::unbounded_channel::<PullProgress>();
            let forward_events = events.clone();
            let forward = tokio::spawn(async move {
                while let Some(progress) = pull_rx.recv().await {
                    let _ = forward_events.send(LaunchEvent {
                        stage: LaunchStage::PullingModel,
                        detail: progress.status,
                        completed_bytes: progress.completed_bytes,
                        total_bytes: progress.total_bytes,
                        skipped: false,
                    });
                }
            });
            let pull_result = tokio::select! {
                _ = cancellation.cancelled() => Err(LaunchError::Cancelled),
                result = ollama.pull_model(&preset.ollama_tag, pull_tx) => {
                    result.map_err(LaunchError::from)
                },
            };
            let _ = forward.await;
            pull_result?;
        }

        send_stage(&events, LaunchStage::WarmingUp, "Loading model into VRAM");
        tokio::select! {
            _ = cancellation.cancelled() => return Err(LaunchError::Cancelled),
            result = ollama.warm_model(&preset.ollama_tag) => result?,
        }

        Ok(RunningSession {
            pod_id,
            preset_id: preset.id,
            model_label: preset.label,
            ollama_tag: preset.ollama_tag,
            gpu_name,
            data_center_id,
            remote_url,
            started_at_epoch_ms,
            cost_per_hr_usd,
            remote_token,
        })
    }

    async fn wait_for_running(
        &self,
        pod_id: &str,
        cancellation: &CancellationToken,
        events: &UnboundedSender<LaunchEvent>,
    ) -> Result<crate::runpod::Pod, LaunchError> {
        let mut previous_status = None;
        let mut consecutive_failures = 0;
        for _ in 0..POD_READY_ATTEMPTS {
            let pod_result = tokio::select! {
                _ = cancellation.cancelled() => return Err(LaunchError::Cancelled),
                result = self.runpod.get_pod(pod_id) => result,
            };
            let pod = match pod_result {
                Ok(pod) => {
                    consecutive_failures = 0;
                    pod
                }
                Err(error) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= 5 {
                        return Err(error.into());
                    }
                    send_stage(
                        events,
                        LaunchStage::BootingContainer,
                        "RunPod status unavailable; retrying",
                    );
                    cancellable_sleep(cancellation, Duration::from_secs(2)).await?;
                    continue;
                }
            };
            let status = pod.desired_status.as_deref().unwrap_or("UNKNOWN");
            if previous_status.as_deref() != Some(status) {
                send_stage(
                    events,
                    LaunchStage::BootingContainer,
                    format!("Pod status: {status}"),
                );
                previous_status = Some(status.to_owned());
            }
            if pod.is_running() {
                return Ok(pod);
            }
            cancellable_sleep(cancellation, Duration::from_secs(2)).await?;
        }
        Err(LaunchError::PodTimeout)
    }
}

async fn cancellable_sleep(
    cancellation: &CancellationToken,
    duration: Duration,
) -> Result<(), LaunchError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(LaunchError::Cancelled),
        _ = tokio::time::sleep(duration) => Ok(()),
    }
}

fn send_stage(
    events: &UnboundedSender<LaunchEvent>,
    stage: LaunchStage,
    detail: impl Into<String>,
) {
    let _ = events.send(LaunchEvent::stage(stage, detail));
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_events_use_stable_camel_case_stage_names() {
        let event = LaunchEvent::stage(LaunchStage::BootingContainer, "Waiting");
        let json = serde_json::to_value(event).unwrap();

        assert_eq!(json["stage"], "bootingContainer");
        assert_eq!(json["detail"], "Waiting");
    }
}
