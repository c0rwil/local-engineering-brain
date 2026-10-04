//! Daemon process lifecycle: one daemon per database, and clean handover.
//!
//! `engramd` holds an exclusive lock on `<database>.daemon.lock` for its whole
//! life, from before migrations and job recovery until the process exits. A
//! second daemon for the same database waits briefly for the lock and then
//! exits, so two daemons never hold the database at once: a new build cannot
//! migrate under an old one, and job recovery never requeues work that a
//! still-running daemon is executing.
//!
//! Builds older than the lock do not take it. A new daemon therefore also
//! refuses to start while such a daemon still has the database open, and
//! clients find a daemon's pid through its socket even when its health
//! response does not report one.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result, io_error};

/// API error code a daemon returns while it finishes in-flight work before it
/// exits. Clients see it as the `draining: ` prefix of a daemon error.
pub const DRAINING_CODE: &str = "draining";

/// How long a starting daemon waits for an exiting one by default.
const DEFAULT_START_WAIT: Duration = Duration::from_secs(10);

/// Overrides [`start_wait`] in milliseconds (operators and tests).
pub const START_WAIT_ENV: &str = "ENGRAM_DAEMON_START_WAIT_MS";

/// Executable names of Engram daemons, current and legacy.
const DAEMON_NAMES: [&str; 2] = ["engramd", "lebd"];

/// How long a starting daemon waits for a previous daemon to exit.
#[must_use]
pub fn start_wait() -> Duration {
    std::env::var(START_WAIT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(DEFAULT_START_WAIT, Duration::from_millis)
}

/// Whether an error is a daemon's "finishing in-flight work" refusal.
#[must_use]
pub fn is_draining(error: &Error) -> bool {
    draining_detail(error).is_some()
}

/// The daemon's explanation when `error` is a `draining` refusal.
#[must_use]
pub fn draining_detail(error: &Error) -> Option<&str> {
    let Error::Daemon(message) = error else {
        return None;
    };
    message
        .strip_prefix(DRAINING_CODE)
        .and_then(|rest| rest.strip_prefix(':'))
        .map(str::trim)
}

/// The exclusive lifetime lock of the daemon that owns one database.
#[derive(Debug)]
pub struct DaemonLock {
    database: PathBuf,
    path: PathBuf,
    _file: File,
}

impl DaemonLock {
    /// `<database>.daemon.lock`, next to the database.
    #[must_use]
    pub fn path_for(database: &Path) -> PathBuf {
        let mut name = database
            .file_name()
            .map(std::ffi::OsStr::to_os_string)
            .unwrap_or_default();
        name.push(".daemon.lock");
        database.with_file_name(name)
    }

    /// Take the lock, waiting up to `wait` while another daemon holds it.
    /// The holder's pid is written into the file for diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error naming the holder when the lock stays held, or an
    /// I/O error when the lock file cannot be created.
    pub fn acquire(database: &Path, wait: Duration) -> Result<Self> {
        let path = Self::path_for(database);
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
                .map_err(|source| io_error(&path, source))?
        };
        let deadline = Instant::now() + wait;
        loop {
            // `flock(2)` through std (stable since Rust 1.89). The pinned
            // toolchain is 1.96 and the crate already needs 1.88 for let
            // chains; the declared `rust-version` is behind both.
            #[allow(clippy::incompatible_msrv)]
            let attempt = file.try_lock();
            match attempt {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    let holder = read_holder(&mut file)
                        .map_or_else(String::new, |pid| format!(" (pid {pid})"));
                    return Err(Error::Daemon(format!(
                        "another engramd{holder} holds {}: it is serving or finishing \
                         in-flight work for this database; it exits on its own after \
                         `engram daemon stop`, then a new daemon can start",
                        path.display()
                    )));
                }
                // Some network file systems implement no `flock`. Serve
                // without the lock rather than not at all; the socket probe
                // still refuses a serving daemon.
                Err(std::fs::TryLockError::Error(source)) if lock_unsupported(&source) => {
                    eprintln!(
                        "engramd: {} cannot be locked ({source}); one daemon per database \
                         is not enforced on this file system",
                        path.display()
                    );
                    break;
                }
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(io_error(&path, source));
                }
            }
        }
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)).map(drop))
            .and_then(|()| writeln!(file, "{}", std::process::id()))
            .and_then(|()| file.flush())
            .map_err(|source| io_error(&path, source))?;
        Ok(Self {
            database: database.to_path_buf(),
            path,
            _file: file,
        })
    }

    /// The database this lock guards.
    #[must_use]
    pub fn database(&self) -> &Path {
        &self.database
    }

    /// The lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn lock_unsupported(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Unsupported
        || matches!(error.raw_os_error(), Some(code) if code == libc::ENOLCK || code == libc::EOPNOTSUPP)
}

