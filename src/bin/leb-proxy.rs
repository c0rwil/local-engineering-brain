use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use leb::config::{Config, ConfigOverrides};

#[derive(Debug, Parser)]
#[command(
    name = "leb-proxy",
    version,
    about = "LEB-compatible local inference proxy"
)]
struct Cli {
    /// Override the local runtime base URL.
    #[arg(long)]
    runtime: Option<String>,

    /// Override the loopback proxy bind address.
    #[arg(long)]
    bind: Option<String>,

    /// Override the local daemon Unix socket.
    #[arg(long)]
    socket: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("leb-proxy: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> leb::Result<()> {
    let mut config = Config::load_for(None, &ConfigOverrides::default())?;
    if let Some(runtime) = cli.runtime {
        config.proxy.runtime_url = runtime;
    }
    if let Some(bind) = cli.bind {
        config.proxy.bind = bind;
    }
    if let Some(socket) = cli.socket {
        config.daemon.socket_path = socket;
    }
    config.validate()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("leb-proxy-runtime")
        .build()
        .map_err(|error| leb::Error::Daemon(format!("failed to start runtime: {error}")))?;
    runtime.block_on(async {
        leb::autostart::ensure_daemon(&config).await?;
        leb::proxy::serve_until(config, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    })
}
