use std::{
    path::PathBuf,
    sync::{PoisonError, RwLock, RwLockReadGuard},
};

use crate::presets::{PresetCatalog, PresetError};

pub struct AppState {
    presets: RwLock<PresetCatalog>,
    pub user_presets_path: PathBuf,
}

impl AppState {
    pub fn load(config_dir: PathBuf) -> Result<Self, PresetError> {
        let user_presets_path = config_dir.join("presets.user.json");
        let presets = PresetCatalog::load(&user_presets_path)?;
        Ok(Self {
            presets: RwLock::new(presets),
            user_presets_path,
        })
    }

    pub fn presets(
        &self,
    ) -> Result<RwLockReadGuard<'_, PresetCatalog>, PoisonError<RwLockReadGuard<'_, PresetCatalog>>>
    {
        self.presets.read()
    }
}
