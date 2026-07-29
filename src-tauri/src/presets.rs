use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const SCHEMA: &str = include_str!("../../presets/schema.json");
const CURATED: &[(&str, &str)] = &[
    (
        "qwen-coder-3b.json",
        include_str!("../../presets/qwen-coder-3b.json"),
    ),
    (
        "qwen-coder-7b.json",
        include_str!("../../presets/qwen-coder-7b.json"),
    ),
    (
        "ministral-8b.json",
        include_str!("../../presets/ministral-8b.json"),
    ),
    (
        "qwen-coder-14b.json",
        include_str!("../../presets/qwen-coder-14b.json"),
    ),
    (
        "devstral-24b.json",
        include_str!("../../presets/devstral-24b.json"),
    ),
];

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Preset {
    pub id: String,
    pub label: String,
    pub ollama_tag: String,
    pub size_gb: f64,
    pub min_vram_gb: u16,
    pub gpu_type_ids: Vec<String>,
    pub est_cost_per_hr: f64,
    pub tags: Vec<String>,
}

impl Preset {
    pub fn volume_size_gb(&self) -> u16 {
        (self.size_gb * 1.25 + 2.0).ceil().max(10.0) as u16
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresetView {
    #[serde(flatten)]
    pub preset: Preset,
    pub user_defined: bool,
}

#[derive(Debug, Error)]
pub enum PresetError {
    #[error("preset schema is invalid: {0}")]
    InvalidSchema(String),
    #[error("preset {source_name} is not valid JSON: {message}")]
    InvalidJson {
        source_name: String,
        message: String,
    },
    #[error("preset {source_name} violates the schema at {path}: {message}")]
    SchemaViolation {
        source_name: String,
        path: String,
        message: String,
    },
    #[error("preset {source_name} has an invalid field: {message}")]
    InvalidField {
        source_name: String,
        message: String,
    },
    #[error("duplicate preset id: {0}")]
    DuplicateId(String),
    #[error("could not read {path}: {message}")]
    ReadFile { path: PathBuf, message: String },
    #[error("presets.user.json must contain a JSON array")]
    UserFileNotArray,
}

#[derive(Debug)]
pub struct PresetCatalog {
    entries: Vec<PresetView>,
}

impl PresetCatalog {
    pub fn load(user_file: &Path) -> Result<Self, PresetError> {
        let schema: Value = serde_json::from_str(SCHEMA).map_err(|error| {
            PresetError::InvalidSchema(format!("schema is not valid JSON: {error}"))
        })?;
        jsonschema::meta::validate(&schema)
            .map_err(|error| PresetError::InvalidSchema(error.to_string()))?;
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| PresetError::InvalidSchema(error.to_string()))?;

        let mut entries = Vec::with_capacity(CURATED.len());
        for (name, contents) in CURATED {
            let preset = parse_preset(name, contents, &validator)?;
            if preset.size_gb > 16.0 {
                return Err(PresetError::InvalidField {
                    source_name: (*name).to_owned(),
                    message: "curated presets must not exceed 16 GB".to_owned(),
                });
            }
            entries.push(PresetView {
                preset,
                user_defined: false,
            });
        }

        if user_file.exists() {
            let contents =
                fs::read_to_string(user_file).map_err(|error| PresetError::ReadFile {
                    path: user_file.to_owned(),
                    message: error.to_string(),
                })?;
            let values: Value =
                serde_json::from_str(&contents).map_err(|error| PresetError::InvalidJson {
                    source_name: user_file.display().to_string(),
                    message: error.to_string(),
                })?;
            let values = values.as_array().ok_or(PresetError::UserFileNotArray)?;

            for (index, value) in values.iter().enumerate() {
                let name = format!("{}[{index}]", user_file.display());
                let preset = parse_value(&name, value.clone(), &validator)?;
                entries.push(PresetView {
                    preset,
                    user_defined: true,
                });
            }
        }

        ensure_unique_ids(&entries)?;
        Ok(Self { entries })
    }

    pub fn list(&self) -> Vec<PresetView> {
        self.entries.clone()
    }

    pub fn find(&self, id: &str) -> Option<Preset> {
        self.entries
            .iter()
            .find(|entry| entry.preset.id == id)
            .map(|entry| entry.preset.clone())
    }
}

fn parse_preset(
    source_name: &str,
    contents: &str,
    validator: &jsonschema::Validator,
) -> Result<Preset, PresetError> {
    let value = serde_json::from_str(contents).map_err(|error| PresetError::InvalidJson {
        source_name: source_name.to_owned(),
        message: error.to_string(),
    })?;
    parse_value(source_name, value, validator)
}

fn parse_value(
    source_name: &str,
    value: Value,
    validator: &jsonschema::Validator,
) -> Result<Preset, PresetError> {
    if let Err(error) = validator.validate(&value) {
        return Err(PresetError::SchemaViolation {
            source_name: source_name.to_owned(),
            path: error.instance_path().as_str().to_owned(),
            message: error.to_string(),
        });
    }

    serde_json::from_value(value).map_err(|error| PresetError::InvalidField {
        source_name: source_name.to_owned(),
        message: error.to_string(),
    })
}

fn ensure_unique_ids(entries: &[PresetView]) -> Result<(), PresetError> {
    let mut ids = HashSet::with_capacity(entries.len());
    for entry in entries {
        if !ids.insert(entry.preset.id.as_str()) {
            return Err(PresetError::DuplicateId(entry.preset.id.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing_user_file() -> PathBuf {
        std::env::temp_dir().join(format!(
            "podpilot-presets-{}-missing.json",
            std::process::id()
        ))
    }

    #[test]
    fn curated_presets_satisfy_the_shipped_schema() {
        let catalog = PresetCatalog::load(&missing_user_file()).unwrap();

        assert_eq!(catalog.list().len(), 5);
        assert!(catalog.list().iter().all(|entry| !entry.user_defined));
        assert!(
            catalog
                .list()
                .iter()
                .all(|entry| entry.preset.size_gb <= 16.0)
        );
    }

    #[test]
    fn model_volume_includes_headroom() {
        let catalog = PresetCatalog::load(&missing_user_file()).unwrap();
        let preset = catalog.find("qwen-coder-14b").unwrap();

        assert_eq!(preset.volume_size_gb(), 14);
    }

    #[test]
    fn schema_rejects_unknown_fields() {
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let value = serde_json::json!({
            "id": "test-model",
            "label": "Test",
            "ollamaTag": "test:latest",
            "sizeGb": 1,
            "minVramGb": 8,
            "gpuTypeIds": ["NVIDIA GeForce RTX 3070"],
            "estCostPerHr": 0.1,
            "tags": ["coding"],
            "surprise": true
        });

        assert!(validator.validate(&value).is_err());
    }
}
