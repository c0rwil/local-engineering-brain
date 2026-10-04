//! Shared local daemon auto-start used by edge binaries.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::Config;
use crate::daemon::DaemonClient;
use crate::error::{Error, Result, io_error_for_cli};

/// Ensure the sibling `engramd` is healthy, starting it detached when absent.
///
/// # Errors
///
/// Returns an error when the daemon cannot be located, started, or made ready.
pub async fn ensure_daemon(config: &Config) -> Result<()> {
    let client = DaemonClient::new(
        &config.daemon.socket_path,
        Duration::from_millis(config.daemon.request_timeout_ms.min(2_000)),
    );
    if matching_health(&client, config).await?.is_some() {
        return Ok(());
    }
    let current =
        std::env::current_exe().map_err(|source| io_error_for_cli("current executable", source))?;
    let daemon = current
        .parent()
        .map(|parent| parent.join("engramd"))
        .ok_or_else(|| Error::Daemon("could not locate sibling engramd".to_owned()))?;
    if !daemon.is_file() {
        return Err(Error::Daemon(format!(
            "cannot auto-start: sibling daemon is missing at {}",
            daemon.display()
        )));
    }
    // Carry the complete resolved configuration across the process boundary.
    // Passing just --db/--socket lost project/profile judge and web settings.
    // NamedTempFile creates a private file; retaining it through readiness lets
    // the child finish reading before its secrets are removed from disk.
    let snapshot = configuration_snapshot(config)?;
    let mut child = daemon_command(&daemon, config, snapshot.path())
        .spawn()
        .map_err(|source| io_error_for_cli(&daemon, source))?;
    std::thread::Builder::new()
        .name("engramd-child-reaper".to_owned())
        .stack_size(128 * 1_024)
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|error| Error::Daemon(format!("could not start daemon reaper: {error}")))?;
    for _ in 0..100 {
        if matching_health(&client, config).await?.is_some() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(Error::Daemon(format!(
        "auto-started engramd did not become ready at {}",
        config.daemon.socket_path.display()
    )))
}

fn configuration_snapshot(config: &Config) -> Result<tempfile::NamedTempFile> {
    let mut snapshot = tempfile::Builder::new()
        .prefix("engram-daemon-config-")
        .suffix(".toml")
        .tempfile()
        .map_err(|source| io_error_for_cli("daemon configuration snapshot", source))?;
    snapshot
        .write_all(toml::to_string(config)?.as_bytes())
        .and_then(|()| snapshot.flush())
        .map_err(|source| io_error_for_cli(snapshot.path(), source))?;
    Ok(snapshot)
}

fn daemon_command(daemon: &Path, config: &Config, snapshot: &Path) -> Command {
    let mut command = Command::new(daemon);
    command
        .arg("--resolved-config")
        .arg(snapshot)
        .env("ENGRAM_WORKSPACE", &config.workspace)
        .env("ENGRAM_DB_PATH", &config.storage.db_path)
        .env("ENGRAM_SOCKET_PATH", &config.daemon.socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

async fn matching_health(
    client: &DaemonClient,
    config: &Config,
) -> Result<Option<crate::daemon::HealthResponse>> {
    let Ok(health) = client.health().await else {
        return Ok(None);
    };
    if health.database != config.storage.db_path {
        return Err(Error::Daemon(format!(
            "socket {} belongs to database {}, not {}",
            config.daemon.socket_path.display(),
            health.database.display(),
            config.storage.db_path.display()
        )));
    }
    // Older daemons did not report a workspace. They can only satisfy the
    // backwards-compatible default profile, never a named workspace.
    if health.workspace != config.workspace
        && !(health.workspace.is_empty()
            && config.workspace == crate::workspaces::DEFAULT_WORKSPACE)
    {
        return Err(Error::Daemon(format!(
            "socket {} serves workspace {:?}, not {:?}",
            config.daemon.socket_path.display(),
            health.workspace,
            config.workspace
        )));
    }
    if health.build_id != crate::BUILD_ID {
        return Err(Error::Daemon(format!(
            "socket {} serves build {}, but this client is {}; stop the old daemon and retry",
            config.daemon.socket_path.display(),
            health.build_id,
            crate::BUILD_ID
        )));
    }
    Ok(Some(health))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigOverrides;

    #[test]
    fn daemon_start_preserves_all_resolved_settings_and_workspace() {
        let temp = tempfile::TempDir::new().expect("home");
        let mut config = Config::load_layers(
            None,
            None,
            &ConfigOverrides {
                workspace: Some("client-a".to_owned()),
                ..ConfigOverrides::default()
            },
            temp.path(),
        )
        .expect("config");
        config.context.max_injected_tokens = 4_321;
        config.web.port = 12_436;
        config.judge.model = "local-judge".to_owned();
        let snapshot = configuration_snapshot(&config).expect("snapshot");
        assert_eq!(
            Config::load_resolved(snapshot.path()).expect("resolved"),
            config
        );
        let command = daemon_command(Path::new("/tmp/engramd"), &config, snapshot.path());
        assert_eq!(
            command.get_args().next(),
            Some(std::ffi::OsStr::new("--resolved-config"))
        );
        let environment: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            environment[std::ffi::OsStr::new("ENGRAM_WORKSPACE")],
            Some(std::ffi::OsStr::new("client-a"))
        );
        assert_eq!(
            environment[std::ffi::OsStr::new("ENGRAM_DB_PATH")],
            Some(config.storage.db_path.as_os_str())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                snapshot
                    .as_file()
                    .metadata()
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
