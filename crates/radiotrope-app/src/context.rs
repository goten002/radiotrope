//! What the window's handlers share

use std::sync::{Mutex, MutexGuard};

/// Lock `mutex`, also after a thread panicked while holding it: what it
/// guards is still used
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