fn read_holder(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(32).read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

/// Pids of daemons other than this process that have `database` open
/// without holding its lock: builds older than the lock, still serving or
/// finishing in-flight work.
#[must_use]
pub fn unlocked_daemons_holding(database: &Path) -> Vec<u32> {
    processes_holding(database, &DAEMON_NAMES)
}

/// Pids of processes other than this one, running a program named in
/// `names`, that have `database` open.
fn processes_holding(database: &Path, names: &[&str]) -> Vec<u32> {
    let own = std::process::id();
    let mut pids = database_holders(database, names);
    pids.retain(|pid| *pid != own);
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Wait up to `wait` for daemons that predate the lock to release
/// `database`.
///
/// # Errors
///
/// Returns an error naming them when they still hold it after `wait`.
pub fn wait_for_unlocked_daemons(database: &Path, wait: Duration) -> Result<()> {
    wait_for_holders(database, &DAEMON_NAMES, wait)
}

fn wait_for_holders(database: &Path, names: &[&str], wait: Duration) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        let holders = processes_holding(database, names);
        if holders.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let pids = holders
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::Daemon(format!(
                "engramd pid {pids} from a build without the daemon lock still has {} open \
                 while it finishes in-flight work; start the daemon again after it exits",
                database.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(target_os = "macos")]
fn database_holders(database: &Path, names: &[&str]) -> Vec<u32> {
    // `lsof -F pc` prints `p<pid>` then `c<command>` for every process that
    // has the file open. `-a -c NAME` limits the scan to processes whose
    // command starts with one of the names, which keeps it to tens of
    // milliseconds; the exact names are checked below. lsof exits non-zero
    // when a name matches nothing, so only its output counts.
    let mut lsof = Command::new("lsof");
    lsof.arg("-a");
    for name in names {
        lsof.args(["-c", name]);
    }
    lsof.args(["-F", "pc", "--"]).arg(database);
    // Every daemon start runs this; lsof can stall on an unresponsive
    // network mount, and an unknown answer must not keep a daemon down.
    let Some(output) = bounded_output(&mut lsof, Duration::from_secs(5)) else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    let mut pid = None;
    for line in String::from_utf8_lossy(&output).lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse::<u32>().ok();
        } else if let Some(command) = line.strip_prefix('c')
            && names.contains(&command)
            && let Some(pid) = pid
        {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(target_os = "linux")]
fn database_holders(database: &Path, names: &[&str]) -> Vec<u32> {
    let Ok(target) = std::fs::canonicalize(database) else {
        return Vec::new();
    };
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids = Vec::new();
    for process in processes.flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let command = std::fs::read_to_string(process.path().join("comm")).unwrap_or_default();
        if !names.contains(&command.trim()) {
            continue;
        }
        let Ok(descriptors) = std::fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        if descriptors.flatten().any(|descriptor| {
            std::fs::read_link(descriptor.path()).is_ok_and(|path| path == target)
        }) {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn database_holders(_database: &Path, _names: &[&str]) -> Vec<u32> {
    Vec::new()
}

/// Standard output of `command`, or `None` when it cannot start or has not
/// finished within `limit` (it is killed then). For commands with short
/// output only: the pipe is read after the command exits.
#[cfg(target_os = "macos")]
fn bounded_output(command: &mut Command, limit: Duration) -> Option<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = Vec::new();
    child.stdout.take()?.read_to_end(&mut output).ok()?;
    Some(output)
}

/// What a process id refers to now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessState {
    /// No such process, or only an unreaped zombie of it.
    Exited,
    /// An Engram daemon executable is running under this pid.
    Daemon,
    /// Another program runs under this pid: a reused pid, or a daemon hosted
    /// inside another process (tests).
    Other(String),
    /// `ps` could not be run.
    Unknown,
}

/// Inspect `pid` with `ps` (no signals are sent).
#[must_use]
pub fn process_state(pid: u32) -> ProcessState {
    let Ok(output) = Command::new("ps")
        .args(["-o", "stat=", "-o", "comm=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return ProcessState::Unknown;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return ProcessState::Exited;
    };
    let (status, command) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    if status.starts_with('Z') {
        return ProcessState::Exited;
    }
    let command = command.trim();
    let name = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command);
    if DAEMON_NAMES.contains(&name) {
        ProcessState::Daemon
    } else {
        ProcessState::Other(command.to_owned())
    }
}

/// The pid of the process listening on `socket`, from the connected peer's
/// credentials. Works with daemons that do not report a pid themselves.
pub async fn serving_pid(socket: &Path) -> Option<u32> {
    let stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let pid = stream.peer_cred().ok()?.pid()?;
    u32::try_from(pid).ok()
}

/// Whether a process accepts connections on `socket`.
pub async fn is_listening(socket: &Path) -> bool {
    tokio::net::UnixStream::connect(socket).await.is_ok()
}

/// Identity of the socket file, to notice when another daemon replaced it.
#[must_use]
pub fn socket_inode(socket: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(socket)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// How a wait for a stopping daemon ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitWait {
    /// The daemon process exited (or, without a pid, its socket was removed).
    Exited,
    /// Without a pid to watch: another daemon replaced the socket. The old
    /// listener is gone; whether its process still runs is unknown.
    Replaced,
    /// Still running when the wait ended.
    Running,
}

/// Wait up to `wait` for the daemon that served `socket` (as `pid`, socket
/// identity `inode`) to exit. A pid is watched only when it is an Engram
/// daemon executable; otherwise the socket is.
pub async fn wait_for_exit(
    socket: &Path,
    pid: Option<u32>,
    inode: Option<(u64, u64)>,
    wait: Duration,
) -> ExitWait {
    let watched = pid.filter(|pid| process_state(*pid) == ProcessState::Daemon);
    let deadline = Instant::now() + wait;
    loop {
        if let Some(pid) = watched {
            // A pid now running another program was reused after the exit.
            if matches!(
                process_state(pid),
                ProcessState::Exited | ProcessState::Other(_)
            ) {
                return ExitWait::Exited;
            }
        } else {
            match socket_inode(socket) {
                None => return ExitWait::Exited,
                Some(current) if inode.is_some_and(|inode| inode != current) => {
                    return ExitWait::Replaced;
                }
                Some(_) => {}
            }
        }
        if Instant::now() >= deadline {
            return ExitWait::Running;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_waits_then_names_the_holder() {
        let temp = tempfile::TempDir::new().expect("temp");
        let database = temp.path().join("brain.sqlite");
        let held = DaemonLock::acquire(&database, Duration::ZERO).expect("first lock");
        assert_eq!(held.path(), temp.path().join("brain.sqlite.daemon.lock"));
        let started = Instant::now();
        let error = DaemonLock::acquire(&database, Duration::from_millis(300))
            .expect_err("second lock must wait and fail");
        assert!(started.elapsed() >= Duration::from_millis(300));
        let message = error.to_string();
        assert!(
            message.contains(&format!("pid {}", std::process::id())),
            "{message}"
        );
        drop(held);
        // Another test thread's child, between fork and exec, can still share
        // the lock file's descriptor for a moment; the lock frees once it execs.
        DaemonLock::acquire(&database, Duration::from_secs(5)).expect("released lock is free");
    }

    #[test]
    fn draining_errors_are_recognized() {
        assert!(is_draining(&Error::Daemon(
            "draining: engramd pid 7 is finishing in-flight work".to_owned()
        )));
        assert!(!is_draining(&Error::Daemon("drainingly: other".to_owned())));
        assert!(!is_draining(&Error::Daemon(
            "internal: draining".to_owned()
        )));
    }

    #[test]
    fn process_states_distinguish_daemons_exits_and_other_programs() {
        assert!(matches!(
            process_state(std::process::id()),
            ProcessState::Other(_)
        ));
        let mut child = Command::new("sleep").arg("30").spawn().expect("sleep");
        let pid = child.id();
        assert!(matches!(process_state(pid), ProcessState::Other(_)));
        child.kill().expect("kill");
        // An unreaped zombie counts as exited.
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_state(pid) != ProcessState::Exited {
            assert!(
                Instant::now() < deadline,
                "killed child must read as exited"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait().expect("reap");
        assert_eq!(process_state(pid), ProcessState::Exited);
    }

    #[test]
    fn serving_pid_reads_the_listener_process_from_the_socket() {
        let temp = tempfile::TempDir::new().expect("temp");
        let socket = temp.path().join("s.sock");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let _listener = tokio::net::UnixListener::bind(&socket).expect("listen");
            assert_eq!(serving_pid(&socket).await, Some(std::process::id()));
        });
        std::fs::remove_file(&socket).expect("remove socket");
        assert_eq!(runtime.block_on(serving_pid(&socket)), None);
    }

    #[test]
    fn exit_wait_follows_the_socket_when_no_daemon_pid_is_known() {
        let temp = tempfile::TempDir::new().expect("temp");
        let socket = temp.path().join("s.sock");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let listener = std::os::unix::net::UnixListener::bind(&socket).expect("listen");
            let inode = socket_inode(&socket);
            // This test process is not a daemon executable, so the socket is watched.
            let running = wait_for_exit(
                &socket,
                Some(std::process::id()),
                inode,
                Duration::from_millis(200),
            )
            .await;
            assert_eq!(running, ExitWait::Running);

            drop(listener);
            // Keep the old file (and so its inode) alive under another name so
            // the replacement cannot reuse the inode number.
            std::fs::rename(&socket, temp.path().join("old.sock")).expect("move old socket");
            let _replacement = std::os::unix::net::UnixListener::bind(&socket).expect("rebind");
            assert_eq!(
                wait_for_exit(&socket, None, inode, Duration::from_secs(1)).await,
                ExitWait::Replaced
            );
            std::fs::remove_file(&socket).expect("remove replacement");
            assert_eq!(
                wait_for_exit(&socket, None, inode, Duration::from_secs(1)).await,
                ExitWait::Exited
            );
        });
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn processes_holding_the_database_are_found_by_program_name() {
        let temp = tempfile::TempDir::new().expect("temp");
        let database = temp.path().join("brain.sqlite");
        std::fs::write(&database, b"").expect("database file");
        assert!(unlocked_daemons_holding(&database).is_empty());
        // `tail -f` keeps the file open the way a draining daemon from a
        // build without the lock keeps the database open. (macOS refuses to
        // run a renamed copy of a system program, so it keeps its name.)
        let mut holder = Command::new("tail")
            .arg("-f")
            .arg(&database)
            .stdout(Stdio::null())
            .spawn()
            .expect("hold database");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = Vec::new();
        while found.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            found = processes_holding(&database, &["tail"]);
        }
        let daemons = unlocked_daemons_holding(&database);
        let refused = wait_for_holders(&database, &["tail"], Duration::from_millis(100));
        holder.kill().expect("kill holder");
        holder.wait().expect("reap holder");
        assert_eq!(found, vec![holder.id()]);
        assert!(
            daemons.is_empty(),
            "only daemon programs count: {daemons:?}"
        );
        let message = refused.expect_err("held database must refuse").to_string();
        assert!(message.contains(&holder.id().to_string()), "{message}");
        assert!(wait_for_holders(&database, &["tail"], Duration::from_secs(5)).is_ok());
    }
}
