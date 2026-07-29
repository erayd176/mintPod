#![cfg_attr(not(feature = "desktop-runtime"), allow(dead_code))]

#[cfg(feature = "desktop-runtime")]
mod commands;
mod credentials;
mod fx;
mod harness;
mod history;
mod lifecycle;
mod ollama;
mod orchestrator;
mod presets;
mod proxy;
mod runpod;
mod settings;
mod state;

#[cfg(feature = "desktop-runtime")]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tauri::Manager;

    tauri::Builder::default()
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            std::fs::create_dir_all(&config_dir)?;
            app.manage(state::AppState::load(config_dir)?);
            let window = app
                .get_webview_window("main")
                .expect("main window is configured");
            let app_handle = app.handle().clone();
            let shutdown_started = Arc::new(AtomicBool::new(false));
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    if !shutdown_started.swap(true, Ordering::AcqRel) {
                        let task_flag = Arc::clone(&shutdown_started);
                        let task_app = app_handle.clone();
                        tauri::async_runtime::spawn(async move {
                            if !commands::shutdown_for_exit(task_app).await {
                                task_flag.store(false, Ordering::Release);
                            }
                        });
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_presets,
            commands::list_gpu_tiers,
            commands::add_custom_preset,
            commands::list_cached_models,
            commands::delete_cached_model,
            commands::session_history,
            commands::get_settings,
            commands::set_storage_region,
            commands::set_idle_timeout,
            commands::list_api_keys,
            commands::add_api_key,
            commands::replace_api_key,
            commands::select_api_key,
            commands::remove_api_key,
            commands::launch_preset,
            commands::stop_session
        ])
        .run(tauri::generate_context!())
        .expect("failed to run PodPilot");
}
