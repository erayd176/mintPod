use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuTierView {
    pub id: &'static str,
    pub label: &'static str,
    pub gpu_type_ids: &'static [&'static str],
    pub est_cost_per_hr: f64,
}

pub const GPU_TIERS: &[GpuTierView] = &[
    GpuTierView {
        id: "economy",
        label: "Economy",
        gpu_type_ids: &[
            "NVIDIA GeForce RTX 3070",
            "NVIDIA GeForce RTX 3080",
            "NVIDIA RTX A4000",
            "NVIDIA RTX PRO 4000 Blackwell",
        ],
        est_cost_per_hr: 0.18,
    },
    GpuTierView {
        id: "balanced",
        label: "Balanced",
        gpu_type_ids: &[
            "NVIDIA GeForce RTX 3080 Ti",
            "NVIDIA GeForce RTX 4070 Ti",
            "NVIDIA RTX A4000",
            "NVIDIA GeForce RTX 4080",
            "NVIDIA RTX PRO 4000 Blackwell",
            "NVIDIA RTX PRO 4500 Blackwell",
        ],
        est_cost_per_hr: 0.22,
    },
    GpuTierView {
        id: "quality",
        label: "Quality",
        gpu_type_ids: &[
            "NVIDIA GeForce RTX 4090",
            "NVIDIA RTX A5000",
            "NVIDIA GeForce RTX 3090",
            "NVIDIA RTX PRO 4000 Blackwell",
            "NVIDIA RTX PRO 4500 Blackwell",
            "NVIDIA A40",
        ],
        est_cost_per_hr: 0.44,
    },
];

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
    #[error("could not write {path}: {message}")]
    WriteFile { path: PathBuf, message: String },
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

    pub fn add_user_preset(
        &mut self,
        user_file: &Path,
        mut preset: Preset,
    ) -> Result<PresetView, PresetError> {
        preset.id = self.available_custom_id(&preset.ollama_tag);
        let schema: Value = serde_json::from_str(SCHEMA).map_err(|error| {
            PresetError::InvalidSchema(format!("schema is not valid JSON: {error}"))
        })?;
        let validator = jsonschema::validator_for(&schema)
            .map_err(|error| PresetError::InvalidSchema(error.to_string()))?;
        let value = serde_json::to_value(&preset).expect("a preset always serializes");
        let preset = parse_value("new custom preset", value, &validator)?;
        let view = PresetView {
            preset,
            user_defined: true,
        };
        let mut user_presets: Vec<&Preset> = self
            .entries
            .iter()
            .filter(|entry| entry.user_defined)
            .map(|entry| &entry.preset)
            .collect();
        user_presets.push(&view.preset);
        write_user_presets(user_file, &user_presets)?;
        self.entries.push(view.clone());
        Ok(view)
    }

    fn available_custom_id(&self, ollama_tag: &str) -> String {
        let slug = ollama_tag
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .split('-')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let base = format!("custom-{slug}")
            .chars()
            .take(60)
            .collect::<String>()
            .trim_end_matches('-')
            .to_owned();
        let mut candidate = base.clone();
        let mut suffix = 2;
        while self
            .entries
            .iter()
            .any(|entry| entry.preset.id == candidate)
        {
            candidate = format!("{base}-{suffix}");
            suffix += 1;
        }
        candidate
    }
}

pub fn verified_gpu_tier(gpu_type_ids: &[String]) -> Option<&'static GpuTierView> {
    GPU_TIERS.iter().find(|tier| {
        tier.gpu_type_ids
            .iter()
            .copied()
            .eq(gpu_type_ids.iter().map(String::as_str))
    })
}

fn write_user_presets(path: &Path, presets: &[&Preset]) -> Result<(), PresetError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| PresetError::WriteFile {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    }
    let mut contents =
        serde_json::to_vec_pretty(presets).expect("a preset list always serializes successfully");
    contents.push(b'\n');
    let mut file = AtomicWriteFile::open(path).map_err(|error| PresetError::WriteFile {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    file.write_all(&contents)
        .and_then(|_| file.sync_all())
        .map_err(|error| PresetError::WriteFile {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    file.commit().map_err(|error| PresetError::WriteFile {
        path: path.to_owned(),
        message: error.to_string(),
    })
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
            "mintpod-presets-{}-missing.json",
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

    #[test]
    fn verified_gpu_tiers_require_the_exact_ranked_list() {
        let balanced = GPU_TIERS[1]
            .gpu_type_ids
            .iter()
            .map(|id| (*id).to_owned())
            .collect::<Vec<_>>();
        let mut reordered = balanced.clone();
        reordered.reverse();

        assert_eq!(
            verified_gpu_tier(&balanced).map(|tier| tier.id),
            Some("balanced")
        );
        assert!(verified_gpu_tier(&reordered).is_none());
    }

    #[test]
    fn custom_presets_are_appended_without_clobbering_existing_entries() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("mintpod-presets-{}-{nonce}", std::process::id(),));
        fs::create_dir_all(&directory).unwrap();
        let user_file = directory.join("presets.user.json");
        let mut catalog = PresetCatalog::load(&user_file).unwrap();
        let custom = Preset {
            id: String::new(),
            label: "example:7b".to_owned(),
            ollama_tag: "example:7b".to_owned(),
            size_gb: 4.0,
            min_vram_gb: 8,
            gpu_type_ids: GPU_TIERS[0]
                .gpu_type_ids
                .iter()
                .map(|id| (*id).to_owned())
                .collect(),
            est_cost_per_hr: GPU_TIERS[0].est_cost_per_hr,
            tags: vec!["coding".to_owned(), "custom".to_owned()],
        };

        let first = catalog.add_user_preset(&user_file, custom.clone()).unwrap();
        let second = catalog.add_user_preset(&user_file, custom).unwrap();
        let reloaded = PresetCatalog::load(&user_file).unwrap();

        assert_eq!(first.preset.id, "custom-example-7b");
        assert_eq!(second.preset.id, "custom-example-7b-2");
        assert_eq!(
            reloaded
                .list()
                .iter()
                .filter(|preset| preset.user_defined)
                .count(),
            2
        );

        fs::remove_dir_all(directory).unwrap();
    }
}
