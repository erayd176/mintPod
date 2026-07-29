use crate::{credentials::CredentialStore, runpod::RunpodClient};

#[tauri::command]
pub async fn api_key_status() -> Result<bool, String> {
    CredentialStore::contains_key().map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn save_api_key(api_key: String) -> Result<(), String> {
    let api_key = api_key.trim();
    let client = RunpodClient::new(api_key).map_err(|error| error.to_string())?;
    client
        .validate_key()
        .await
        .map_err(|error| error.to_string())?;
    CredentialStore::write_key(api_key).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn remove_api_key() -> Result<(), String> {
    CredentialStore::delete_key().map_err(|error| error.to_string())
}
