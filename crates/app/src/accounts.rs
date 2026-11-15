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
mod tests;
