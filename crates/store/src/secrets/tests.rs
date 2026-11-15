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
