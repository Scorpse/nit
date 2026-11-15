//! Accounts: the move of the single pre-multi-forge Bitbucket sign-in to the default
//! account's Credential Manager keys, and the keys the app reads for it.

use prcore::model::AccountId;
use store::{email_key, token_key, Account, SecretStore, Store, LEGACY_BB_EMAIL, LEGACY_BB_TOKEN};

/// The default Bitbucket account's (email key, token key).
pub fn bitbucket_keys() -> (String, String) {
    let id = AccountId::default_bitbucket();
    (email_key(&id), token_key(&id))
}

/// What `migrate_legacy_bitbucket` did. Carries no values.
#[derive(Debug, PartialEq, Eq)]
pub enum MigrateOutcome {
    /// No complete legacy sign-in (none, or only half of it): nothing to move.
    Nothing,
    Moved,
    /// Writing or reading back the new keys failed: the new keys are removed again, the
    /// legacy ones are kept, and the next start tries again.
    Failed,
}

/// Moves `bitbucket.email` / `bitbucket.token` to `account.acc-bitbucket-default.{email,token}`:
/// copy, read both back, make sure the account exists in the store, and only then delete the
/// legacy keys. The legacy keys are the truth while they exist (only an older build writes
/// them), so a rerun after a half-done move copies them again. Values are never logged or
/// returned.
pub fn migrate_legacy_bitbucket(secrets: &dyn SecretStore, store: &Store) -> MigrateOutcome {
    let (Ok(Some(email)), Ok(Some(token))) =
        (secrets.get(LEGACY_BB_EMAIL), secrets.get(LEGACY_BB_TOKEN))
    else {
        return MigrateOutcome::Nothing;
    };
    let (ek, tk) = bitbucket_keys();
    let copied = secrets
        .set(&ek, &email)
        .and_then(|_| secrets.set(&tk, &token))
        .and_then(|_| Ok((secrets.get(&ek)?, secrets.get(&tk)?)));
    let verified = matches!(
        &copied,
        Ok((Some(e), Some(t))) if *e == email && *t == token
    );
    let account_ok = verified && ensure_default_account(store);
    if !account_ok {
        let _ = secrets.delete(&tk);
        let _ = secrets.delete(&ek);
        return MigrateOutcome::Failed;
    }
    let _ = secrets.delete(LEGACY_BB_TOKEN);
    let _ = secrets.delete(LEGACY_BB_EMAIL);
    MigrateOutcome::Moved
}

/// Adds the default Bitbucket account unless the store has it (the v7 upgrade adds it for
/// a cache that had a sign-in). False when the store could not be read or written.
pub fn ensure_default_account(store: &Store) -> bool {
    let id = AccountId::default_bitbucket();
    match store.accounts() {
        Ok(all) if all.iter().any(|a| a.id == id) => true,
        Ok(_) => store
            .upsert_account(&Account::default_bitbucket(String::new(), String::new()))
            .is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
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
}
