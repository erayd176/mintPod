use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::presets::Preset;

pub const LOCAL_PROXY_URL: &str = "http://127.0.0.1:8080";
const PI_PROVIDER_ID: &str = "mintpod";
const LEGACY_PI_PROVIDER_ID: &str = "podpilot";

pub struct HarnessConnection<'a> {
    pub url: &'a str,
    pub api_key: &'a str,
    pub preset: &'a Preset,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WiringReceipt {
    pub harness: &'static str,
    pub command: String,
    pub config_path: PathBuf,
}

pub trait HarnessAdapter: Send + Sync {
    fn wire(&self, connection: &HarnessConnection<'_>) -> Result<WiringReceipt, HarnessError>;
}

pub struct PiAdapter {
    agent_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("could not locate the home directory for Pi configuration")]
    HomeDirectoryUnavailable,
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} must contain a JSON object at {location}")]
    InvalidShape {
        path: PathBuf,
        location: &'static str,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not restore {path} after a failed Pi configuration update: {source}")]
    Rollback {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl PiAdapter {
    pub fn system() -> Result<Self, HarnessError> {
        let home = dirs::home_dir().ok_or(HarnessError::HomeDirectoryUnavailable)?;
        Ok(Self {
            agent_dir: home.join(".pi").join("agent"),
        })
    }

    #[cfg(test)]
    fn at(agent_dir: PathBuf) -> Self {
        Self { agent_dir }
    }

    fn legacy_path(&self) -> PathBuf {
        self.agent_dir.join("local-models.json")
    }

    fn current_path(&self) -> PathBuf {
        self.agent_dir.join("models.json")
    }
}

impl HarnessAdapter for PiAdapter {
    fn wire(&self, connection: &HarnessConnection<'_>) -> Result<WiringReceipt, HarnessError> {
        fs::create_dir_all(&self.agent_dir).map_err(|source| HarnessError::Write {
            path: self.agent_dir.clone(),
            source,
        })?;

        let legacy_path = self.legacy_path();
        let current_path = self.current_path();
        let legacy_original = read_optional(&legacy_path)?;
        let mut legacy = parse_document(&legacy_path, legacy_original.as_deref())?;
        let legacy_root = root_object(&legacy_path, &mut legacy)?;
        legacy_root.insert("url".to_owned(), Value::String(connection.url.to_owned()));
        legacy_root.insert(
            "apiKey".to_owned(),
            Value::String(connection.api_key.to_owned()),
        );

        let mut current = parse_document(&current_path, read_optional(&current_path)?.as_deref())?;
        merge_current_pi_config(&current_path, &mut current, connection)?;

        write_json(&legacy_path, &legacy)?;
        if let Err(error) = write_json(&current_path, &current) {
            restore(&legacy_path, legacy_original.as_deref()).map_err(|source| {
                HarnessError::Rollback {
                    path: legacy_path.clone(),
                    source,
                }
            })?;
            return Err(error);
        }

        Ok(WiringReceipt {
            harness: "Pi",
            command: format!(
                "pi --provider {PI_PROVIDER_ID} --model {}",
                connection.preset.ollama_tag
            ),
            config_path: current_path,
        })
    }
}

fn merge_current_pi_config(
    path: &Path,
    document: &mut Value,
    connection: &HarnessConnection<'_>,
) -> Result<(), HarnessError> {
    let root = root_object(path, document)?;
    if !root.contains_key("providers") {
        root.insert("providers".to_owned(), Value::Object(Map::new()));
    }
    let providers = root
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| HarnessError::InvalidShape {
            path: path.to_owned(),
            location: "providers",
        })?;
    let legacy_provider = providers.remove(LEGACY_PI_PROVIDER_ID);
    if !providers.contains_key(PI_PROVIDER_ID) {
        providers.insert(
            PI_PROVIDER_ID.to_owned(),
            legacy_provider.unwrap_or_else(|| Value::Object(Map::new())),
        );
    } else if let Some(Value::Object(legacy)) = legacy_provider {
        let current = providers
            .get_mut(PI_PROVIDER_ID)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| HarnessError::InvalidShape {
                path: path.to_owned(),
                location: "providers.mintpod",
            })?;
        for (key, value) in legacy {
            current.entry(key).or_insert(value);
        }
    }
    let provider = providers
        .get_mut(PI_PROVIDER_ID)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| HarnessError::InvalidShape {
            path: path.to_owned(),
            location: "providers.mintpod",
        })?;

    provider.insert(
        "baseUrl".to_owned(),
        Value::String(format!("{}/v1", connection.url.trim_end_matches('/'))),
    );
    provider.insert(
        "api".to_owned(),
        Value::String("openai-completions".to_owned()),
    );
    provider.insert(
        "apiKey".to_owned(),
        Value::String(connection.api_key.to_owned()),
    );
    provider.insert("authHeader".to_owned(), Value::Bool(true));
    provider.insert(
        "compat".to_owned(),
        json!({
            "supportsDeveloperRole": false,
            "supportsReasoningEffort": false
        }),
    );
    provider.insert(
        "models".to_owned(),
        json!([{
            "id": connection.preset.ollama_tag,
            "name": connection.preset.label
        }]),
    );
    Ok(())
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, HarnessError> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(HarnessError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn parse_document(path: &Path, contents: Option<&[u8]>) -> Result<Value, HarnessError> {
    match contents {
        Some(contents) => {
            serde_json::from_slice(contents).map_err(|source| HarnessError::InvalidJson {
                path: path.to_owned(),
                source,
            })
        }
        None => Ok(Value::Object(Map::new())),
    }
}

fn root_object<'a>(
    path: &Path,
    document: &'a mut Value,
) -> Result<&'a mut Map<String, Value>, HarnessError> {
    document
        .as_object_mut()
        .ok_or_else(|| HarnessError::InvalidShape {
            path: path.to_owned(),
            location: "root",
        })
}

