use std::{collections::HashMap, sync::Arc, time::Duration};

use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const RUNPOD_BASE_URL: &str = "https://rest.runpod.io/v1";
const MODEL_VOLUME_PREFIX: &str = "mintpod-";
const LEGACY_MODEL_VOLUME_PREFIX: &str = "podpilot-";

#[derive(Debug, Error)]
pub enum RunpodError {
    #[error("RunPod rejected the request ({status}): {message}")]
    Api { status: StatusCode, message: String },
    #[error("RunPod request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("RunPod returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("the RunPod API key is empty")]
    EmptyApiKey,
}

#[derive(Clone)]
pub struct RunpodClient {
    http: Client,
    api_key: Arc<str>,
    base_url: Arc<str>,
}

impl RunpodClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, RunpodError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(RunpodError::EmptyApiKey);
        }

        let http = Client::builder()
            .user_agent(concat!("mintPod/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            http,
            api_key: Arc::from(api_key),
            base_url: Arc::from(RUNPOD_BASE_URL),
        })
    }

    #[cfg(test)]
    fn with_base_url(mut self, base_url: impl Into<Arc<str>>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub async fn validate_key(&self) -> Result<(), RunpodError> {
        self.list_pods().await.map(|_| ())
    }

    pub async fn list_pods(&self) -> Result<Vec<Pod>, RunpodError> {
        self.send_json(self.get("/pods")).await
    }

    pub async fn create_pod(&self, request: &CreatePodRequest) -> Result<Pod, RunpodError> {
        self.send_json(self.post("/pods").json(request)).await
    }

    pub async fn get_pod(&self, pod_id: &str) -> Result<Pod, RunpodError> {
        self.send_json(self.get(&pod_detail_path(pod_id))).await
    }

    pub async fn start_pod(&self, pod_id: &str) -> Result<Pod, RunpodError> {
        self.send_json(self.post(&format!("/pods/{pod_id}/start")))
            .await
    }

    pub async fn stop_pod(&self, pod_id: &str) -> Result<Pod, RunpodError> {
        self.send_json(self.post(&format!("/pods/{pod_id}/stop")))
            .await
    }

    pub async fn terminate_pod(&self, pod_id: &str) -> Result<(), RunpodError> {
        let response = self.delete(&format!("/pods/{pod_id}")).send().await?;
        ensure_success(response).await.map(|_| ())
    }

    pub async fn list_network_volumes(&self) -> Result<Vec<NetworkVolume>, RunpodError> {
        self.send_json(self.get("/networkvolumes")).await
    }

    pub async fn create_network_volume(
        &self,
        request: &CreateNetworkVolumeRequest,
    ) -> Result<NetworkVolume, RunpodError> {
        self.send_json(self.post("/networkvolumes").json(request))
            .await
    }

    pub async fn resize_network_volume(
        &self,
        volume_id: &str,
        size: u16,
    ) -> Result<NetworkVolume, RunpodError> {
        self.send_json(
            self.patch(&format!("/networkvolumes/{volume_id}"))
                .json(&serde_json::json!({ "size": size })),
        )
        .await
    }

    pub async fn delete_network_volume(&self, volume_id: &str) -> Result<(), RunpodError> {
        let response = self
            .delete(&format!("/networkvolumes/{volume_id}"))
            .send()
            .await?;
        ensure_success(response).await.map(|_| ())
    }

    pub async fn ensure_model_volume(
        &self,
        preset_id: &str,
        size: u16,
        data_center_id: &str,
    ) -> Result<NetworkVolume, RunpodError> {
        let name = format!("{MODEL_VOLUME_PREFIX}{preset_id}");
        let existing = self
            .list_network_volumes()
            .await?
            .into_iter()
            .find(|volume| {
                model_volume_preset_id(&volume.name) == Some(preset_id)
                    && volume.data_center_id == data_center_id
            });

        match existing {
            Some(volume) if volume.size >= size => Ok(volume),
            Some(volume) => self.resize_network_volume(&volume.id, size).await,
            None => {
                self.create_network_volume(&CreateNetworkVolumeRequest {
                    name,
                    size,
                    data_center_id: data_center_id.to_owned(),
                })
                .await
            }
        }
    }

    fn get(&self, path: &str) -> RequestBuilder {
        self.http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(self.api_key.as_ref())
    }

    fn post(&self, path: &str) -> RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(self.api_key.as_ref())
    }

    fn delete(&self, path: &str) -> RequestBuilder {
        self.http
            .delete(format!("{}{path}", self.base_url))
            .bearer_auth(self.api_key.as_ref())
    }

    fn patch(&self, path: &str) -> RequestBuilder {
        self.http
            .patch(format!("{}{path}", self.base_url))
            .bearer_auth(self.api_key.as_ref())
    }

    async fn send_json<T>(&self, request: RequestBuilder) -> Result<T, RunpodError>
    where
        T: for<'de> Deserialize<'de>,
    {
        ensure_success(request.send().await?)
            .await?
            .json()
            .await
            .map_err(|error| RunpodError::InvalidResponse(error.to_string()))
    }
}

