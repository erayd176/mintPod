use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{presets::Preset, settings::IntegrationPreferences};

const PI_PROVIDER_ID: &str = "mintpod";
const LEGACY_PI_PROVIDER_ID: &str = "podpilot";
const OPENCODE_PROVIDER_ID: &str = "mintpod";

pub struct HarnessConnection<'a> {
    pub url: &'a str,
    pub api_key: &'a str,
    pub preset: &'a Preset,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IntegrationStatus {
    Active,
    CommandReady,
    NotInstalled,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationReceipt {
    pub id: &'static str,
    pub name: &'static str,
    pub status: IntegrationStatus,
    pub command: Option<String>,
    pub config_path: Option<PathBuf>,
    pub error: Option<String>,
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error("could not locate the configuration directory for {0}")]
    ConfigDirectoryUnavailable(&'static str),
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
}

pub struct IntegrationManager;

impl IntegrationManager {
    pub fn publish(
        connection: &HarnessConnection<'_>,
        preferences: &IntegrationPreferences,
    ) -> Vec<IntegrationReceipt> {
        let mut receipts = Vec::with_capacity(3);
        if preferences.pi {
            receipts.push(configured_receipt("pi", "Pi", "pi", || {
                PiAdapter::system().and_then(|adapter| adapter.publish(connection))
            }));
        }
        if preferences.opencode {
            receipts.push(configured_receipt(
                "opencode",
                "OpenCode",
                "opencode",
                || OpenCodeAdapter::system().and_then(|adapter| adapter.publish(connection)),
            ));
        }
        if preferences.aider {
            receipts.push(aider_receipt(connection));
        }
        receipts
    }

    pub fn unpublish(preferences: &IntegrationPreferences) -> Vec<String> {
        let mut errors = Vec::new();
        if preferences.pi
            && let Err(error) = PiAdapter::system().and_then(|adapter| adapter.unpublish())
        {
            errors.push(error.to_string());
        }
        if preferences.opencode
            && let Err(error) = OpenCodeAdapter::system().and_then(|adapter| adapter.unpublish())
        {
            errors.push(error.to_string());
        }
        errors
    }
}

fn configured_receipt<F>(
    id: &'static str,
    name: &'static str,
    binary: &str,
    publish: F,
) -> IntegrationReceipt
where
    F: FnOnce() -> Result<(String, PathBuf), HarnessError>,
{
    if !command_exists(binary) {
        return IntegrationReceipt {
            id,
            name,
            status: IntegrationStatus::NotInstalled,
            command: None,
            config_path: None,
            error: None,
        };
    }
    match publish() {
        Ok((command, config_path)) => IntegrationReceipt {
            id,
            name,
            status: IntegrationStatus::Active,
            command: Some(command),
            config_path: Some(config_path),
            error: None,
        },
        Err(error) => IntegrationReceipt {
            id,
            name,
            status: IntegrationStatus::Error,
            command: None,
            config_path: None,
            error: Some(error.to_string()),
        },
    }
}

fn aider_receipt(connection: &HarnessConnection<'_>) -> IntegrationReceipt {
    if !command_exists("aider") {
        return IntegrationReceipt {
            id: "aider",
            name: "Aider",
            status: IntegrationStatus::NotInstalled,
            command: None,
            config_path: None,
            error: None,
        };
    }
    IntegrationReceipt {
        id: "aider",
        name: "Aider",
        status: IntegrationStatus::CommandReady,
        command: Some(aider_command(connection)),
        config_path: None,
        error: None,
    }
}

struct PiAdapter {
    path: PathBuf,
}

