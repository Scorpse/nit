//! Poisoned-lock recovery.

use std::sync::{Mutex, MutexGuard};

/// Locks `m`; a lock poisoned by a thread that panicked while holding it is taken anyway.
pub fn lock_ok<T: ?Sized>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