pub fn model_volume_preset_id(name: &str) -> Option<&str> {
    name.strip_prefix(MODEL_VOLUME_PREFIX)
        .or_else(|| name.strip_prefix(LEGACY_MODEL_VOLUME_PREFIX))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePodRequest {
    pub name: String,
    pub image_name: String,
    pub gpu_type_ids: Vec<String>,
    pub gpu_type_priority: &'static str,
    pub gpu_count: u8,
    pub cloud_type: &'static str,
    pub data_center_ids: Vec<String>,
    pub data_center_priority: &'static str,
    pub container_disk_in_gb: u16,
    pub network_volume_id: String,
    pub volume_in_gb: u16,
    pub volume_mount_path: &'static str,
    pub ports: Vec<&'static str>,
    pub env: HashMap<&'static str, &'static str>,
}

impl CreatePodRequest {
    pub fn ollama(
        name: String,
        gpu_type_ids: Vec<String>,
        network_volume: &NetworkVolume,
        volume_in_gb: u16,
    ) -> Self {
        Self {
            name,
            image_name: "ollama/ollama:0.32.3".to_owned(),
            gpu_type_ids,
            gpu_type_priority: "custom",
            gpu_count: 1,
            cloud_type: "SECURE",
            data_center_ids: vec![network_volume.data_center_id.clone()],
            data_center_priority: "custom",
            container_disk_in_gb: 20,
            network_volume_id: network_volume.id.clone(),
            volume_in_gb,
            volume_mount_path: "/root/.ollama",
            ports: vec!["11434/http"],
            env: HashMap::from([
                ("OLLAMA_MODELS", "/root/.ollama/models"),
                ("OLLAMA_KEEP_ALIVE", "-1"),
            ]),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateNetworkVolumeRequest {
    pub name: String,
    pub size: u16,
    pub data_center_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pod {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub desired_status: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub cost_per_hr: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_number")]
    pub adjusted_cost_per_hr: Option<f64>,
    #[serde(default)]
    pub network_volume: Option<NetworkVolume>,
    #[serde(default)]
    pub gpu: Option<PodGpu>,
    #[serde(default)]
    pub machine: Option<PodMachine>,
}

impl Pod {
    pub fn is_running(&self) -> bool {
        self.desired_status.as_deref() == Some("RUNNING")
    }

    pub fn effective_cost_per_hr(&self) -> Option<f64> {
        self.adjusted_cost_per_hr.or(self.cost_per_hr)
    }

    pub fn allocated_gpu(&self) -> Option<&str> {
        self.gpu
            .as_ref()
            .and_then(|gpu| nonempty(&gpu.display_name).or_else(|| nonempty(&gpu.id)))
            .or_else(|| {
                self.machine.as_ref().and_then(|machine| {
                    nonempty(&machine.gpu_display_name).or_else(|| nonempty(&machine.gpu_type_id))
                })
            })
    }

    pub fn data_center_id(&self) -> Option<&str> {
        self.machine
            .as_ref()
            .and_then(|machine| nonempty(&machine.data_center_id))
            .or_else(|| {
                self.network_volume
                    .as_ref()
                    .and_then(|volume| nonempty(&volume.data_center_id))
            })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PodGpu {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub display_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PodMachine {
    #[serde(default)]
    pub gpu_type_id: String,
    #[serde(default)]
    pub gpu_display_name: String,
    #[serde(default)]
    pub data_center_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkVolume {
    pub id: String,
    pub name: String,
    pub size: u16,
    pub data_center_id: String,
}

fn nonempty(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

fn pod_detail_path(pod_id: &str) -> String {
    format!("/pods/{pod_id}?includeMachine=true&includeNetworkVolume=true")
}

async fn ensure_success(response: Response) -> Result<Response, RunpodError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response.text().await.unwrap_or_default();
    let message = error_message(&body).unwrap_or_else(|| {
        status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_owned()
    });
    Err(RunpodError::Api { status, message })
}

fn error_message(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .map(str::to_owned)
                .or_else(|| error.get("message")?.as_str().map(str::to_owned))
        })
        .or_else(|| value.get("message")?.as_str().map(str::to_owned))
}

fn deserialize_optional_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("number is outside f64 range"))
            .map(Some),
        Some(Value::String(number)) => number
            .parse::<f64>()
            .map(Some)
            .map_err(serde::de::Error::custom),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected a number or numeric string, got {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_volumes_accept_current_and_legacy_names() {
        assert_eq!(
            model_volume_preset_id("mintpod-qwen-coder-7b"),
            Some("qwen-coder-7b")
        );
        assert_eq!(
            model_volume_preset_id("podpilot-qwen-coder-7b"),
            Some("qwen-coder-7b")
        );
        assert_eq!(model_volume_preset_id("unrelated-volume"), None);
    }

    #[test]
    fn ollama_request_keeps_models_on_the_mounted_volume() {
        let request = CreatePodRequest::ollama(
            "mintpod-coder".to_owned(),
            vec!["NVIDIA GeForce RTX 4090".to_owned()],
            &NetworkVolume {
                id: "volume-1".to_owned(),
                name: "mintpod-coder".to_owned(),
                size: 12,
                data_center_id: "EU-RO-1".to_owned(),
            },
            12,
        );
        let json = serde_json::to_value(request).unwrap();

        assert_eq!(json["networkVolumeId"], "volume-1");
        assert_eq!(json["volumeMountPath"], "/root/.ollama");
        assert_eq!(json["env"]["OLLAMA_MODELS"], "/root/.ollama/models");
        assert_eq!(json["env"]["OLLAMA_KEEP_ALIVE"], "-1");
        assert_eq!(json["gpuTypePriority"], "custom");
        assert_eq!(json["dataCenterIds"], serde_json::json!(["EU-RO-1"]));
        assert_eq!(json["cloudType"], "SECURE");
    }

    #[test]
    fn pod_cost_accepts_runpod_numeric_strings() {
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "id": "pod-1",
            "desiredStatus": "RUNNING",
            "costPerHr": "0.34",
            "adjustedCostPerHr": 0.31,
            "gpu": {
                "id": "NVIDIA RTX PRO 4000 Blackwell",
                "displayName": "RTX PRO 4000"
            },
            "machine": {
                "gpuTypeId": "NVIDIA RTX PRO 4000 Blackwell",
                "gpuDisplayName": "RTX PRO 4000",
                "dataCenterId": "EU-RO-1"
            }
        }))
        .unwrap();

        assert!(pod.is_running());
        assert_eq!(pod.effective_cost_per_hr(), Some(0.31));
        assert_eq!(pod.allocated_gpu(), Some("RTX PRO 4000"));
        assert_eq!(pod.data_center_id(), Some("EU-RO-1"));
    }

    #[test]
    fn extracts_structured_api_errors() {
        assert_eq!(
            error_message(r#"{"error":{"message":"insufficient funds"}}"#).as_deref(),
            Some("insufficient funds")
        );
    }

    #[test]
    fn test_client_can_override_its_base_url() {
        let client = RunpodClient::new("test")
            .unwrap()
            .with_base_url("http://127.0.0.1");
        assert_eq!(client.base_url.as_ref(), "http://127.0.0.1");
    }

    #[test]
    fn pod_lookup_requests_allocation_metadata() {
        assert_eq!(
            pod_detail_path("pod-1"),
            "/pods/pod-1?includeMachine=true&includeNetworkVolume=true"
        );
    }
}
