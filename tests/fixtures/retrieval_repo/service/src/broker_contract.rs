//! Contract checks for the message broker client configuration.

use crate::env_guard::ENV_GUARD;

pub fn broker_endpoint_from_env() -> Option<String> {
    let _guard = ENV_GUARD.lock().expect("environment guard");
    std::env::var("BROKER_ENDPOINT").ok()
}

pub fn broker_requires_tls(endpoint: &str) -> bool {
    !endpoint.starts_with("http://127.0.0.1")
}
