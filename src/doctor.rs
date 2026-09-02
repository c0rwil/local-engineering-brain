//! Local diagnostics used by `leb doctor`.

use serde::{Deserialize, Serialize};

use crate::blobs::BlobStore;
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
        report.checks.push(blob_store_check(database));
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

        checks.push(DoctorCheck {
            name: "workspace_browser".to_owned(),
            status: CheckStatus::Pass,
            detail: if config.web.enabled {
                format!(
                    "served by the daemon at http://127.0.0.1:{} (loopback only)",
                    config.web.port
                )
            } else {
                "disabled ([web] enabled = false); `leb web` still works manually".to_owned()
            },
        });

        checks.push(DoctorCheck {
            name: "commit_watcher".to_owned(),
            status: CheckStatus::Pass,
            detail: if config.watch.enabled {
                let origin = if config.watch.origin_poll_secs == 0 {
                    "origin polling disabled".to_owned()
                } else {
                    format!("origin every {} s", config.watch.origin_poll_secs)
                };
                format!(
                    "committed-HEAD refresh every {} s; {origin}",
                    config.watch.local_poll_secs
                )
            } else {
                "disabled ([watch] enabled = false); indexes refresh only on use".to_owned()
            },
        });

        checks.push(DoctorCheck {
            name: "vault_export".to_owned(),
            status: CheckStatus::Pass,
            detail: if config.vault.enabled {
                format!(
                    "Obsidian vault at {} (auto-refresh {})",
                    config.vault.root.display(),
                    if config.vault.auto_refresh {
                        "on"
                    } else {
                        "off"
                    }
                )
            } else {
                "disabled ([vault] enabled = false)".to_owned()
            },
        });

        checks.push(DoctorCheck {
            name: "brain_sync".to_owned(),
            status: CheckStatus::Pass,
            detail: if config.sync.central_url.is_empty() {
                "standalone (no [sync] central_url)".to_owned()
            } else {
                format!(
                    "syncing with {} every {} s ({})",
                    config.sync.central_url,
                    config.sync.every_secs,
                    if config.sync.push {
                        "pull + offer"
                    } else {
                        "pull only"
                    }
                )
            },
        });

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

/// Report content-addressed blob-store health without touching any file.
///
/// A file with no metadata row wastes space but never loses data, so it is a
/// warning. A metadata row with no file means unreadable canonical content and
/// fails the check.
fn blob_store_check(database: &Database) -> DoctorCheck {
    match BlobStore::new(database.clone()).health() {
        Ok(health) => {
            let status = if health.missing_files > 0 {
                CheckStatus::Fail
            } else if health.orphan_files > 0 {
                CheckStatus::Warning
            } else {
                CheckStatus::Pass
            };
            DoctorCheck {
                name: "blob_store".to_owned(),
                status,
                detail: format!(
                    "blobs={} bytes={} orphan_files={} missing_files={} root={}",
                    health.blob_count,
                    health.total_bytes,
                    health.orphan_files,
                    health.missing_files,
                    health.root.display()
                ),
            }
        }
        Err(error) => DoctorCheck {
            name: "blob_store".to_owned(),
            status: CheckStatus::Fail,
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::blobs::digest_hex;

    fn blob_check(report: &DoctorReport) -> DoctorCheck {
        report
            .checks
            .iter()
            .find(|check| check.name == "blob_store")
            .expect("blob_store check")
            .clone()
    }

    #[test]
    fn blob_store_health_reports_files_without_rows_and_rows_without_files() {
        let temp = TempDir::new().expect("temp dir");
        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        let database = Database::open(&config.storage.db_path).expect("database");

        let healthy = blob_check(&DoctorReport::run_with_database(&config, &database));
        assert_eq!(healthy.status, CheckStatus::Pass);
        assert!(healthy.detail.contains("blobs=0"), "{healthy:?}");

        // A file with no metadata row wastes space but loses nothing.
        BlobStore::new(database.clone())
            .put(b"orphan artifact")
            .expect("orphan blob");
        let report = DoctorReport::run_with_database(&config, &database);
        let orphaned = blob_check(&report);
        assert_eq!(orphaned.status, CheckStatus::Warning);
        assert!(orphaned.detail.contains("orphan_files=1"), "{orphaned:?}");
        assert!(report.ok, "an orphan file must not fail the report");

        // A metadata row with no file means unreadable canonical content.
        let missing = digest_hex(b"never written to disk");
        database
            .write_immediate(|transaction| {
                transaction.execute(
                    "INSERT INTO blobs(digest, size_bytes, content_type, created_at_ms, refcount)
                     VALUES (?1, 21, 'text/plain', 0, 1)",
                    [missing.as_str()],
                )?;
                Ok(())
            })
            .expect("insert blob row");
        let report = DoctorReport::run_with_database(&config, &database);
        let broken = blob_check(&report);
        assert_eq!(broken.status, CheckStatus::Fail);
        assert!(broken.detail.contains("missing_files=1"), "{broken:?}");
        assert!(broken.detail.contains("orphan_files=1"), "{broken:?}");
        assert!(!report.ok);
    }

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
