//! Shared local daemon auto-start used by edge binaries.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::Config;
use crate::daemon::DaemonClient;
use crate::error::{Error, Result, io_error_for_cli};

/// Environment variable through which an auto-started `engramd` learns which
/// process chain started it. On macOS the daemon inherits privacy (TCC)
/// access from that chain, so `engram doctor` and `engram daemon status`
/// report it.
pub const LAUNCHER_ENV: &str = "ENGRAM_DAEMON_LAUNCHER";

/// Ancestor processes recorded in the launcher description.
const MAX_LAUNCHER_ANCESTORS: usize = 4;

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
    start_daemon(&client, config, &launcher_description()).await
}

async fn start_daemon(client: &DaemonClient, config: &Config, launcher: &str) -> Result<()> {
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
        .env(LAUNCHER_ENV, launcher)
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
        if matching_health(client, config).await?.is_some() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(Error::Daemon(format!(
        "auto-started engramd did not become ready at {}",
        config.daemon.socket_path.display()
    )))
}

/// Outcome of [`restart_daemon`].
#[derive(Clone, Debug, Serialize)]
pub struct RestartReport {
    /// Whether a running daemon answered and was stopped.
    pub stopped_previous: bool,
    /// Process id of the daemon that was stopped, when it reported one.
    pub previous_pid: Option<u32>,
    /// Process id of the daemon now serving the socket.
    pub pid: Option<u32>,
    /// Process chain that started the serving daemon.
    pub launcher: Option<String>,
    /// False when another client auto-started the serving daemon first; it
    /// then runs with that client's environment and privacy access.
    pub started_by_this_process: bool,
}

/// Gracefully stop the daemon that serves this configuration, wait until it
/// has finished in-flight requests, background jobs and any judge call and
/// has exited, then start a new daemon from this process. The new daemon
/// inherits this process's environment and, on macOS, its privacy access:
/// the fix when the old daemon was started by an app that may not read a
/// repository folder.
///
/// # Errors
///
/// Returns an error when the socket serves another database, the old daemon
/// does not exit within `wait`, or the new daemon cannot start.
pub async fn restart_daemon(config: &Config, wait: Duration) -> Result<RestartReport> {
    let client = DaemonClient::new(
        &config.daemon.socket_path,
        Duration::from_millis(config.daemon.request_timeout_ms.min(2_000)),
    );
    let previous = client.health().await.ok();
    if let Some(previous) = &previous {
        if previous.database != config.storage.db_path {
            return Err(Error::Daemon(format!(
                "socket {} belongs to database {}, not {}; not stopping it",
                config.daemon.socket_path.display(),
                previous.database.display(),
                config.storage.db_path.display()
            )));
        }
        client.shutdown().await?;
        wait_for_exit(&config.daemon.socket_path, previous.pid, wait).await?;
    }
    let launcher = launcher_description();
    if matching_health(&client, config).await?.is_none() {
        start_daemon(&client, config, &launcher).await?;
    }
    let health = client.health().await?;
    Ok(RestartReport {
        stopped_previous: previous.is_some(),
        previous_pid: previous.and_then(|previous| previous.pid),
        pid: health.pid,
        started_by_this_process: health.launcher.as_deref() == Some(launcher.as_str()),
        launcher: health.launcher,
    })
}

/// Wait until the stopping daemon has exited. It removes its socket only
/// after draining, so a vanished socket (or, for daemons that report a pid,
/// a vanished process) means the old process is done.
async fn wait_for_exit(socket: &Path, pid: Option<u32>, wait: Duration) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        if !socket.exists() || pid.is_some_and(|pid| !process_alive(pid)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let process =
                pid.map_or_else(|| "engramd".to_owned(), |pid| format!("engramd pid {pid}"));
            return Err(Error::Daemon(format!(
                "{process} is still finishing in-flight work after {} s; nothing was started. \
                 Run `engram daemon restart` again later",
                wait.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Describe the process starting a daemon and its ancestors, for example
/// `engram hook session-start (pid 41) <- /bin/zsh <- /Applications/…/codex`.
/// Only plain subcommand words are kept from the arguments, never options,
/// paths, prompts or queries.
#[must_use]
pub fn launcher_description() -> String {
    let program = crate::error::current_process_name();
    let words = subcommand_words(std::env::args().skip(1));
    let mut description = if words.is_empty() {
        program
    } else {
        format!("{program} {}", words.join(" "))
    };
    let _ = write!(description, " (pid {})", std::process::id());
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..MAX_LAUNCHER_ANCESTORS {
        if pid <= 1 {
            break;
        }
        let Some((parent, command)) = process_parent_and_command(pid) else {
            break;
        };
        description.push_str(" <- ");
        description.push_str(&command);
        pid = parent;
    }
    description
}

/// Up to three plain words (letters, digits, `-`) that do not start with `-`.
fn subcommand_words(arguments: impl Iterator<Item = String>) -> Vec<String> {
    arguments
        .filter(|argument| {
            !argument.starts_with('-')
                && (1..=32).contains(&argument.len())
                && argument
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
        .take(3)
        .collect()
}

/// Parent pid and executable of `pid`, from `ps`.
fn process_parent_and_command(pid: u32) -> Option<(u32, String)> {
    let output = Command::new("ps")
        .args(["-o", "ppid=", "-o", "comm=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let line = text.lines().next()?.trim();
    let (parent, command) = line.split_once(char::is_whitespace)?;
    Some((parent.trim().parse().ok()?, command.trim().to_owned()))
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

    #[test]
    fn launcher_description_names_this_process_and_keeps_no_paths() {
        let description = launcher_description();
        assert!(
            description.contains(&format!("(pid {})", std::process::id())),
            "{description}"
        );
        assert!(
            description.starts_with(&crate::error::current_process_name()),
            "{description}"
        );
    }

    #[test]
    fn launcher_keeps_subcommands_but_no_options_paths_or_text() {
        let words = |arguments: &[&str]| {
            subcommand_words(arguments.iter().map(|argument| (*argument).to_owned()))
        };
        assert_eq!(
            words(&["--db", "/tmp/brain.sqlite", "daemon", "restart"]),
            ["daemon", "restart"]
        );
        assert_eq!(
            words(&["hook", "session-start", "--codex"]),
            ["hook", "session-start"]
        );
        assert_eq!(
            words(&["context", "fix the login bug", "--path", "/repo"]),
            ["context"]
        );
    }

    #[test]
    fn process_liveness_probe_sees_this_process() {
        assert!(process_alive(std::process::id()));
    }
}
