use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{lifecycle::StopReason, state::SessionView};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistoryEntry {
    pub preset_id: String,
    pub model_label: String,
    pub started_at_epoch_ms: u64,
    pub duration_seconds: u64,
    pub final_cost_eur: f64,
    pub stop_reason: String,
}

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid session history: {source}")]
    Invalid {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub fn recent(path: &Path, limit: usize) -> Result<Vec<SessionHistoryEntry>, HistoryError> {
    let mut entries = read(path)?;
    let start = entries.len().saturating_sub(limit);
    Ok(entries.drain(start..).rev().collect())
}

pub fn append(
    path: &Path,
    session: &SessionView,
    duration_seconds: u64,
    final_cost_eur: f64,
    reason: StopReason,
) -> Result<(), HistoryError> {
    let mut entries = read(path)?;
    entries.push(SessionHistoryEntry {
        preset_id: session.session.preset_id.clone(),
        model_label: session.session.model_label.clone(),
        started_at_epoch_ms: session.session.started_at_epoch_ms,
        duration_seconds,
        final_cost_eur,
        stop_reason: reason.to_string(),
    });
    write(path, &entries)
}

fn read(path: &Path) -> Result<Vec<SessionHistoryEntry>, HistoryError> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).map_err(|source| HistoryError::Invalid {
            path: path.to_owned(),
            source,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(HistoryError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn write(path: &Path, entries: &[SessionHistoryEntry]) -> Result<(), HistoryError> {
    let mut contents =
        serde_json::to_vec_pretty(entries).expect("session history always serializes");
    contents.push(b'\n');
    let mut file = AtomicWriteFile::open(path).map_err(|source| HistoryError::Write {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(&contents)
        .and_then(|_| file.sync_all())
        .map_err(|source| HistoryError::Write {
            path: path.to_owned(),
            source,
        })?;
    file.commit().map_err(|source| HistoryError::Write {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn recent_history_returns_newest_entries_first() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("podpilot-history-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session-history.json");
        let entries = (0..7)
            .map(|index| SessionHistoryEntry {
                preset_id: format!("preset-{index}"),
                model_label: format!("Model {index}"),
                started_at_epoch_ms: index,
                duration_seconds: 60,
                final_cost_eur: 0.01,
                stop_reason: "manual".to_owned(),
            })
            .collect::<Vec<_>>();
        write(&path, &entries).unwrap();

        let recent = recent(&path, 5).unwrap();

        assert_eq!(recent.len(), 5);
        assert_eq!(recent[0].preset_id, "preset-6");
        assert_eq!(recent[4].preset_id, "preset-2");

        fs::remove_dir_all(directory).unwrap();
    }
}
