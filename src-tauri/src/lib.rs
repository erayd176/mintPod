#![cfg_attr(not(feature = "desktop-runtime"), allow(dead_code))]

#[cfg(feature = "desktop-runtime")]
mod commands;
mod credentials;
mod fx;
mod harness;
mod history;
mod journal;
mod lifecycle;
mod ollama;
mod orchestrator;
mod presets;
mod proxy;
mod runpod;
mod settings;
#[cfg(feature = "desktop-runtime")]
mod startup;
mod state;

#[cfg(feature = "desktop-runtime")]
pub(crate) fn migrate_legacy_config(config_dir: &std::path::Path) -> Result<(), std::io::Error> {
    let Some(base) = dirs::config_dir() else {
        return Ok(());
    };
    let legacy_dir = base.join("dev.podpilot.desktop");
    if !legacy_dir.is_dir() {
        return Ok(());
    }

    std::fs::create_dir_all(config_dir)?;
    for name in [
        "api-keys.json",
        "settings.json",
        "presets.user.json",
        "session-history.json",
        "fx-rate.json",
    ] {
        let source = legacy_dir.join(name);
        let destination = config_dir.join(name);
        if source.is_file() && !destination.exists() {
            std::fs::copy(source, destination)?;
        }
    }
    Ok(())
}

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
            // Startup failures become a blocked screen rather than a panic: a
            // locked keychain, a taken port, or a hand-edited local document
            // must never leave the user with no window and no explanation.
            let handle = app.handle().clone();
            handle.manage(startup::StartupState::new());
            startup::initialize(&handle);
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
            commands::preflight_preset,
            commands::add_custom_preset,
            commands::list_cached_models,
            commands::delete_cached_model,
            commands::session_history,
            commands::diagnostics,
            commands::connection_details,
            commands::get_settings,
            commands::set_storage_region,
            commands::set_idle_timeout,
            commands::set_integration_enabled,
            commands::list_api_keys,
            commands::add_api_key,
            commands::replace_api_key,
            commands::select_api_key,
            commands::remove_api_key,
            commands::launch_preset,
            commands::cancel_launch,
            commands::stop_session,
            commands::recovery_status,
            commands::cleanup_recovery,
            commands::recover_session,
            commands::startup_status,
            commands::retry_startup,
            commands::reset_local_config
        ])
        .run(tauri::generate_context!())
        .expect("failed to run mintPod");
}
