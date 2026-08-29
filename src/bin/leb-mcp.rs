use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use leb::config::{Config, ConfigOverrides};
use leb::mcp::McpServer;

#[derive(Debug, Parser)]
#[command(name = "leb-mcp", version, about = "LEB MCP compatibility adapter")]
struct Cli {
    /// Override the canonical database path used when auto-starting `lebd`.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Override the local daemon Unix socket.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Require an already running daemon instead of starting the sibling `lebd`.
    #[arg(long)]
    no_autostart: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("leb-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> leb::Result<()> {
    let mut config = Config::load_for(
        None,
        &ConfigOverrides {
            db_path: cli.db,
            local_only: None,
        },
    )?;
    if let Some(socket) = cli.socket {
        config.daemon.socket_path = socket;
    }
    config.validate()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| leb::Error::Daemon(format!("failed to start MCP runtime: {error}")))?;
    runtime.block_on(async {
        if !cli.no_autostart {
            leb::autostart::ensure_daemon(&config).await?;
        }
        McpServer::new(
            config.daemon.socket_path,
            Duration::from_millis(config.daemon.request_timeout_ms),
            usize::try_from(config.daemon.max_request_bytes).map_err(|_| {
                leb::Error::Config("MCP request limit exceeds this platform".to_owned())
            })?,
        )
        .serve_stdio()
        .await
    })
}
