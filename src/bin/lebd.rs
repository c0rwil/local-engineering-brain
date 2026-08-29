use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use leb::config::{Config, ConfigOverrides};

#[derive(Debug, Parser)]
#[command(name = "lebd", version, about = "Local Engineering Brain daemon")]
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
            eprintln!("lebd: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> leb::Result<()> {
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
        .thread_name("lebd-runtime")
        .build()
        .map_err(|error| leb::Error::Daemon(format!("failed to start runtime: {error}")))?;
    runtime.block_on(leb::daemon::serve_until(config, async {
        let _ = tokio::signal::ctrl_c().await;
    }))
}
