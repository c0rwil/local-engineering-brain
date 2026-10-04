//! Local diagnostics used by `engram doctor`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::blobs::BlobStore;
use crate::codex_trust::{self, HookTrust};
use crate::config::Config;
use crate::projects::{CheckoutState, CheckoutSurvey, ProjectRegistry};
use crate::storage::{Database, LATEST_MIGRATION_VERSION};

/// Upper bound on recorded checkout paths probed per doctor run.
const MAX_PROBED_CHECKOUTS: usize = 256;

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
        report
            .checks
            .push(crate::security::validated_memory_check(database));
        report.checks.extend(checkout_checks(database));
        report.finish()
    }

    /// Add checks that read the invoking user's agent configuration. They run
    /// in the CLI process: agent homes such as `$CODEX_HOME` follow the user's
    /// environment, not that of whichever process started the daemon.
    #[must_use]
    pub fn with_agent_checks(mut self) -> Self {
        if let Some(check) = codex_trust::codex_home()
            .as_deref()
            .and_then(codex_hooks_check)
        {
            self.checks.push(check);
        }
        self.finish()
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
                "disabled ([web] enabled = false); `engram web` still works manually".to_owned()
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

/// Probe every registered checkout from this process (the daemon, whose
/// permissions are the ones that matter) and report access and duplicate
/// project identity.
fn checkout_checks(database: &Database) -> Vec<DoctorCheck> {
    match ProjectRegistry::new(database.clone()).survey_checkouts(MAX_PROBED_CHECKOUTS) {
        Ok(survey) => vec![
            checkout_access_check(&survey),
            duplicate_identity_check(&survey),
        ],
        Err(error) => vec![DoctorCheck {
            name: "checkout_access".to_owned(),
            status: CheckStatus::Warning,
            detail: format!("could not list registered checkouts: {error}"),
        }],
    }
}

/// Who probed, and (for an auto-started daemon) which process started it:
/// on macOS a daemon inherits privacy access from that launcher.
fn probing_process() -> (String, Option<String>) {
    let process = format!(
        "{} pid {}",
        crate::error::current_process_name(),
        std::process::id()
    );
    let launcher = std::env::var(crate::autostart::LAUNCHER_ENV)
        .ok()
        .filter(|launcher| !launcher.trim().is_empty());
    (process, launcher)
}

fn checkout_access_check(survey: &CheckoutSurvey) -> DoctorCheck {
    let (process, launcher) = probing_process();
    let started_by = launcher
        .as_deref()
        .map_or_else(String::new, |launcher| format!("; started by {launcher}"));
    let mut readable = 0_usize;
    let mut missing = 0_usize;
    let mut privacy_denial = false;
    let mut denied = Vec::new();
    let mut failed = Vec::new();
    for probe in &survey.probes {
        match &probe.state {
            CheckoutState::Missing => missing += 1,
            CheckoutState::Readable { .. } => readable += 1,
            CheckoutState::Denied { path, reason } => {
                privacy_denial |= reason.contains("(os error 1)");
                denied.push(format!(
                    "{} ({}) at {}: {reason}",
                    probe.project_name,
                    probe.project_id,
                    path.display()
                ));
            }
            CheckoutState::Failed { reason } => failed.push(format!(
                "{} ({}): {reason}",
                probe.project_name, probe.project_id
            )),
        }
    }
    let truncated = if survey.truncated {
        format!(" (first {MAX_PROBED_CHECKOUTS} recorded paths only)")
    } else {
        String::new()
    };
    let examined = readable + denied.len() + failed.len();
    if !denied.is_empty() {
        let remedy = if cfg!(target_os = "macos") && privacy_denial {
            format!(
                "On macOS a daemon inherits privacy (TCC) access from the app that started \
                 it{started_by}. Restart it from a terminal or app that can read these folders \
                 (`engram daemon restart`), or grant engramd access in System Settings > Privacy \
                 & Security (Full Disk Access or Files and Folders); each reinstall puts engramd \
                 at a new path, so such a grant likely does not survive reinstalling"
            )
        } else {
            "Make these paths readable by the user running the daemon, then retry".to_owned()
        };
        return DoctorCheck {
            name: "checkout_access".to_owned(),
            status: CheckStatus::Fail,
            detail: format!(
                "{process} cannot read {} of {examined} existing checkout paths of registered \
                 projects{truncated}: {}. Context, capture, indexing and identity degrade for \
                 these repositories. {remedy}.",
                denied.len(),
                denied.join("; ")
            ),
        };
    }
    if !failed.is_empty() {
        return DoctorCheck {
            name: "checkout_access".to_owned(),
            status: CheckStatus::Warning,
            detail: format!(
                "{process} could not probe {} of {examined} existing checkout paths of \
                 registered projects{truncated}: {}",
                failed.len(),
                failed.join("; ")
            ),
        };
    }
    DoctorCheck {
        name: "checkout_access".to_owned(),
        status: CheckStatus::Pass,
        detail: format!(
            "{process} can read all {readable} existing checkout paths of registered \
             projects{truncated} ({missing} recorded paths no longer exist){started_by}"
        ),
    }
}

/// Flag repositories registered as more than one project: live checkouts of
/// different project ids that share a canonical remote and root commit.
fn duplicate_identity_check(survey: &CheckoutSurvey) -> DoctorCheck {
    let names = survey
        .probes
        .iter()
        .map(|probe| {
            (
                probe.project_id.clone(),
                (probe.project_name.clone(), probe.hidden),
            )
        })
        .collect::<BTreeMap<_, _>>();
    // (remote, root commit) -> owning project -> first live checkout path
    let mut repositories = BTreeMap::<(String, String), BTreeMap<_, PathBuf>>::new();
    let mut unknown = 0_usize;
    for probe in &survey.probes {
        match &probe.state {
            CheckoutState::Readable {
                identity: Some(identity),
            } => {
                let (Some(remote), Some(root)) =
                    (&identity.canonical_remote, &identity.root_commit)
                else {
                    continue;
                };
                // The checkout's own pinned metadata decides which project it
                // belongs to; an older alias of another project is stale.
                let owner = identity
                    .pinned_project_id
                    .clone()
                    .filter(|pinned| names.contains_key(pinned))
                    .unwrap_or_else(|| probe.project_id.clone());
                repositories
                    .entry((remote.clone(), root.clone()))
                    .or_default()
                    .entry(owner)
                    .or_insert_with(|| probe.path.clone());
            }
            CheckoutState::Denied { .. } | CheckoutState::Failed { .. } => unknown += 1,
            _ => {}
        }
    }
    let unchecked = if unknown == 0 {
        String::new()
    } else {
        format!(" ({unknown} unreadable checkouts not compared)")
    };
    let duplicates = repositories
        .iter()
        .filter(|(_, owners)| owners.len() > 1)
        .map(|((remote, root), owners)| {
            let projects = owners
                .iter()
                .map(|(id, path)| {
                    let (name, hidden) = names
                        .get(id)
                        .map_or(("?", false), |(name, hidden)| (name.as_str(), *hidden));
                    let hidden = if hidden { ", hidden" } else { "" };
                    format!("{name} ({id}{hidden}) at {}", path.display())
                })
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "{remote} (root commit {}) is registered as {} projects: {projects}",
                &root[..root.len().min(12)],
                owners.len()
            )
        })
        .collect::<Vec<_>>();
    if duplicates.is_empty() {
        return DoctorCheck {
            name: "project_identity".to_owned(),
            status: CheckStatus::Pass,
            detail: format!(
                "no repository is registered as more than one project (live remote and root \
                 commit){unchecked}"
            ),
        };
    }
    DoctorCheck {
        name: "project_identity".to_owned(),
        status: CheckStatus::Warning,
        detail: format!(
            "{}{unchecked}. Knowledge recorded under one project id is not visible from the \
             other. Engram never merges projects automatically: review `engram project merge \
             --into <keep> --from <retire> --dry-run`, then run it with --yes (see \"Duplicate \
             project registrations\" in docs/USER_GUIDE.md)",
            duplicates.join(". ")
        ),
    }
}

