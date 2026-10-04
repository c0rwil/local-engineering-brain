//! Harness that runs settings checks with temporary environment overrides.

use crate::env_guard::ENV_GUARD;
use crate::settings::{RunMode, Settings};

pub fn run_with_overrides(overrides: &[(&str, &str)]) -> Settings {
    let _guard = ENV_GUARD.lock().expect("environment guard");
    for (key, value) in overrides {
        std::env::set_var(key, value);
    }
    Settings::load_for_mode(RunMode::Test, std::path::Path::new(".env"))
}
