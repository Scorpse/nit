//! Secret storage. Tokens live only in the OS credential store (Windows Credential Manager,
//! the macOS Keychain, the Secret Service on Linux); they never reach SQLite, config, logs or
//! error text. There is no file or environment fallback: a machine without a credential store
//! (a headless Linux without a Secret Service) gets `SecretError::Unavailable`, and the
//! token check can still read its token from the environment for that one run.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use prcore::model::AccountId;

/// The single Bitbucket sign-in's keys before accounts existed. Only read, to move them to
/// the default account's keys (`app::accounts::migrate_legacy_bitbucket`), and written only
/// by the move from the pre-rename service (`app::brand::migrate_credentials`).
pub const LEGACY_BB_EMAIL: &str = "bitbucket.email";
pub const LEGACY_BB_TOKEN: &str = "bitbucket.token";

/// `account.<id>.token`: the account's token (one OS entry per key, service unchanged).
pub fn token_key(id: &AccountId) -> String {
    format!("account.{}.token", id.as_str())
}

/// `account.<id>.email`: the sign-in email (Bitbucket only).
pub fn email_key(id: &AccountId) -> String {
    format!("account.{}.email", id.as_str())
}

/// `account.<id>.refresh`: an OAuth refresh token (unused until OAuth, decision D3).
pub fn refresh_key(id: &AccountId) -> String {
    format!("account.{}.refresh", id.as_str())
}

/// Every key an account may own; removing the account deletes these.
pub fn account_keys(id: &AccountId) -> [String; 3] {
    [token_key(id), email_key(id), refresh_key(id)]
}

/// Deletes every key of account `id` and nothing else. All are attempted; the first error
/// (no values in it) is returned.
pub fn delete_account_keys(secrets: &dyn SecretStore, id: &AccountId) -> Result<(), SecretError> {
    let mut first = Ok(());
    for k in account_keys(id) {
        if let Err(e) = secrets.delete(&k) {
            if first.is_ok() {
                first = Err(e);
            }
        }
    }
    first
}

/// The OS credential store as the UI names it ("stored in ...").
pub const CREDENTIAL_STORE: &str = if cfg!(windows) {
    "Windows Credential Manager"
} else if cfg!(target_os = "macos") {
    "the macOS Keychain"
} else {
    "the Secret Service keyring"
};

/// `SecretError::Unavailable`: on Linux and the BSDs it says what is missing, since a desktop
/// without a running Secret Service (or a headless machine without a D-Bus session) is common.
const UNAVAILABLE: &str = if cfg!(all(unix, not(target_os = "macos"))) {
    "credential store unavailable: no Secret Service is running (start GNOME Keyring, KWallet \
     or KeePassXC in a D-Bus session)"
} else {
    "credential store unavailable"
};

/// Error text is fixed per category and never carries secret values or
/// underlying OS error messages.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("{}", UNAVAILABLE)]
    Unavailable,
    #[error("stored credential has invalid encoding")]
    BadEncoding,
    #[error("credential too large for the store")]
    TooLong,
    #[error("credential store access denied")]
    AccessDenied,
    #[error("credential store failure")]
    Other,
}

impl From<keyring::Error> for SecretError {
    fn from(e: keyring::Error) -> Self {
        match e {
            keyring::Error::NoStorageAccess(_) => SecretError::AccessDenied,
            keyring::Error::BadEncoding(_) => SecretError::BadEncoding,
            keyring::Error::TooLong(..) => SecretError::TooLong,
            keyring::Error::PlatformFailure(_) => SecretError::Unavailable,
            _ => SecretError::Other,
        }
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

#[derive(Default)]
pub struct MemorySecrets {
    map: Mutex<HashMap<String, String>>,
}

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }
}

impl fmt::Debug for MemorySecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.map.lock().map(|m| m.len()).unwrap_or(0);
        write!(f, "MemorySecrets({n} entries)")
    }
}

impl SecretStore for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        let m = self.map.lock().map_err(|_| SecretError::Other)?;
        Ok(m.get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        let mut m = self.map.lock().map_err(|_| SecretError::Other)?;
        m.insert(key.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let mut m = self.map.lock().map_err(|_| SecretError::Other)?;
        m.remove(key);
        Ok(())
    }
}

pub struct CredentialManagerSecrets {
    service: String,
}

impl CredentialManagerSecrets {
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(&self.service, key).map_err(Into::into)
    }
}

impl fmt::Debug for CredentialManagerSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialManagerSecrets")
            .field("service", &self.service)
            .finish()
    }
}

impl SecretStore for CredentialManagerSecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        match self.entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.entry(key)?.set_password(value).map_err(Into::into)
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests;
