//! One process-wide guard serializes code that mutates environment variables.

use std::sync::{Mutex, MutexGuard, PoisonError};

pub static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Run `body` while holding the shared environment guard. A panic in an
/// earlier holder poisons the mutex; recover the guard so later callers still
/// run and the original failure stays visible.
pub fn with_env_guard<T>(body: impl FnOnce() -> T) -> T {
    let _guard = recover_poisoned_guard(ENV_GUARD.lock());
    body()
}

/// Take the guard from a poisoned lock result.
pub fn recover_poisoned_guard<'a>(
    result: Result<MutexGuard<'a, ()>, PoisonError<MutexGuard<'a, ()>>>,
) -> MutexGuard<'a, ()> {
    result.unwrap_or_else(PoisonError::into_inner)
}