fn write_json(path: &Path, document: &Value) -> Result<(), HarnessError> {
    let mut contents =
        serde_json::to_vec_pretty(document).expect("a JSON value always serializes successfully");
    contents.push(b'\n');

    let mut file = AtomicWriteFile::open(path).map_err(|source| HarnessError::Write {
        path: path.to_owned(),
        source,
    })?;
    secure_permissions(&file).map_err(|source| HarnessError::Write {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(&contents)
        .and_then(|_| file.sync_all())
        .map_err(|source| HarnessError::Write {
            path: path.to_owned(),
            source,
        })?;
    file.commit().map_err(|source| HarnessError::Write {
        path: path.to_owned(),
        source,
    })
}

fn restore(path: &Path, original: Option<&[u8]>) -> Result<(), std::io::Error> {
    match original {
        Some(contents) => {
            let mut file = AtomicWriteFile::open(path)?;
            secure_permissions(&file)?;
            file.write_all(contents)?;
            file.sync_all()?;
            file.commit()
        }
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

#[cfg(unix)]
fn secure_permissions(file: &std::fs::File) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn secure_permissions(_file: &std::fs::File) -> Result<(), std::io::Error> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mintpod-harness-{}-{nonce}", std::process::id()))
    }

    fn preset() -> Preset {
        Preset {
            id: "coder-7b".to_owned(),
            label: "Qwen Coder".to_owned(),
            ollama_tag: "qwen2.5-coder:7b".to_owned(),
            size_gb: 4.7,
            min_vram_gb: 12,
            gpu_type_ids: vec!["NVIDIA GeForce RTX 4090".to_owned()],
            est_cost_per_hr: 0.3,
            tags: vec!["coding".to_owned()],
        }
    }

    #[test]
    fn pi_merge_preserves_other_providers_and_unknown_fields() {
        let directory = test_dir();
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("models.json"),
            r#"{
                "customRoot": true,
                "providers": {
                    "company": {"baseUrl": "https://internal.example/v1"},
                    "mintpod": {"customHeader": "preserve"}
                }
            }"#,
        )
        .unwrap();
        let adapter = PiAdapter::at(directory.clone());
        let model = preset();

        adapter
            .wire(&HarnessConnection {
                url: LOCAL_PROXY_URL,
                api_key: "secret",
                preset: &model,
            })
            .unwrap();

        let current: Value =
            serde_json::from_slice(&fs::read(directory.join("models.json")).unwrap()).unwrap();
        assert_eq!(current["customRoot"], true);
        assert_eq!(
            current["providers"]["company"]["baseUrl"],
            "https://internal.example/v1"
        );
        assert_eq!(current["providers"]["mintpod"]["customHeader"], "preserve");
        assert_eq!(
            current["providers"]["mintpod"]["baseUrl"],
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            current["providers"]["mintpod"]["models"][0]["id"],
            "qwen2.5-coder:7b"
        );
        let legacy: Value =
            serde_json::from_slice(&fs::read(directory.join("local-models.json")).unwrap())
                .unwrap();
        assert_eq!(legacy["url"], LOCAL_PROXY_URL);
        assert_eq!(legacy["apiKey"], "secret");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pi_merge_migrates_the_legacy_provider_name() {
        let directory = test_dir();
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("models.json"),
            r#"{
                "providers": {
                    "podpilot": {"customHeader": "preserve"}
                }
            }"#,
        )
        .unwrap();
        let adapter = PiAdapter::at(directory.clone());
        let model = preset();

        adapter
            .wire(&HarnessConnection {
                url: LOCAL_PROXY_URL,
                api_key: "secret",
                preset: &model,
            })
            .unwrap();

        let current: Value =
            serde_json::from_slice(&fs::read(directory.join("models.json")).unwrap()).unwrap();
        assert!(current["providers"].get("podpilot").is_none());
        assert_eq!(current["providers"]["mintpod"]["customHeader"], "preserve");
        assert_eq!(
            current["providers"]["mintpod"]["models"][0]["id"],
            "qwen2.5-coder:7b"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_existing_config_is_never_overwritten() {
        let directory = test_dir();
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("models.json");
        fs::write(&path, b"not json").unwrap();
        let adapter = PiAdapter::at(directory.clone());
        let model = preset();

        let result = adapter.wire(&HarnessConnection {
            url: LOCAL_PROXY_URL,
            api_key: "secret",
            preset: &model,
        });

        assert!(matches!(result, Err(HarnessError::InvalidJson { .. })));
        assert_eq!(fs::read(&path).unwrap(), b"not json");
        assert!(!directory.join("local-models.json").exists());

        fs::remove_dir_all(directory).unwrap();
    }
}
