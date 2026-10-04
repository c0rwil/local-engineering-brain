//! Daemon lifecycle with real processes: one daemon per database through
//! stop, drain and handover. Regression for a second daemon starting (and
//! migrating, and recovering jobs) while the first was still draining.
use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use engram::daemon::{DaemonClient, SemanticRebuildRequest};
use engram::effectiveness::{ToolObservationRequest, ToolPhase};
use engram::ids::ProjectId;
use tempfile::TempDir;

struct Fixture {
    temp: TempDir,
}

impl Fixture {
    /// A workspace whose daemon config points semantic rebuilds at an
    /// embeddings endpoint that accepts connections and never answers, so a
    /// rebuild stays in flight for `timeout_ms`. `accepted` signals each
    /// connection it holds.
    fn with_silent_embedder() -> (Self, mpsc::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("embedder listener");
        let port = listener.local_addr().expect("embedder address").port();
        let (accepted_sender, accepted) = mpsc::channel();
        thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
                let _ = accepted_sender.send(());
            }
        });
        let fixture = Self::new(&format!(
            "[semantic]\nprovider = \"local\"\n\
             endpoint = \"http://127.0.0.1:{port}/v1/embeddings\"\n\
             model = \"silent-fixture\"\ntimeout_ms = 3000\n"
        ));
        (fixture, accepted)
    }

    fn new(extra_config: &str) -> Self {
        let temp = TempDir::new().expect("temp dir");
        std::fs::create_dir_all(temp.path().join("project")).expect("project dir");
        std::fs::write(
            temp.path().join("engram.toml"),
            format!(
                "{extra_config}\n[web]\nenabled = false\n\n[watch]\nenabled = false\n\n\
                 [vault]\nenabled = false\n"
            ),
        )
        .expect("config");
        Self { temp }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn database(&self) -> PathBuf {
        self.root().join("brain.sqlite")
    }

    fn socket(&self) -> PathBuf {
        self.root().join("e.sock")
    }

    fn client(&self) -> DaemonClient {
        DaemonClient::new(self.socket(), Duration::from_secs(5))
    }

    /// Start `engramd` directly, the way an auto-start does, with this
    /// workspace's config.
    fn engramd(&self, start_wait_ms: Option<u64>) -> Child {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engramd"));
        command
            .env("HOME", self.root())
            .args([
                "--config",
                self.root().join("engram.toml").to_str().unwrap(),
            ])
            .args(["--db", self.database().to_str().unwrap()])
            .args(["--socket", self.socket().to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(wait) = start_wait_ms {
            command.env(engram::lifecycle::START_WAIT_ENV, wait.to_string());
        }
        command.spawn().expect("spawn engramd")
    }

    fn engram(&self, args: &[&str]) -> Output {
        Command::new(assert_cmd::cargo::cargo_bin!("engram"))
            .env("HOME", self.root())
            .args(["--db", self.database().to_str().unwrap()])
            .args(["--socket", self.socket().to_str().unwrap()])
            .args(args)
            .output()
            .expect("run engram")
    }

    fn register_project(&self) -> ProjectId {
        let output = self.engram(&[
            "status",
            self.root().join("project").to_str().unwrap(),
            "--json",
        ]);
        assert!(output.status.success(), "{}", stderr(&output));
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status");
        ProjectId::parse(status["project"]["id"].as_str().expect("project id")).expect("id")
    }
}

impl Drop for Fixture {
    /// Stop any daemon a failed test left serving this workspace.
    fn drop(&mut self) {
        if self.socket().exists() {
            let _ = self.engram(&["daemon", "stop", "--wait-secs", "10"]);
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn wait_until_listening(socket: &Path, daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        if let Some(status) = daemon.try_wait().expect("poll daemon") {
            let mut message = String::new();
            if let Some(mut stderr) = daemon.stderr.take() {
                let _ = stderr.read_to_string(&mut message);
            }
            panic!("daemon exited before listening ({status}): {message}");
        }
        assert!(Instant::now() < deadline, "daemon readiness timeout");
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_exit(daemon: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = daemon.try_wait().expect("poll daemon") {
            return status;
        }
        assert!(Instant::now() < deadline, "daemon did not exit in time");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Start a daemon that must refuse to start (bounded, so a regression fails
/// instead of hanging); returns its error output.
fn refused_start(fixture: &Fixture, start_wait_ms: u64) -> String {
    let mut daemon = fixture.engramd(Some(start_wait_ms));
    let status = wait_for_exit(&mut daemon, Duration::from_secs(15));
    let mut message = String::new();
    daemon
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut message)
        .expect("daemon stderr");
    assert!(
        !status.success(),
        "a daemon started that must not: {message}"
    );
    message
}

/// Start a semantic rebuild that stays in flight until the silent embedder's
/// timeout, and return once the daemon is blocked on it.
fn rebuild_in_flight(
    fixture: &Fixture,
    project_id: ProjectId,
    accepted: &mpsc::Receiver<()>,
) -> thread::JoinHandle<()> {
    let socket = fixture.socket();
    let rebuild = thread::spawn(move || {
        let client = DaemonClient::new(&socket, Duration::from_secs(60));
        // The silent endpoint makes the rebuild fail after its timeout.
        let _ = runtime()
            .block_on(client.rebuild_semantic_index(&SemanticRebuildRequest { project_id }));
    });
    accepted
        .recv_timeout(Duration::from_secs(15))
        .expect("the rebuild reached the embedder");
    rebuild
}

#[test]
fn draining_daemon_keeps_its_socket_and_no_second_daemon_starts_until_it_exits() {
    let (fixture, accepted) = Fixture::with_silent_embedder();
    let mut first = fixture.engramd(None);
    wait_until_listening(&fixture.socket(), &mut first);
    let project_id = fixture.register_project();
    let rebuild = rebuild_in_flight(&fixture, project_id, &accepted);

    let client = fixture.client();
    let runtime = runtime();
    runtime
        .block_on(client.shutdown())
        .expect("request shutdown");

    // While it drains, the daemon still answers: `draining`, not a refused
    // connection that would invite clients to start another daemon.
    let health = runtime.block_on(client.health());
    assert!(
        matches!(&health, Err(error) if engram::lifecycle::is_draining(error)),
        "{health:?}"
    );
    assert!(first.try_wait().expect("poll").is_none(), "still draining");

    // A second daemon cannot take over the database meanwhile: it waits for
    // the lock, then exits naming the holder.
    let refusal = refused_start(&fixture, 300);
    assert!(refusal.contains("holds"), "{refusal}");
    assert!(refusal.contains(&first.id().to_string()), "{refusal}");
    assert!(
        first.try_wait().expect("poll").is_none(),
        "first still owns it"
    );

    // A client waits out the drain, then auto-starts a replacement daemon.
    let status = fixture.engram(&["daemon", "status", "--json"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let replacement: serde_json::Value = serde_json::from_slice(&status.stdout).expect("health");
    let replacement_pid = replacement["pid"].as_u64().expect("replacement pid");
    assert_ne!(replacement_pid, u64::from(first.id()));
    let exited = wait_for_exit(&mut first, Duration::from_secs(5));
    assert!(
        exited.success(),
        "the first daemon drained and exited cleanly"
    );
    rebuild.join().expect("rebuild client");
    let mut drained = String::new();
    first
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut drained)
        .expect("first stderr");
    assert!(!drained.contains("dropped"), "{drained}");

    let stopped = fixture.engram(&["daemon", "stop", "--json"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    let report: serde_json::Value = serde_json::from_slice(&stopped.stdout).expect("stop json");
    assert_eq!(report["exited"], true, "{report}");
    assert!(!fixture.socket().exists());
}

#[test]
fn stop_waits_until_the_daemon_process_has_exited() {
    let (fixture, accepted) = Fixture::with_silent_embedder();
    let mut daemon = fixture.engramd(None);
    wait_until_listening(&fixture.socket(), &mut daemon);
    let project_id = fixture.register_project();
    let rebuild = rebuild_in_flight(&fixture, project_id, &accepted);

    let started = Instant::now();
    let stopped = fixture.engram(&["daemon", "stop", "--wait-secs", "60", "--json"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    let report: serde_json::Value = serde_json::from_slice(&stopped.stdout).expect("stop json");
    assert_eq!(report["shutdown_requested"], true, "{report}");
    assert_eq!(report["exited"], true, "{report}");
    assert_eq!(report["pid"].as_u64(), Some(u64::from(daemon.id())));
    // The stop returned only after the in-flight rebuild ended and the
    // process was gone (an unreaped child counts as gone).
    assert!(started.elapsed() >= Duration::from_secs(1), "{report}");
    assert!(
        daemon.try_wait().expect("poll").is_some(),
        "stop returned while the daemon was still running"
    );
    rebuild.join().expect("rebuild client");

    let again = fixture.engram(&["daemon", "stop"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(String::from_utf8_lossy(&again.stdout).contains("No daemon is answering"));
}

/// Observations acknowledged at enqueue before the stop request.
const OBSERVED_CALLS: usize = 200;

#[test]
fn tool_observations_acknowledged_before_a_stop_are_written() {
    let fixture = Fixture::new("");
    let mut daemon = fixture.engramd(None);
    wait_until_listening(&fixture.socket(), &mut daemon);
    let project_id = fixture.register_project();
    let client = fixture.client();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        // Bounded concurrency: more simultaneous connections than the
        // listen backlog would be refused before reaching the daemon.
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(16));
        let mut pending = tokio::task::JoinSet::new();
        for call in 0..OBSERVED_CALLS {
            let client = client.clone();
            let project_id = project_id.clone();
            let permit = std::sync::Arc::clone(&permits)
                .acquire_owned()
                .await
                .expect("permit");
            pending.spawn(async move {
                let _permit = permit;
                client
                    .record_tool_observation(&ToolObservationRequest {
                        project_id,
                        session_id: "drain-session".to_owned(),
                        tool_use_id: format!("call-{call}"),
                        task_id: None,
                        context_run_id: None,
                        source: "claude_code".to_owned(),
                        tool_name: "Read".to_owned(),
                        phase: ToolPhase::Post,
                        paths: vec!["src/lib.rs".to_owned()],
                        ranged: None,
                        result_bytes: None,
                        limit_hit: None,
                    })
                    .await
            });
        }
        while let Some(acknowledged) = pending.join_next().await {
            assert!(acknowledged.expect("task").expect("queued").queued);
        }
        client.shutdown().await.expect("request shutdown");
    });
    assert!(wait_for_exit(&mut daemon, Duration::from_secs(30)).success());
    let database = rusqlite::Connection::open(fixture.database()).expect("database");
    let stored: i64 = database
        .query_row("SELECT COUNT(*) FROM tool_observations", [], |row| {
            row.get(0)
        })
        .expect("count");
    assert_eq!(stored, i64::try_from(OBSERVED_CALLS).unwrap());
}

/// Whether process `pid` has `path` open.
fn process_holds(pid: u32, path: &Path) -> bool {
    let target = std::fs::canonicalize(path).expect("canonical path");
    if cfg!(target_os = "linux") {
        return std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| std::fs::read_link(entry.path()).is_ok_and(|open| open == target))
        });
    }
    let listing = Command::new("lsof")
        .args(["-n", "-P", "-F", "n", "-p", &pid.to_string()])
        .output()
        .expect("lsof");
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .any(|name| Path::new(name) == target)
}

#[test]
fn auto_started_daemon_does_not_inherit_the_starters_descriptors() {
    let fixture = Fixture::new("");
    let marker = fixture.root().join("inherited-marker");
    std::fs::write(&marker, b"held by the starter\n").expect("marker");
    // The shell opens descriptor 7 without close-on-exec, like a pipe another
    // thread had just created when the daemon was spawned; the CLI it execs
    // then auto-starts the daemon.
    let started = Command::new("/bin/sh")
        .arg("-c")
        .arg(
            "exec 7<\"$MARKER\"; \
             exec \"$ENGRAM\" --db \"$DB\" --socket \"$SOCKET\" status \"$PROJECT\" --json",
        )
        .env("HOME", fixture.root())
        .env("MARKER", &marker)
        .env("ENGRAM", assert_cmd::cargo::cargo_bin!("engram"))
        .env("DB", fixture.database())
        .env("SOCKET", fixture.socket())
        .env("PROJECT", fixture.root().join("project"))
        .output()
        .expect("start the daemon through the CLI");
    assert!(started.status.success(), "{}", stderr(&started));
    let status = fixture.engram(&["daemon", "status", "--json"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let health: serde_json::Value = serde_json::from_slice(&status.stdout).expect("health");
    let pid = u32::try_from(health["pid"].as_u64().expect("daemon pid")).expect("pid");
    let inherited = process_holds(pid, &marker);
    let stopped = fixture.engram(&["daemon", "stop"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    assert!(
        !inherited,
        "the daemon kept a descriptor of the process that started it"
    );
}

/// Kills a helper process if the test fails before the helper exits.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Database the stand-in for a pre-lock daemon keeps open.
const STAND_IN_DATABASE_ENV: &str = "ENGRAM_TEST_STAND_IN_DATABASE";
/// File whose creation lets the stand-in exit.
const STAND_IN_RELEASE_ENV: &str = "ENGRAM_TEST_STAND_IN_RELEASE";

/// Stand-in for a daemon from a build without the lock. Run as a copy of
/// this test binary named `engramd`, it keeps the database open until it is
/// released. Run as an ordinary test, it does nothing.
#[test]
#[ignore = "helper process for new_daemon_waits_for_a_daemon_without_the_lock"]
fn pre_lock_daemon_stand_in() {
    let (Some(database), Some(release)) = (
        std::env::var_os(STAND_IN_DATABASE_ENV),
        std::env::var_os(STAND_IN_RELEASE_ENV),
    ) else {
        return;
    };
    let release = PathBuf::from(release);
    let _open = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(database)
        .expect("open database");
    std::fs::write(release.with_extension("ready"), b"").expect("ready marker");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !release.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn new_daemon_waits_for_a_daemon_without_the_lock() {
    let fixture = Fixture::new("");
    let old_build = fixture.root().join("old-build");
    std::fs::create_dir_all(&old_build).expect("old build dir");
    let stand_in = old_build.join("engramd");
    std::fs::copy(std::env::current_exe().expect("test binary"), &stand_in)
        .expect("copy the test binary");
    let release = fixture.root().join("release");
    let mut old = KillOnDrop(
        Command::new(&stand_in)
            .args(["--exact", "pre_lock_daemon_stand_in", "--ignored"])
            .env(STAND_IN_DATABASE_ENV, fixture.database())
            .env(STAND_IN_RELEASE_ENV, &release)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the stand-in"),
    );
    let ready = release.with_extension("ready");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready.exists() {
        assert!(
            old.0.try_wait().expect("poll stand-in").is_none(),
            "stand-in exited early"
        );
        assert!(Instant::now() < deadline, "stand-in readiness timeout");
        thread::sleep(Duration::from_millis(20));
    }

    // Such a daemon stops listening as soon as a stop begins and removes its
    // socket file only shortly before it exits. Either way, a new daemon
    // waits and then refuses, before it touches the database.
    for stale_socket in [false, true] {
        if stale_socket {
            drop(std::os::unix::net::UnixListener::bind(fixture.socket()).expect("stale socket"));
        }
        let message = refused_start(&fixture, 300);
        assert!(message.contains("without the daemon lock"), "{message}");
        assert!(message.contains(&old.0.id().to_string()), "{message}");
        assert_eq!(
            std::fs::metadata(fixture.database())
                .expect("database")
                .len(),
            0,
            "the new daemon opened (and migrated) the database"
        );
    }

    std::fs::write(&release, b"").expect("release the stand-in");
    assert!(wait_for_exit(&mut old.0, Duration::from_secs(15)).success());
    let mut new = fixture.engramd(Some(300));
    wait_until_listening(&fixture.socket(), &mut new);
    let stopped = fixture.engram(&["daemon", "stop", "--json"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    assert!(wait_for_exit(&mut new, Duration::from_secs(15)).success());
}
