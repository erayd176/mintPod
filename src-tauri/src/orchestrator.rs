use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use thiserror::Error;
use tokio::sync::mpsc::{self, UnboundedSender};

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
    pub remote_url: String,
    pub started_at_epoch_ms: u64,
    pub cost_per_hr: f64,
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
}

pub struct LaunchOrchestrator {
    runpod: RunpodClient,
}

impl LaunchOrchestrator {
    pub fn new(runpod: RunpodClient) -> Self {
        Self { runpod }
    }

    pub async fn launch(
        &self,
        preset: Preset,
        network_volume: crate::runpod::NetworkVolume,
        events: UnboundedSender<LaunchEvent>,
    ) -> Result<RunningSession, LaunchError> {
        send_stage(
            &events,
            LaunchStage::RequestingPod,
            "Requesting GPU capacity",
        );
        let request = CreatePodRequest::ollama(
            format!("podpilot-{}", preset.id),
            preset.gpu_type_ids.clone(),
            &network_volume,
            preset.volume_size_gb(),
        );
        let started_at_epoch_ms = now_epoch_ms();
        let pod = self.runpod.create_pod(&request).await?;
        let pod_id = pod.id.clone();

        let result = self
            .finish_launch(pod_id.clone(), preset, started_at_epoch_ms, events)
            .await;
        if result.is_err() {
            let _ = self.runpod.stop_pod(&pod_id).await;
        }
        result
    }

    async fn finish_launch(
        &self,
        pod_id: String,
        preset: Preset,
        started_at_epoch_ms: u64,
        events: UnboundedSender<LaunchEvent>,
    ) -> Result<RunningSession, LaunchError> {
        send_stage(
            &events,
            LaunchStage::BootingContainer,
            "Waiting for container",
        );
        let pod = self.wait_for_running(&pod_id, &events).await?;
        let remote_url = format!("https://{pod_id}-11434.proxy.runpod.net");
        let ollama = OllamaClient::new(&remote_url)?;

        send_stage(
            &events,
            LaunchStage::BootingContainer,
            "Waiting for Ollama health",
        );
        ollama.wait_until_healthy(OLLAMA_HEALTH_ATTEMPTS).await?;

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
            let pull_result = ollama.pull_model(&preset.ollama_tag, pull_tx).await;
            let _ = forward.await;
            pull_result?;
        }

        send_stage(&events, LaunchStage::WarmingUp, "Loading model into VRAM");
        ollama.warm_model(&preset.ollama_tag).await?;
        send_stage(&events, LaunchStage::Ready, "Model ready");

        Ok(RunningSession {
            pod_id,
            preset_id: preset.id,
            model_label: preset.label,
            ollama_tag: preset.ollama_tag,
            remote_url,
            started_at_epoch_ms,
            cost_per_hr: pod
                .effective_cost_per_hr()
                .ok_or(LaunchError::MissingCost)?,
        })
    }

    async fn wait_for_running(
        &self,
        pod_id: &str,
        events: &UnboundedSender<LaunchEvent>,
    ) -> Result<crate::runpod::Pod, LaunchError> {
        let mut previous_status = None;
        let mut consecutive_failures = 0;
        for _ in 0..POD_READY_ATTEMPTS {
            let pod = match self.runpod.get_pod(pod_id).await {
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
                    tokio::time::sleep(Duration::from_secs(2)).await;
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
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(LaunchError::PodTimeout)
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
