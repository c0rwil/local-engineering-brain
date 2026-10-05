use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use engram::config::{Config, ConfigOverrides};

#[derive(Debug, Parser)]
#[command(name = "engramd", version, about = "Engram daemon")]
struct Cli {
    #[arg(long)]
    workspace: Option<String>,
    /// Private resolved configuration snapshot supplied by a sibling client.
    #[arg(long, hide = true, conflicts_with = "config")]
    resolved_config: Option<PathBuf>,
    /// Use this configuration file instead of the user configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override the canonical `SQLite` database path.
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Override the local Unix socket path.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("engramd: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> engram::Result<()> {
    let overrides = ConfigOverrides {
        workspace: cli.workspace,
        db_path: cli.db,
        local_only: None,
    };
    let mut config = if let Some(path) = cli.resolved_config {
        Config::load_resolved(&path)?
    } else if let Some(path) = cli.config {
        Config::load_layers(
            Some(&path),
            None,
            &overrides,
            &std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from),
        )?
    } else {
        Config::load_for(None, &overrides)?
    };
    if let Some(socket) = cli.socket {
        config.daemon.socket_path = socket;
    }
    // Per-repository settings merge the user layer of the home this daemon
    // started in; a resolved snapshot or explicit `--config` does not record it.
    config.config_home = std::env::var_os("HOME").map(PathBuf::from);
    config.validate()?;

    // One daemon per database, for the whole life of this process: taken
    // before migrations and job recovery, released only after the runtime and
    // every thread that could still write have stopped.
    let lock = engram::lifecycle::DaemonLock::acquire(
        &config.storage.db_path,
        engram::lifecycle::start_wait(),
    )?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("engramd-runtime")
        .build()
        .map_err(|error| engram::Error::Daemon(format!("failed to start runtime: {error}")))?;
    let served = runtime.block_on(engram::daemon::serve_with_lock(config, &lock, async {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = terminate => {}
        }
    }));
    // Blocking work that outlived the drain must not keep the process (and
    // the lock) alive indefinitely.
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    drop(lock);
    served
}
