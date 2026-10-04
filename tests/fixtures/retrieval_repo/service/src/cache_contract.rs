//! Contract checks for the shared cache client configuration.

use crate::env_guard::ENV_GUARD;

pub fn cache_token_present() -> bool {
    let _guard = ENV_GUARD.lock().expect("environment guard");
    std::env::var_os("CACHE_TOKEN").is_some()
}
