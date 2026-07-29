#![cfg_attr(not(feature = "desktop-runtime"), allow(dead_code))]

#[cfg(feature = "desktop-runtime")]
mod commands;
mod credentials;
mod ollama;
mod orchestrator;
mod presets;
mod runpod;
mod settings;
mod state;

#[cfg(feature = "desktop-runtime")]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri::Manager;

    tauri::Builder::default()
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&config_dir)?;
            app.manage(state::AppState::load(config_dir)?);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_presets,
            commands::get_settings,
            commands::set_storage_region,
            commands::api_key_status,
            commands::save_api_key,
            commands::remove_api_key,
            commands::launch_preset
        ])
        .run(tauri::generate_context!())
        .expect("failed to run PodPilot");
}
