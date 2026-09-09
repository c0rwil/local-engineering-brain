use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use engram::config::{Config, ConfigOverrides};

#[derive(Debug, Parser)]
#[command(name = "engramd", version, about = "Engram daemon")]
struct Cli {
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
        db_path: cli.db,
        local_only: None,
    };
    let mut config = Config::load_for(None, &overrides)?;
    if let Some(socket) = cli.socket {
        config.daemon.socket_path = socket;
    }
    config.validate()?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("engramd-runtime")
        .build()
        .map_err(|error| engram::Error::Daemon(format!("failed to start runtime: {error}")))?;
    runtime.block_on(engram::daemon::serve_until(config, async {
        let _ = tokio::signal::ctrl_c().await;
    }))
}
