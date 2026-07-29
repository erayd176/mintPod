use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Error)]
pub enum OllamaError {
    #[error("Ollama request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Ollama rejected the request ({status}): {message}")]
    Api { status: StatusCode, message: String },
    #[error("Ollama returned malformed pull progress: {0}")]
    InvalidProgress(String),
    #[error("Ollama did not become reachable in time")]
    HealthTimeout,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PullProgress {
    pub status: String,
    pub completed_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

#[derive(Clone)]
pub struct OllamaClient {
    http: Client,
    base_url: String,
}

impl OllamaClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self, OllamaError> {
        let http = Client::builder()
            .user_agent(concat!("mintPod/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        })
    }

    pub async fn wait_until_healthy(&self, attempts: u16) -> Result<(), OllamaError> {
        for _ in 0..attempts {
            let response = self
                .http
                .get(format!("{}/api/version", self.base_url))
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            if response.is_ok_and(|response| response.status().is_success()) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        Err(OllamaError::HealthTimeout)
    }

    pub async fn has_model(&self, model: &str) -> Result<bool, OllamaError> {
        let response = self
            .http
            .get(format!("{}/api/tags", self.base_url))
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        let response = ensure_success(response).await?;
        let tags: TagsResponse = response.json().await?;
        Ok(tags.models.iter().any(|entry| entry.name == model))
    }

    pub async fn pull_model(
        &self,
        model: &str,
        progress: UnboundedSender<PullProgress>,
    ) -> Result<(), OllamaError> {
        for _ in 0..120 {
            match self.pull_once(model, &progress).await {
                Ok(()) => return Ok(()),
                Err(error) if is_resumable_pull_error(&error) => {
                    if self.has_model(model).await.unwrap_or(false) {
                        return Ok(());
                    }
                    let _ = progress.send(PullProgress {
                        status: "Resuming model pull".to_owned(),
                        completed_bytes: None,
                        total_bytes: None,
                    });
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(OllamaError::InvalidProgress(
            "model pull exceeded the retry limit".to_owned(),
        ))
    }

    async fn pull_once(
        &self,
        model: &str,
        progress: &UnboundedSender<PullProgress>,
    ) -> Result<(), OllamaError> {
        let response = self
            .http
            .post(format!("{}/api/pull", self.base_url))
            .json(&serde_json::json!({ "model": model, "stream": true }))
            .send()
            .await?;
        let response = ensure_success(response).await?;
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut completed = false;

        while let Some(chunk) = stream.next().await {
            buffer.extend_from_slice(&chunk?);
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line = buffer.drain(..=newline).collect::<Vec<_>>();
                if let Some(update) = parse_pull_line(&line)? {
                    completed |= update.status == "success";
                    let _ = progress.send(update.into());
                }
            }
        }

        if !buffer.is_empty()
            && let Some(update) = parse_pull_line(&buffer)?
        {
            completed |= update.status == "success";
            let _ = progress.send(update.into());
        }

        if completed {
            Ok(())
        } else {
            Err(OllamaError::InvalidProgress(
                "stream ended before Ollama reported success".to_owned(),
            ))
        }
    }

    pub async fn warm_model(&self, model: &str) -> Result<(), OllamaError> {
        let response = self
            .http
            .post(format!("{}/api/generate", self.base_url))
            .json(&serde_json::json!({
                "model": model,
                "keep_alive": -1,
                "stream": false
            }))
            .send()
            .await?;
        ensure_success(response).await.map(|_| ())
    }
}

fn is_resumable_pull_error(error: &OllamaError) -> bool {
    match error {
        OllamaError::Transport(_) => true,
        OllamaError::Api { status, .. } => matches!(status.as_u16(), 502 | 503 | 504 | 524),
        OllamaError::InvalidProgress(message) => {
            message == "stream ended before Ollama reported success"
        }
        OllamaError::HealthTimeout => false,
    }
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Debug, Deserialize)]
struct TagEntry {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PullUpdate {
    status: String,
    #[serde(default)]
    completed: Option<u64>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

impl From<PullUpdate> for PullProgress {
    fn from(update: PullUpdate) -> Self {
        Self {
            status: update.status,
            completed_bytes: update.completed,
            total_bytes: update.total,
        }
    }
}

fn parse_pull_line(line: &[u8]) -> Result<Option<PullUpdate>, OllamaError> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim();
    if line.is_empty() {
        return Ok(None);
    }

    let update: PullUpdate = serde_json::from_str(line)
        .map_err(|error| OllamaError::InvalidProgress(error.to_string()))?;
    if let Some(error) = update.error.as_ref() {
        return Err(OllamaError::InvalidProgress(error.clone()));
    }
    Ok(Some(update))
}

async fn ensure_success(response: reqwest::Response) -> Result<reqwest::Response, OllamaError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let message = response.text().await.unwrap_or_default();
    Err(OllamaError::Api { status, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_byte_progress() {
        let update = parse_pull_line(
            br#"{"status":"downloading digest","digest":"sha256:x","total":1000,"completed":640}"#,
        )
        .unwrap()
        .unwrap();
        let progress = PullProgress::from(update);

        assert_eq!(progress.completed_bytes, Some(640));
        assert_eq!(progress.total_bytes, Some(1000));
    }

    #[test]
    fn surfaces_pull_errors() {
        let error =
            parse_pull_line(br#"{"status":"pulling","error":"manifest not found"}"#).unwrap_err();

        assert!(error.to_string().contains("manifest not found"));
    }
}