impl PiAdapter {
    fn system() -> Result<Self, HarnessError> {
        let home = dirs::home_dir().ok_or(HarnessError::ConfigDirectoryUnavailable("Pi"))?;
        Ok(Self {
            path: home.join(".pi").join("agent").join("models.json"),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn publish(
        &self,
        connection: &HarnessConnection<'_>,
    ) -> Result<(String, PathBuf), HarnessError> {
        let mut document = parse_document(&self.path, read_optional(&self.path)?.as_deref())?;
        let root = root_object(&self.path, &mut document)?;
        let providers = object_field(&self.path, root, "providers", "providers")?;
        let legacy = providers.remove(LEGACY_PI_PROVIDER_ID);
        let mut provider = match providers.remove(PI_PROVIDER_ID).or(legacy) {
            Some(Value::Object(provider)) => provider,
            Some(_) => {
                return Err(HarnessError::InvalidShape {
                    path: self.path.clone(),
                    location: "providers.mintpod",
                });
            }
            None => Map::new(),
        };
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
                "name": connection.preset.label,
                "contextWindow": connection.preset.context_length,
                "maxTokens": connection.preset.max_output_tokens
            }]),
        );
        providers.insert(PI_PROVIDER_ID.to_owned(), Value::Object(provider));
        write_json(&self.path, &document)?;
        Ok((
            format!(
                "pi --provider {PI_PROVIDER_ID} --model {}",
                connection.preset.ollama_tag
            ),
            self.path.clone(),
        ))
    }

    fn unpublish(&self) -> Result<(), HarnessError> {
        remove_owned_entry(&self.path, "providers", PI_PROVIDER_ID)?;
        remove_owned_entry(&self.path, "providers", LEGACY_PI_PROVIDER_ID)
    }
}

struct OpenCodeAdapter {
    path: PathBuf,
}

