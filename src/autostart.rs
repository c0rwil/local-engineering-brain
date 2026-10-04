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

/// How long a client waits for a draining daemon to exit before it gives up
/// instead of starting a second daemon.
const CLIENT_DRAIN_WAIT: Duration = Duration::from_secs(10);

/// What answers on the daemon socket.
enum Probe {
    Ready(Box<crate::daemon::HealthResponse>),
    /// A daemon is finishing in-flight work before it exits.
    Draining(String),
    Absent,
}

/// Ensure the sibling `engramd` is healthy, starting it detached when absent.
/// A daemon that is draining is waited out (bounded) rather than raced.
///
/// # Errors
///
/// Returns an error when the daemon cannot be located, started, or made
/// ready, or when a draining daemon does not exit in time.
pub async fn ensure_daemon(config: &Config) -> Result<()> {
    let client = DaemonClient::new(
        &config.daemon.socket_path,
        Duration::from_millis(config.daemon.request_timeout_ms.min(2_000)),
    );
    if wait_out_drain(&client, config, CLIENT_DRAIN_WAIT)
        .await?
        .is_some()
    {
        return Ok(());
    }
    start_daemon(&client, config, &launcher_description()).await
}

/// The serving daemon's health (`Some`) or nothing answering (`None`). A
/// draining daemon is polled until it exits, up to `wait`.
async fn wait_out_drain(
    client: &DaemonClient,
    config: &Config,
    wait: Duration,
) -> Result<Option<crate::daemon::HealthResponse>> {
    let deadline = Instant::now() + wait;
    loop {
        match probe(client, config).await? {
            Probe::Ready(health) => return Ok(Some(*health)),
            Probe::Absent => return Ok(None),
            Probe::Draining(detail) => {
                if Instant::now() >= deadline {
                    return Err(Error::Daemon(format!(
                        "{}: {detail}; it did not exit within {} s, retry shortly",
                        crate::lifecycle::DRAINING_CODE,
                        wait.as_secs()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn probe(client: &DaemonClient, config: &Config) -> Result<Probe> {
    match client.health().await {
        Ok(health) => {
            check_health(&health, config)?;
            Ok(Probe::Ready(Box::new(health)))
        }
        Err(error) => Ok(crate::lifecycle::draining_detail(&error)
            .map_or(Probe::Absent, |detail| Probe::Draining(detail.to_owned()))),
    }
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
    // A daemon that is still exiting holds the lock; the new one waits for
    // it, so `draining` here means "not yet", not "failed".
    for _ in 0..100 {
        if matches!(probe(client, config).await?, Probe::Ready(_)) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(Error::Daemon(format!(
        "auto-started engramd did not become ready at {}",
        config.daemon.socket_path.display()
    )))
}

/// Outcome of [`stop_daemon`].
#[derive(Clone, Debug, Serialize)]
pub struct StopReport {
    /// A daemon answered (serving or already draining) and was asked to stop.
    #[serde(rename = "shutdown_requested")]
    pub stopped: bool,
    /// The daemon's process id, from its health report or its socket.
    pub pid: Option<u32>,
    /// Whether it exited within the wait; `None` when not waited for.
    pub exited: Option<bool>,
    /// Without a pid to watch: another daemon replaced the socket meanwhile.
    pub replaced: bool,
    pub waited_ms: u64,
}

/// Ask the daemon on this configuration's socket to stop, then wait up to
/// `wait` (zero: do not wait) for its process to exit. The pid comes from the
/// health report or, for daemons that do not report one, from the socket's
/// peer credentials, so the wait follows the process even when another
/// client binds a new socket meanwhile.
///
/// # Errors
///
/// Returns an error when the socket serves another database or the stop
/// request fails.
pub async fn stop_daemon(config: &Config, wait: Duration) -> Result<StopReport> {
    let socket = &config.daemon.socket_path;
    let client = DaemonClient::new(
        socket,
        Duration::from_millis(config.daemon.request_timeout_ms.min(2_000)),
    );
    let reported_pid = match client.health().await {
        Ok(health) if health.database != config.storage.db_path => {
            return Err(Error::Daemon(format!(
                "socket {} belongs to database {}, not {}; not stopping it",
                socket.display(),
                health.database.display(),
                config.storage.db_path.display()
            )));
        }
        Ok(health) => health.pid,
        Err(error) => {
            // A draining daemon, or an older build whose health response this
            // client cannot read, is still asked to stop: after an upgrade the
            // socket serves an old build, and stopping it is the point.
            if !crate::lifecycle::is_draining(&error)
                && !crate::lifecycle::is_listening(socket).await
            {
                return Ok(StopReport {
                    stopped: false,
                    pid: None,
                    exited: None,
                    replaced: false,
                    waited_ms: 0,
                });
            }
            None
        }
    };
    let pid = match reported_pid {
        Some(pid) => Some(pid),
        None => crate::lifecycle::serving_pid(socket).await,
    };
    let inode = crate::lifecycle::socket_inode(socket);
    client.shutdown().await?;
    if wait.is_zero() {
        return Ok(StopReport {
            stopped: true,
            pid,
            exited: None,
            replaced: false,
            waited_ms: 0,
        });
    }
    let started = Instant::now();
    let outcome = crate::lifecycle::wait_for_exit(socket, pid, inode, wait).await;
    Ok(StopReport {
        stopped: true,
        pid,
        exited: Some(outcome != crate::lifecycle::ExitWait::Running),
        replaced: outcome == crate::lifecycle::ExitWait::Replaced,
        waited_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
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
    /// A daemon another client started after the old one exited, replaced
    /// once so the new daemon runs with this process's environment.
    pub displaced_pid: Option<u32>,
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
    let stopped = stop_daemon(config, wait).await?;
    if stopped.exited == Some(false) {
        let process = stopped
            .pid
            .map_or_else(|| "engramd".to_owned(), |pid| format!("engramd pid {pid}"));
        return Err(Error::Daemon(format!(
            "{process} is still finishing in-flight work after {} s; nothing was started. \
             Run `engram daemon restart` again later",
            wait.as_secs()
        )));
    }
    let launcher = launcher_description();
    let mut displaced = false;
    let mut displaced_pid = None;
    // Start, then confirm; at most one displacement in between.
    for _ in 0..3 {
        match wait_out_drain(&client, config, CLIENT_DRAIN_WAIT).await? {
            None => start_daemon(&client, config, &launcher).await?,
            Some(health) if health.launcher.as_deref() == Some(launcher.as_str()) => break,
            // Another client started a daemon in the gap, with its own
            // environment and folder access. Replace it once.
            Some(_) if !displaced => {
                displaced = true;
                let stopped_other = stop_daemon(config, wait).await?;
                displaced_pid = stopped_other.pid;
                if stopped_other.exited == Some(false) {
                    break;
                }
            }
            Some(_) => break,
        }
    }
    let health = client.health().await?;
    Ok(RestartReport {
        stopped_previous: stopped.stopped,
        previous_pid: stopped.pid,
        pid: health.pid,
        started_by_this_process: health.launcher.as_deref() == Some(launcher.as_str()),
        launcher: health.launcher,
        displaced_pid,
    })
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
    close_inherited_descriptors(&mut command);
    command
}

/// Highest descriptor number checked when closing inherited descriptors.
const MAX_INHERITED_DESCRIPTOR: i32 = 65_536;

/// A long-lived daemon must hold no descriptor of the process that started
/// it beyond stdio. On macOS Rust creates pipes with `pipe()` and marks them
/// close-on-exec only afterwards, so a daemon spawned while another thread
/// creates a pipe can inherit it and keep it open for its whole life (a hook
/// that waited for EOF on such a pipe hung for over 20 minutes). The child
/// closes every descriptor from 3 up that is not already close-on-exec.
#[allow(unsafe_code)]
fn close_inherited_descriptors(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    // Read in the parent so the child only makes async-signal-safe calls.
    // SAFETY: getdtablesize has no preconditions and touches no memory.
    let limit = unsafe { libc::getdtablesize() }.clamp(3, MAX_INHERITED_DESCRIPTOR);
    let close = move || -> std::io::Result<()> {
        for descriptor in 3..limit {
            // SAFETY: fcntl and close are async-signal-safe system calls on a
            // plain integer. Close-on-exec descriptors, such as the standard
            // library's exec-error pipe, are left for exec to close.
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                // SAFETY: as above; nothing in the child uses the descriptor.
                unsafe { libc::close(descriptor) };
            }
        }
        Ok(())
    };
    // SAFETY: `close` runs between fork and exec, allocates nothing, holds no
    // locks and only calls fcntl and close.
    unsafe { command.pre_exec(close) };
}

/// Refuse a daemon that serves another database, workspace or build.
fn check_health(health: &crate::daemon::HealthResponse, config: &Config) -> Result<()> {
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
    Ok(())
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

    /// A daemon from an older build: its health response lacks fields this
    /// client requires and it reports no pid. On shutdown it stops listening
    /// at once and removes its socket only when it has finished.
    async fn serve_unreadable_health(
        listener: tokio::net::UnixListener,
        socket: std::path::PathBuf,
    ) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1_024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            if request.is_empty() {
                // A probe that only connected.
                continue;
            }
            let shutdown = request.starts_with(b"POST /api/v1/admin/shutdown ");
            let body = if shutdown {
                "null"
            } else {
                r#"{"status":"ok","version":"0.0.1"}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            if shutdown {
                drop(listener);
                tokio::time::sleep(Duration::from_millis(300)).await;
                std::fs::remove_file(&socket).expect("remove socket");
                return;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_reaches_a_daemon_whose_health_this_client_cannot_read() {
        let temp = tempfile::TempDir::new().expect("home");
        let mut config = Config::load_layers(None, None, &ConfigOverrides::default(), temp.path())
            .expect("config");
        config.daemon.socket_path = temp.path().join("old.sock");
        let listener = tokio::net::UnixListener::bind(&config.daemon.socket_path).expect("listen");
        let old_daemon = tokio::spawn(serve_unreadable_health(
            listener,
            config.daemon.socket_path.clone(),
        ));
        assert!(
            DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(2))
                .health()
                .await
                .is_err()
        );

        let report = stop_daemon(&config, Duration::from_secs(10))
            .await
            .expect("stop");
        assert!(report.stopped, "{report:?}");
        // Found through the socket's peer credentials.
        assert_eq!(report.pid, Some(std::process::id()));
        // This test process is no daemon executable, so the socket was
        // watched until the old daemon removed it.
        assert_eq!(report.exited, Some(true));
        assert!(!report.replaced);
        assert!(report.waited_ms >= 200, "{report:?}");
        old_daemon.await.expect("old daemon");

        let absent = stop_daemon(&config, Duration::from_secs(1))
            .await
            .expect("nothing to stop");
        assert!(!absent.stopped);
        assert_eq!(absent.pid, None);
    }
}
