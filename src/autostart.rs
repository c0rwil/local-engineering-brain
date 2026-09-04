//! Shared local daemon auto-start used by edge binaries.

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
    let mut child = Command::new(&daemon)
        .args(["--db", config.storage.db_path.to_string_lossy().as_ref()])
        .args([
            "--socket",
            config.daemon.socket_path.to_string_lossy().as_ref(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
