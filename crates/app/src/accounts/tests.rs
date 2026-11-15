use super::*;
use store::{MemorySecrets, SecretError};

const EMAIL: &str = "ana@example.com";
const TOKEN: &str = "ATATT-not-a-real-token-value";

fn legacy() -> MemorySecrets {
    let s = MemorySecrets::new();
    s.set(LEGACY_BB_EMAIL, EMAIL).unwrap();
    s.set(LEGACY_BB_TOKEN, TOKEN).unwrap();
    s
}

fn get(s: &dyn SecretStore, k: &str) -> Option<String> {
    s.get(k).unwrap()
}

#[test]
fn legacy_keys_move_to_default_account() {
    let (s, store) = (legacy(), Store::open_in_memory().unwrap());
    assert_eq!(migrate_legacy_bitbucket(&s, &store), MigrateOutcome::Moved);
    let (ek, tk) = bitbucket_keys();
    assert_eq!(ek, "account.acc-bitbucket-default.email");
    assert_eq!(tk, "account.acc-bitbucket-default.token");
    assert_eq!(get(&s, &ek).as_deref(), Some(EMAIL));
    assert_eq!(get(&s, &tk).as_deref(), Some(TOKEN));
    assert_eq!(get(&s, LEGACY_BB_EMAIL), None);
    assert_eq!(get(&s, LEGACY_BB_TOKEN), None);
    let accounts = store.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, AccountId::default_bitbucket());
}

#[test]
fn migration_is_idempotent() {
    let (s, store) = (legacy(), Store::open_in_memory().unwrap());
    assert_eq!(migrate_legacy_bitbucket(&s, &store), MigrateOutcome::Moved);
    assert_eq!(
        migrate_legacy_bitbucket(&s, &store),
        MigrateOutcome::Nothing
    );
    let (ek, tk) = bitbucket_keys();
    assert_eq!(get(&s, &ek).as_deref(), Some(EMAIL));
    assert_eq!(get(&s, &tk).as_deref(), Some(TOKEN));
    assert_eq!(store.accounts().unwrap().len(), 1);
}

#[test]
fn an_existing_account_row_is_kept() {
    let (s, store) = (legacy(), Store::open_in_memory().unwrap());
    let a = Account::default_bitbucket("{me}".into(), "Ann".into());
    store.upsert_account(&a).unwrap();
    assert_eq!(migrate_legacy_bitbucket(&s, &store), MigrateOutcome::Moved);
    assert_eq!(store.accounts().unwrap(), vec![a]);
}

#[test]
fn a_half_legacy_entry_is_not_moved() {
    let (s, store) = (MemorySecrets::new(), Store::open_in_memory().unwrap());
    s.set(LEGACY_BB_EMAIL, EMAIL).unwrap();
    assert_eq!(
        migrate_legacy_bitbucket(&s, &store),
        MigrateOutcome::Nothing
    );
    assert_eq!(get(&s, LEGACY_BB_EMAIL).as_deref(), Some(EMAIL));
    assert!(store.accounts().unwrap().is_empty());
}

/// Fails writes of one key, or returns another value on read-back.
struct Faulty {
    inner: MemorySecrets,
    fail_key: String,
    corrupt: bool,
}

impl SecretStore for Faulty {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        let v = self.inner.get(key)?;
        if self.corrupt && key == self.fail_key {
            return Ok(v.map(|_| "something else".to_string()));
        }
        Ok(v)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        if !self.corrupt && key == self.fail_key {
            return Err(SecretError::TooLong);
        }
        self.inner.set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.inner.delete(key)
    }
}

fn faulty(corrupt: bool) -> Faulty {
    Faulty {
        inner: legacy(),
        fail_key: bitbucket_keys().1,
        corrupt,
    }
}

#[test]
fn failed_write_leaves_legacy_keys() {
    let (s, store) = (faulty(false), Store::open_in_memory().unwrap());
    let out = migrate_legacy_bitbucket(&s, &store);
    assert_eq!(out, MigrateOutcome::Failed);
    assert!(!format!("{out:?}").contains(TOKEN));
    let (ek, tk) = bitbucket_keys();
    assert_eq!(get(&s, &ek), None, "no half entry");
    assert_eq!(get(&s, &tk), None);
    assert_eq!(get(&s, LEGACY_BB_EMAIL).as_deref(), Some(EMAIL));
    assert_eq!(get(&s, LEGACY_BB_TOKEN).as_deref(), Some(TOKEN));
    // the store was not touched either
    assert!(store.accounts().unwrap().is_empty());
}

#[test]
fn a_read_back_mismatch_leaves_legacy_keys() {
    let (s, store) = (faulty(true), Store::open_in_memory().unwrap());
    assert_eq!(migrate_legacy_bitbucket(&s, &store), MigrateOutcome::Failed);
    assert_eq!(get(&s, LEGACY_BB_TOKEN).as_deref(), Some(TOKEN));
    assert_eq!(get(&s.inner, &bitbucket_keys().1), None);
}
