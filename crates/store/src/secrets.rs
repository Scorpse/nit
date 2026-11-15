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
mod tests {
    use super::*;

    const KEY_BB_TOKEN: &str = LEGACY_BB_TOKEN;
    const KEY_BB_EMAIL: &str = LEGACY_BB_EMAIL;

    #[test]
    fn key_names_never_contain_token() {
        let id = AccountId("acc-7f3c".into());
        assert_eq!(token_key(&id), "account.acc-7f3c.token");
        assert_eq!(email_key(&id), "account.acc-7f3c.email");
        assert_eq!(refresh_key(&id), "account.acc-7f3c.refresh");
        let s = MemorySecrets::new();
        s.set(&token_key(&id), "s3cr3t-token-value").unwrap();
        // a key is derived from the account id only, never from a stored value
        for k in account_keys(&id) {
            assert!(!k.contains("s3cr3t"), "{k}");
        }
    }

    #[test]
    fn remove_account_deletes_its_keys_only() {
        let (a, b) = (AccountId("acc-a".into()), AccountId("acc-b".into()));
        let s = MemorySecrets::new();
        for id in [&a, &b] {
            s.set(&token_key(id), "t").unwrap();
            s.set(&email_key(id), "e").unwrap();
        }
        s.set(LEGACY_BB_TOKEN, "legacy").unwrap();
        delete_account_keys(&s, &a).unwrap();
        assert_eq!(s.get(&token_key(&a)).unwrap(), None);
        assert_eq!(s.get(&email_key(&a)).unwrap(), None);
        assert_eq!(s.get(&token_key(&b)).unwrap().as_deref(), Some("t"));
        assert_eq!(s.get(&email_key(&b)).unwrap().as_deref(), Some("e"));
        assert_eq!(s.get(LEGACY_BB_TOKEN).unwrap().as_deref(), Some("legacy"));
    }

    #[test]
    fn set_then_get() {
        let s = MemorySecrets::new();
        s.set(KEY_BB_TOKEN, "tok").unwrap();
        assert_eq!(s.get(KEY_BB_TOKEN).unwrap().as_deref(), Some("tok"));
    }

    #[test]
    fn get_missing_is_none() {
        let s = MemorySecrets::new();
        assert_eq!(s.get(KEY_BB_EMAIL).unwrap(), None);
    }

    #[test]
    fn delete_removes() {
        let s = MemorySecrets::new();
        s.set(KEY_BB_TOKEN, "tok").unwrap();
        s.delete(KEY_BB_TOKEN).unwrap();
        assert_eq!(s.get(KEY_BB_TOKEN).unwrap(), None);
    }

    #[test]
    fn delete_missing_is_ok() {
        let s = MemorySecrets::new();
        s.delete("nope").unwrap();
    }

    #[test]
    fn set_overwrites_existing_value() {
        let s = MemorySecrets::new();
        s.set(KEY_BB_TOKEN, "old").unwrap();
        s.set(KEY_BB_TOKEN, "new").unwrap();
        assert_eq!(s.get(KEY_BB_TOKEN).unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn secret_error_display_never_contains_value() {
        let secret = "s3cr3t-token-value";
        let cases = [
            keyring::Error::PlatformFailure(secret.into()),
            keyring::Error::NoStorageAccess(secret.into()),
            keyring::Error::BadEncoding(secret.as_bytes().to_vec()),
            keyring::Error::TooLong(secret.to_string(), 1),
            keyring::Error::Invalid(secret.to_string(), secret.to_string()),
            keyring::Error::Ambiguous(vec![]),
        ];
        for e in cases {
            let err = SecretError::from(e);
            assert!(!err.to_string().contains(secret), "display: {err}");
            assert!(!format!("{err:?}").contains(secret), "debug: {err:?}");
        }
    }

    #[test]
    fn an_unavailable_store_says_what_is_missing() {
        let text = SecretError::from(keyring::Error::PlatformFailure("no dbus".into())).to_string();
        assert!(text.starts_with("credential store unavailable"), "{text}");
        assert!(
            !text.contains("no dbus"),
            "the OS message is never shown: {text}"
        );
        if cfg!(all(unix, not(target_os = "macos"))) {
            assert!(text.contains("Secret Service"), "{text}");
        }
    }

    #[test]
    fn memory_secrets_debug_never_contains_value() {
        let s = MemorySecrets::new();
        s.set(KEY_BB_TOKEN, "s3cr3t-token-value").unwrap();
        let d = format!("{s:?}");
        assert!(!d.contains("s3cr3t-token-value"));
        assert!(!d.contains(KEY_BB_TOKEN));
        assert!(d.contains('1'));
    }

    /// Manual: touches the real Windows Credential Manager.
    /// Run with `cargo test -p store -- --ignored`.
    #[test]
    #[ignore]
    fn credential_manager_round_trip() {
        let service = format!(
            "pr-review-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let s = CredentialManagerSecrets::new(&service);
        assert_eq!(s.get(KEY_BB_TOKEN).unwrap(), None);
        s.set(KEY_BB_TOKEN, "one").unwrap();
        assert_eq!(s.get(KEY_BB_TOKEN).unwrap().as_deref(), Some("one"));
        s.set(KEY_BB_TOKEN, "two").unwrap();
        assert_eq!(s.get(KEY_BB_TOKEN).unwrap().as_deref(), Some("two"));
        s.delete(KEY_BB_TOKEN).unwrap();
        assert_eq!(s.get(KEY_BB_TOKEN).unwrap(), None);
        s.delete(KEY_BB_TOKEN).unwrap();
    }
}
