//! Local diagnostics used by `leb doctor`.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::storage::{Database, LATEST_MIGRATION_VERSION};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Warning,
    Fail,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    #[must_use]
    pub fn run(config: &Config) -> Self {
        match Database::open(&config.storage.db_path) {
            Ok(database) => Self::run_with_database(config, &database),
            Err(error) => {
                let mut report = Self::base_checks(config);
                report.checks.push(DoctorCheck {
                    name: "sqlite_open".to_owned(),
                    status: CheckStatus::Fail,
                    detail: error.to_string(),
                });
                report.finish()
            }
        }
    }

    /// Run diagnostics against the daemon's existing canonical database.
    #[must_use]
    pub fn run_with_database(config: &Config, database: &Database) -> Self {
        let mut report = Self::base_checks(config);
        match database.integrity_report() {
            Ok(integrity) => {
                report.checks.push(DoctorCheck {
                    name: "sqlite_integrity".to_owned(),
                    status: if integrity.integrity == "ok" {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    },
                    detail: integrity.integrity,
                });
                report.checks.push(DoctorCheck {
                    name: "sqlite_wal".to_owned(),
                    status: if integrity.journal_mode == "wal" {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    },
                    detail: format!("journal_mode={}", integrity.journal_mode),
                });
                report.checks.push(DoctorCheck {
                    name: "sqlite_foreign_keys".to_owned(),
                    status: if integrity.foreign_keys {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    },
                    detail: format!("enabled={}", integrity.foreign_keys),
                });
                report.checks.push(DoctorCheck {
                    name: "schema".to_owned(),
                    status: if integrity.migration_version == LATEST_MIGRATION_VERSION {
                        CheckStatus::Pass
                    } else {
                        CheckStatus::Fail
                    },
                    detail: format!("migration_version={}", integrity.migration_version),
                });
            }
            Err(error) => report.checks.push(DoctorCheck {
                name: "sqlite_integrity".to_owned(),
                status: CheckStatus::Fail,
                detail: error.to_string(),
            }),
        }
        report.finish()
    }

    fn base_checks(config: &Config) -> Self {
        let mut checks = vec![DoctorCheck {
            name: "configuration".to_owned(),
            status: CheckStatus::Pass,
            detail: "configuration layers parsed and validated".to_owned(),
        }];

        if config.network.local_only {
            checks.push(DoctorCheck {
                name: "local_only".to_owned(),
                status: CheckStatus::Pass,
                detail: "network providers are disabled by default".to_owned(),
            });
        } else {
            checks.push(DoctorCheck {
                name: "local_only".to_owned(),
                status: CheckStatus::Warning,
                detail: "network access was explicitly enabled".to_owned(),
            });
        }

        Self { ok: false, checks }
    }

    fn finish(mut self) -> Self {
        self.ok = self
            .checks
            .iter()
            .all(|check| check.status != CheckStatus::Fail);
        self
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn healthy_local_configuration_passes() {
        let temp = TempDir::new().expect("temp dir");
        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        let report = DoctorReport::run(&config);
        assert!(report.ok, "{report:?}");
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "local_only" && check.status == CheckStatus::Pass)
        );
    }
}
