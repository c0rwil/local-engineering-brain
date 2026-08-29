use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use leb::config::{Config, ConfigOverrides};

#[derive(Debug, Parser)]
#[command(name = "leb-proxy", version, about = "LEB-compatible inference proxy")]
struct Cli {
    /// Override the runtime base URL.
    #[arg(long)]
    runtime: Option<String>,

    /// Permit a non-loopback runtime for this proxy process. Remote URLs also
    /// require --no-local-only and HTTPS.
    #[arg(long)]
    allow_remote_runtime: bool,

    /// Disable the local-only network policy for this process. This flag alone
    /// does not permit a remote proxy runtime.
    #[arg(long)]
    no_local_only: bool,

    /// Override the loopback proxy bind address.
    #[arg(long)]
    bind: Option<String>,

    /// Override the local daemon Unix socket.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Override the canonical database path (must match the daemon that owns
    /// the socket).
    #[arg(long)]
    db: Option<PathBuf>,
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
    let overrides = ConfigOverrides {
        db_path: cli.db.clone(),
        ..ConfigOverrides::default()
    };
    let config = Config::load_for(None, &overrides)?;
    let config = apply_cli(config, cli)?;
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

fn apply_cli(mut config: Config, cli: Cli) -> leb::Result<Config> {
    if let Some(runtime) = cli.runtime {
        config.proxy.runtime_url = runtime;
    }
    if cli.allow_remote_runtime {
        config.proxy.allow_remote_runtime = true;
    }
    if cli.no_local_only {
        config.network.local_only = false;
    }
    if let Some(bind) = cli.bind {
        config.proxy.bind = bind;
    }
    if let Some(socket) = cli.socket {
        config.daemon.socket_path = socket;
    }
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_cli_requires_both_independent_opt_ins() {
        let remote = "https://runtime.example.com";

        let cli = Cli::try_parse_from(["leb-proxy", "--runtime", remote, "--no-local-only"])
            .expect("CLI");
        let error = apply_cli(Config::default(), cli)
            .expect_err("broad network opt-in alone must not allow remote runtime");
        assert!(error.to_string().contains("allow_remote_runtime"));

        let cli = Cli::try_parse_from(["leb-proxy", "--runtime", remote, "--allow-remote-runtime"])
            .expect("CLI");
        let error = apply_cli(Config::default(), cli)
            .expect_err("proxy opt-in alone must not disable local-only mode");
        assert!(error.to_string().contains("network.local_only = false"));

        let cli = Cli::try_parse_from([
            "leb-proxy",
            "--runtime",
            remote,
            "--allow-remote-runtime",
            "--no-local-only",
        ])
        .expect("CLI");
        let config = apply_cli(Config::default(), cli).expect("both opt-ins");
        assert!(config.proxy.allow_remote_runtime);
        assert!(!config.network.local_only);
        assert_eq!(config.proxy.runtime_url, remote);
    }
}
