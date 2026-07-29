#![cfg_attr(not(feature = "desktop-runtime"), allow(dead_code))]

#[cfg(feature = "desktop-runtime")]
mod commands;
mod credentials;
mod runpod;

#[cfg(feature = "desktop-runtime")]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::api_key_status,
            commands::save_api_key,
            commands::remove_api_key
        ])
        .run(tauri::generate_context!())
        .expect("failed to run PodPilot");
}
