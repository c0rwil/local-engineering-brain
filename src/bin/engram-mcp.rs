use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use engram::config::{Config, ConfigOverrides};
use engram::mcp::McpServer;

#[derive(Debug, Parser)]
#[command(
    name = "engram-mcp",
    version,
    about = "Engram MCP compatibility adapter"
)]
struct Cli {
    /// Isolated workspace served by this MCP connection.
    #[arg(long)]
    workspace: Option<String>,
    /// Override the canonical database path used when auto-starting `engramd`.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Override the local daemon Unix socket.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Require an already running daemon instead of starting the sibling `engramd`.
    #[arg(long)]
    no_autostart: bool,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("engram-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> engram::Result<()> {
    let mut config = Config::load_for_current_dir(&ConfigOverrides {
        workspace: cli.workspace,
        db_path: cli.db,
        local_only: None,
    })?;
    if let Some(socket) = cli.socket {
        config.daemon.socket_path = socket;
    }
    config.validate()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| engram::Error::Daemon(format!("failed to start MCP runtime: {error}")))?;
    runtime.block_on(async {
        if !cli.no_autostart {
            engram::autostart::ensure_daemon(&config).await?;
        }
        McpServer::new(
            config.daemon.socket_path,
            Duration::from_millis(config.daemon.request_timeout_ms),
            usize::try_from(config.daemon.max_request_bytes).map_err(|_| {
                engram::Error::Config("MCP request limit exceeds this platform".to_owned())
            })?,
        )
        .serve_stdio()
        .await
    })
}
