//! Runtime settings read from the process environment and an optional
//! developer dotenv file.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunMode {
    Development,
    Test,
    Production,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Settings {
    pub database_url: String,
    pub broker_endpoint: Option<String>,
    pub request_timeout_ms: u64,
}

impl Settings {
    /// Load settings for one run mode. Values a test removed from the
    /// environment must stay removed.
    pub fn load_for_mode(mode: RunMode, dotenv: &Path) -> Self {
        let mut values = environment_snapshot();
        if mode != RunMode::Production {
            for (key, value) in reload_dotenv_values(dotenv) {
                values.entry(key).or_insert(value);
            }
        }
        Self::from_values(&values)
    }

    fn from_values(values: &BTreeMap<String, String>) -> Self {
        Self {
            database_url: values.get("DATABASE_URL").cloned().unwrap_or_default(),
            broker_endpoint: values.get("BROKER_ENDPOINT").cloned(),
            request_timeout_ms: values
                .get("REQUEST_TIMEOUT_MS")
                .and_then(|value| value.parse().ok())
                .unwrap_or(5_000),
        }
    }
}

/// Copy the current process environment.
pub fn environment_snapshot() -> BTreeMap<String, String> {
    std::env::vars().collect()
}

/// Parse `KEY=VALUE` lines from a developer dotenv file. Missing files are
/// treated as empty.
pub fn reload_dotenv_values(path: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}
