//! Isolated local knowledge workspaces.
//!
//! Existing installations keep `default` at `~/.engram`. Named workspaces live
//! at `~/.engram/workspaces/<name>` and inherit global tuning, with their own
//! database, socket, vault, and optional `config.toml`. Select one with
//! `--workspace NAME`, `ENGRAM_WORKSPACE=NAME`, or a top-level
//! `workspace = "NAME"` in a repository's `.engram/config.toml`, in that order.
//! Merely selecting or listing a workspace never creates state or indexes a
//! repository. Named workspace web listeners are off until a profile or
//! project config explicitly enables one on a separate port.

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::config::Config;
use crate::error::{Error, Result, io_error};

pub const DEFAULT_WORKSPACE: &str = "default";

/// A workspace profile and its current local state. No database is opened to
/// list profiles, so inspecting an empty workspace cannot create a database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub root: PathBuf,
    pub db_path: PathBuf,
    pub socket_path: PathBuf,
    pub config_path: PathBuf,
    pub initialized: bool,
    pub active: bool,
}

/// Validate a portable, case-insensitive-filesystem-safe workspace name.
///
/// # Errors
///
/// Rejects names outside 1–32 lowercase ASCII letters, digits, hyphens, and
/// underscores, or names that do not begin with a letter or digit.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 32
        || !name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
    {
        return Err(Error::Config(
            "workspace must be 1–32 lowercase letters, digits, hyphens, or underscores, starting with a letter or digit".to_owned(),
        ));
    }
    Ok(())
}

/// Return a workspace's canonical state root without creating it.
///
/// # Errors
///
/// Rejects an invalid workspace name.
pub fn root(home: &Path, name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    let base = home.join(".engram");
    Ok(if name == DEFAULT_WORKSPACE {
        base
    } else {
        base.join("workspaces").join(name)
    })
}

/// List the default profile, existing named profile directories, and the
/// selected profile (including a selected workspace not created yet).
///
/// # Errors
///
/// Returns directory read errors or invalid selected workspace names.
pub fn list(home: &Path, selected: &Config) -> Result<Vec<WorkspaceInfo>> {
    validate_name(&selected.workspace)?;
    let profiles = home.join(".engram/workspaces");
    let mut names = std::collections::BTreeSet::from([DEFAULT_WORKSPACE.to_owned()]);
    names.insert(selected.workspace.clone());
    match fs::read_dir(&profiles) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|source| io_error(&profiles, source))?;
                let file_type = entry
                    .file_type()
                    .map_err(|source| io_error(entry.path(), source))?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if file_type.is_dir() && validate_name(&name).is_ok() {
                    names.insert(name);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(&profiles, error)),
    }
    let mut result = Vec::with_capacity(names.len());
    for name in names {
        let profile_root = root(home, &name)?;
        let active = name == selected.workspace;
        let db_path = if active {
            selected.storage.db_path.clone()
        } else {
            profile_root.join("brain.sqlite")
        };
        let socket_path = if active {
            selected.daemon.socket_path.clone()
        } else {
            profile_root.join("engramd.sock")
        };
        let config_path = if name == DEFAULT_WORKSPACE {
            home.join(".config/engram/config.toml")
        } else {
            profile_root.join("config.toml")
        };
        result.push(WorkspaceInfo {
            name,
            root: profile_root,
            initialized: db_path.is_file(),
            db_path,
            socket_path,
            config_path,
            active,
        });
    }
    result.sort_by(|left, right| {
        (left.name != DEFAULT_WORKSPACE, &left.name)
            .cmp(&(right.name != DEFAULT_WORKSPACE, &right.name))
    });
    Ok(result)
}

/// Revalidate already-resolved configs after CLI/socket overrides. Derive the
/// selected profile root from the database path so this also works with an
/// explicit home directory in hermetic tests and with startup snapshots.
pub(crate) fn validate_config(config: &Config) -> Result<()> {
    if config.workspace == DEFAULT_WORKSPACE {
        return Ok(());
    }
    let root = config
        .storage
        .db_path
        .ancestors()
        .find(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|name| name == config.workspace.as_str())
                && ancestor
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "workspaces")
                && ancestor
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == ".engram")
        })
        .ok_or_else(|| {
            Error::Config(format!(
                "named workspace {} must use its own .engram/workspaces/{}/ database directory",
                config.workspace, config.workspace,
            ))
        })?;
    validate_paths(config, root)?;
    if config.web.enabled && config.web.port == crate::config::WebConfig::default().port {
        return Err(Error::Config(
            "named workspace web requires its own explicit web.port (the default port belongs to the default workspace)".to_owned(),
        ));
    }
    Ok(())
}

/// Named workspace paths must stay beneath their own state directory. Reject
/// symlink aliases too: distinct path strings are insufficient isolation.
pub(crate) fn validate_paths(config: &Config, root: &Path) -> Result<()> {
    if let Some(parent) = root.parent() {
        reject_symlink(parent)?;
    }
    reject_symlink(root)?;
    for (field, path) in [
        ("storage.db_path", &config.storage.db_path),
        ("daemon.socket_path", &config.daemon.socket_path),
        ("vault.root", &config.vault.root),
    ] {
        let Ok(relative) = path.strip_prefix(root) else {
            return Err(Error::Config(format!(
                "{field} for workspace {} must stay inside {}",
                config.workspace,
                root.display()
            )));
        };
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(Error::Config(format!(
                "{field} must name a path inside the workspace without parent traversal"
            )));
        }
        let mut ancestor = root.to_path_buf();
        for component in relative.components() {
            ancestor.push(component);
            reject_symlink(&ancestor)?;
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Config(format!(
            "workspace state must not alias another location through symlink {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigOverrides;

    #[test]
    fn workspace_names_reject_traversal_and_case_aliases() {
        for name in [
            "",
            ".",
            "..",
            "../other",
            "team/repo",
            "/tmp/work",
            "Team",
            "_hidden",
            "a.b",
            "white space",
        ] {
            assert!(validate_name(name).is_err(), "accepted {name}");
        }
        assert!(validate_name(&"a".repeat(33)).is_err());
        for name in ["default", "work", "client-a", "team_2", "7days"] {
            validate_name(name).expect("valid name");
        }
    }

    #[test]
    fn listing_does_not_create_selected_workspace() {
        let temp = tempfile::TempDir::new().expect("temp");
        let selected = Config::load_layers(
            None,
            None,
            &ConfigOverrides {
                workspace: Some("work".to_owned()),
                ..ConfigOverrides::default()
            },
            temp.path(),
        )
        .expect("config");
        let entries = list(temp.path(), &selected).expect("list");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "default");
        assert!(entries[1].active);
        assert!(!entries[1].initialized);
        assert!(!entries[1].root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn named_workspace_cannot_alias_default_through_symlink() {
        let temp = tempfile::TempDir::new().expect("temp");
        fs::create_dir_all(temp.path().join(".engram/workspaces")).expect("profiles");
        std::os::unix::fs::symlink(
            temp.path().join(".engram"),
            temp.path().join(".engram/workspaces/work"),
        )
        .expect("symlink");
        let error = Config::load_layers(
            None,
            None,
            &ConfigOverrides {
                workspace: Some("work".to_owned()),
                ..ConfigOverrides::default()
            },
            temp.path(),
        )
        .expect_err("alias rejected");
        assert!(error.to_string().contains("symlink"));
    }
}
