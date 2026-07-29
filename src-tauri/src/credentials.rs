use keyring::v1::{Entry, Error as KeyringError};
use thiserror::Error;

const SERVICE: &str = "dev.podpilot.app";
const USER: &str = "runpod-api-key";

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("the operating-system keychain is unavailable: {0}")]
    Keychain(String),
}

pub struct CredentialStore;

impl CredentialStore {
    pub fn contains_key() -> Result<bool, CredentialError> {
        match Self::entry()?.get_password() {
            Ok(_) => Ok(true),
            Err(KeyringError::NoEntry) => Ok(false),
            Err(error) => Err(CredentialError::Keychain(error.to_string())),
        }
    }

    pub fn read_key() -> Result<Option<String>, CredentialError> {
        match Self::entry()?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(CredentialError::Keychain(error.to_string())),
        }
    }

    pub fn write_key(key: &str) -> Result<(), CredentialError> {
        Self::entry()?
            .set_password(key)
            .map_err(|error| CredentialError::Keychain(error.to_string()))
    }

    pub fn delete_key() -> Result<(), CredentialError> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => Err(CredentialError::Keychain(error.to_string())),
        }
    }

    fn entry() -> Result<Entry, CredentialError> {
        Entry::new(SERVICE, USER).map_err(|error| CredentialError::Keychain(error.to_string()))
    }
}