impl OpenCodeAdapter {
    fn system() -> Result<Self, HarnessError> {
        let config =
            dirs::config_dir().ok_or(HarnessError::ConfigDirectoryUnavailable("OpenCode"))?;
        Ok(Self {
            path: config.join("opencode").join("opencode.json"),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    fn publish(
        &self,
        connection: &HarnessConnection<'_>,
    ) -> Result<(String, PathBuf), HarnessError> {
        let mut document = parse_document(&self.path, read_optional(&self.path)?.as_deref())?;
        let root = root_object(&self.path, &mut document)?;
        root.entry("$schema".to_owned())
            .or_insert_with(|| Value::String("https://opencode.ai/config.json".to_owned()));
        let providers = object_field(&self.path, root, "provider", "provider")?;
        providers.insert(
            OPENCODE_PROVIDER_ID.to_owned(),
            json!({
                "npm": "@ai-sdk/openai-compatible",
                "name": "mintPod",
                "options": {
                    "baseURL": format!("{}/v1", connection.url.trim_end_matches('/')),
                    "apiKey": connection.api_key
                },
                "models": {
                    connection.preset.ollama_tag.clone(): {
                        "name": connection.preset.label,
                        "limit": {
                            "context": connection.preset.context_length,
                            "output": connection.preset.max_output_tokens
                        }
                    }
                }
            }),
        );
        write_json(&self.path, &document)?;
        Ok((
            format!(
                "opencode --model {OPENCODE_PROVIDER_ID}/{}",
                connection.preset.ollama_tag
            ),
            self.path.clone(),
        ))
    }

    fn unpublish(&self) -> Result<(), HarnessError> {
        remove_owned_entry(&self.path, "provider", OPENCODE_PROVIDER_ID)
    }
}

fn aider_command(connection: &HarnessConnection<'_>) -> String {
    #[cfg(target_os = "windows")]
    {
        format!(
            "$env:OPENAI_API_BASE='{}/v1'; $env:OPENAI_API_KEY='{}'; aider --model openai/{}",
            connection.url.trim_end_matches('/'),
            connection.api_key,
            connection.preset.ollama_tag
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        format!(
            "OPENAI_API_BASE='{}/v1' OPENAI_API_KEY='{}' aider --model 'openai/{}'",
            connection.url.trim_end_matches('/'),
            connection.api_key,
            connection.preset.ollama_tag
        )
    }
}

fn command_exists(binary: &str) -> bool {
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|directory| {
        if directory.join(binary).is_file() {
            return true;
        }
        #[cfg(target_os = "windows")]
        {
            ["exe", "cmd", "bat"]
                .iter()
                .any(|extension| directory.join(format!("{binary}.{extension}")).is_file())
        }
        #[cfg(not(target_os = "windows"))]
        false
    })
}

fn remove_owned_entry(path: &Path, section: &'static str, id: &str) -> Result<(), HarnessError> {
    let Some(contents) = read_optional(path)? else {
        return Ok(());
    };
    let mut document = parse_document(path, Some(&contents))?;
    let root = root_object(path, &mut document)?;
    let Some(section_value) = root.get_mut(section) else {
        return Ok(());
    };
    let section_object =
        section_value
            .as_object_mut()
            .ok_or_else(|| HarnessError::InvalidShape {
                path: path.to_owned(),
                location: section,
            })?;
    if section_object.remove(id).is_some() {
        write_json(path, &document)?;
    }
    Ok(())
}

fn object_field<'a>(
    path: &Path,
    root: &'a mut Map<String, Value>,
    key: &str,
    location: &'static str,
) -> Result<&'a mut Map<String, Value>, HarnessError> {
    root.entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| HarnessError::InvalidShape {
            path: path.to_owned(),
            location,
        })
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| HarnessError::Write {
            path: parent.to_owned(),
            source,
        })?;
    }
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
    use crate::presets::VerificationStatus;
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
            id: "coder".to_owned(),
            label: "Coder".to_owned(),
            ollama_tag: "coder:latest".to_owned(),
            size_gb: 4.7,
            min_vram_gb: 12,
            gpu_type_ids: vec!["GPU".to_owned()],
            est_cost_per_hr: 0.3,
            tags: vec!["coding".to_owned()],
            context_length: 65_536,
            max_output_tokens: 16_384,
            verification: VerificationStatus::Candidate,
        }
    }

    fn connection<'a>(model: &'a Preset) -> HarnessConnection<'a> {
        HarnessConnection {
            url: crate::proxy::LOCAL_GATEWAY_URL,
            api_key: "secret",
            preset: model,
        }
    }

    #[test]
    fn pi_publish_and_unpublish_preserve_unowned_configuration() {
        let directory = test_dir();
        let path = directory.join("models.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            &path,
            r#"{"customRoot":true,"providers":{"company":{"baseUrl":"https://example"}}}"#,
        )
        .unwrap();
        let adapter = PiAdapter::at(path.clone());
        let model = preset();

        adapter.publish(&connection(&model)).unwrap();
        let published: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(published["customRoot"], true);
        assert_eq!(
            published["providers"]["company"]["baseUrl"],
            "https://example"
        );
        assert_eq!(
            published["providers"]["mintpod"]["models"][0]["contextWindow"],
            65_536
        );

        adapter.unpublish().unwrap();
        let unpublished: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(unpublished["providers"].get("mintpod").is_none());
        assert_eq!(
            unpublished["providers"]["company"]["baseUrl"],
            "https://example"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn opencode_publish_uses_the_stable_provider_schema() {
        let directory = test_dir();
        let path = directory.join("opencode.json");
        let adapter = OpenCodeAdapter::at(path.clone());
        let model = preset();

        adapter.publish(&connection(&model)).unwrap();
        let document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        assert_eq!(
            document["provider"]["mintpod"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            document["provider"]["mintpod"]["options"]["baseURL"],
            "http://127.0.0.1:11435/v1"
        );
        assert_eq!(
            document["provider"]["mintpod"]["models"]["coder:latest"]["limit"]["output"],
            16_384
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_existing_configuration_is_not_overwritten() {
        let directory = test_dir();
        let path = directory.join("opencode.json");
        fs::create_dir_all(&directory).unwrap();
        fs::write(&path, "not json").unwrap();
        let adapter = OpenCodeAdapter::at(path.clone());
        let model = preset();

        assert!(adapter.publish(&connection(&model)).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
        fs::remove_dir_all(directory).unwrap();
    }
}