/// Trust state of Engram's Codex hooks. `None` when Codex has no
/// `hooks.json` in `codex_home`.
fn codex_hooks_check(codex_home: &Path) -> Option<DoctorCheck> {
    let report = match codex_trust::inspect(codex_home) {
        Ok(Some(report)) => report,
        Ok(None) => return None,
        Err(error) => {
            return Some(DoctorCheck {
                name: "codex_hooks".to_owned(),
                status: CheckStatus::Warning,
                detail: format!(
                    "could not read Codex hook trust in {}: {error}",
                    codex_home.display()
                ),
            });
        }
    };
    if report.hooks.is_empty() {
        return Some(DoctorCheck {
            name: "codex_hooks".to_owned(),
            status: CheckStatus::Pass,
            detail: format!(
                "no Engram hooks in {}; `engram hooks codex --install` adds them",
                report.hooks_file.display()
            ),
        });
    }
    let inactive = report.inactive().collect::<Vec<_>>();
    if inactive.is_empty() {
        return Some(DoctorCheck {
            name: "codex_hooks".to_owned(),
            status: CheckStatus::Pass,
            detail: format!(
                "all {} Engram hooks in {} are trusted in {}",
                report.hooks.len(),
                report.hooks_file.display(),
                report.config_file.display()
            ),
        });
    }
    let mut reasons = Vec::new();
    if report.hooks_feature_disabled {
        reasons.push("Codex hooks are turned off ([features] hooks = false)".to_owned());
    }
    for (trust, label) in [
        (HookTrust::Untrusted, "never trusted"),
        (HookTrust::Modified, "changed since trusted"),
        (HookTrust::Disabled, "disabled"),
    ] {
        let events = inactive
            .iter()
            .filter(|hook| hook.trust == trust)
            .map(|hook| hook.event.as_str())
            .collect::<Vec<_>>();
        if !events.is_empty() {
            reasons.push(format!("{label}: {}", events.join(", ")));
        }
    }
    let recent =
        codex_trust::recent_session_files(codex_home, Duration::from_secs(7 * 24 * 60 * 60));
    Some(DoctorCheck {
        name: "codex_hooks".to_owned(),
        status: CheckStatus::Warning,
        detail: format!(
            "{} of {} Engram hooks in {} will not run ({}). Codex runs only hooks you reviewed \
             and trusted: open Codex, run /hooks and trust the Engram entries. Codex wrote \
             {recent} session files in the last 7 days.",
            inactive.len(),
            report.hooks.len(),
            report.hooks_file.display(),
            reasons.join("; ")
        ),
    })
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

    fn check(report: &DoctorReport, name: &str) -> DoctorCheck {
        report
            .checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("{name} check in {report:?}"))
            .clone()
    }

    fn git(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args([
                "-c",
                "user.email=engram@example.invalid",
                "-c",
                "user.name=Engram",
            ])
            .args(args)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn database(temp: &TempDir) -> (Config, Database) {
        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        let database = Database::open(&config.storage.db_path).expect("database");
        (config, database)
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_registered_checkout_fails_with_the_path_and_remedy() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = TempDir::new().expect("temp dir");
        let (config, database) = database(&temp);
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        git(&repo, &["init", "-q"]);
        let registry = ProjectRegistry::new(database.clone());
        let project = registry.resolve_or_register(&repo).expect("register");
        let readable = DoctorReport::run_with_database(&config, &database);
        assert_eq!(
            check(&readable, "checkout_access").status,
            CheckStatus::Pass
        );

        std::fs::set_permissions(&project.root, std::fs::Permissions::from_mode(0o000))
            .expect("lock");
        let enforced = std::fs::read_dir(&project.root).is_err();
        let report = DoctorReport::run_with_database(&config, &database);
        std::fs::set_permissions(&project.root, std::fs::Permissions::from_mode(0o755))
            .expect("unlock");
        if !enforced {
            return; // running as root: file modes are not enforced
        }
        let access = check(&report, "checkout_access");
        assert_eq!(access.status, CheckStatus::Fail, "{access:?}");
        assert!(
            access.detail.contains(&project.root.display().to_string()),
            "{access:?}"
        );
        assert!(
            access.detail.contains(project.project.id.as_str()),
            "{access:?}"
        );
        assert!(!report.ok);
    }

    #[test]
    fn repositories_registered_as_two_projects_are_flagged() {
        let temp = TempDir::new().expect("temp dir");
        let (config, database) = database(&temp);
        let registry = ProjectRegistry::new(database.clone());
        let upstream = temp.path().join("upstream");
        std::fs::create_dir_all(&upstream).expect("upstream");
        git(&upstream, &["init", "-q"]);
        std::fs::write(upstream.join("README.md"), "shared\n").expect("readme");
        git(&upstream, &["add", "README.md"]);
        git(&upstream, &["commit", "-qm", "root"]);
        let mut ids = Vec::new();
        for name in ["first", "second"] {
            // Registered while Git was invisible, then the history appeared.
            let checkout = temp.path().join(name);
            std::fs::create_dir_all(&checkout).expect("checkout");
            ids.push(
                registry
                    .resolve_or_register(&checkout)
                    .expect("register")
                    .project
                    .id,
            );
            git(&checkout, &["init", "-q"]);
            git(
                &checkout,
                &["fetch", "-q", upstream.to_str().unwrap(), "HEAD"],
            );
            git(&checkout, &["reset", "-q", "--hard", "FETCH_HEAD"]);
            git(
                &checkout,
                &["remote", "add", "origin", "git@example.com:org/shared.git"],
            );
        }
        let report = DoctorReport::run_with_database(&config, &database);
        let identity = check(&report, "project_identity");
        assert_eq!(identity.status, CheckStatus::Warning, "{identity:?}");
        assert!(
            identity.detail.contains("example.com/org/shared"),
            "{identity:?}"
        );
        for id in &ids {
            assert!(identity.detail.contains(id.as_str()), "{identity:?}");
        }
        assert!(report.ok, "a duplicate is a warning, not a failure");

        // Only live checkouts count: with one copy gone there is no duplicate.
        std::fs::remove_dir_all(temp.path().join("second")).expect("remove copy");
        let single = DoctorReport::run_with_database(&config, &database);
        assert_eq!(
            check(&single, "project_identity").status,
            CheckStatus::Pass,
            "{single:?}"
        );
    }

    #[test]
    fn codex_hooks_check_reports_untrusted_engram_hooks_only() {
        let home = TempDir::new().expect("codex home");
        assert!(codex_hooks_check(home.path()).is_none(), "no hooks.json");

        let hooks = |handlers: &[serde_json::Value]| {
            serde_json::json!({"hooks": {"Stop": [{"hooks": handlers}]}}).to_string()
        };
        let foreign = serde_json::json!({"type": "command", "command": "echo foreign"});
        std::fs::write(
            home.path().join("hooks.json"),
            hooks(std::slice::from_ref(&foreign)),
        )
        .expect("hooks.json");
        let none = codex_hooks_check(home.path()).expect("check");
        assert_eq!(none.status, CheckStatus::Pass);
        assert!(
            none.detail.contains("engram hooks codex --install"),
            "{none:?}"
        );

        let engram = serde_json::json!({"type": "command",
            "command": "'/opt/example/bin/engram' hook stop --codex", "timeout": 15});
        std::fs::write(
            home.path().join("hooks.json"),
            hooks(&[foreign, engram.clone()]),
        )
        .expect("hooks.json");
        let untrusted = codex_hooks_check(home.path()).expect("check");
        assert_eq!(untrusted.status, CheckStatus::Warning, "{untrusted:?}");
        assert!(
            untrusted.detail.contains("never trusted: stop"),
            "{untrusted:?}"
        );
        assert!(untrusted.detail.contains("/hooks"), "{untrusted:?}");

        let hooks_file = std::fs::canonicalize(home.path())
            .expect("canonical")
            .join("hooks.json");
        let key = codex_trust::hook_key(&hooks_file, "stop", 0, 1);
        let hash = codex_trust::trust_hash("stop", None, &engram).expect("hash");
        std::fs::write(
            home.path().join("config.toml"),
            format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"{hash}\"\n"),
        )
        .expect("config.toml");
        let trusted = codex_hooks_check(home.path()).expect("check");
        assert_eq!(trusted.status, CheckStatus::Pass, "{trusted:?}");
    }
}
