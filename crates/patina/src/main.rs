use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use patina_core::{Config, Paths};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "patina", version, about = "Self-hosted agent workbench")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Human,
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon
    Serve {
        #[arg(long)]
        bind: Option<IpAddr>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        config_dir: Option<PathBuf>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "human")]
        log_format: LogFormat,
    },
    /// Print version and build info
    Version,
}

fn init_logging(format: LogFormat) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let b = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Human => b.init(),
        LogFormat::Json => b.json().init(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Version => {
            println!("patina {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Serve {
            bind,
            port,
            config_dir,
            data_dir,
            log_format,
        } => {
            init_logging(log_format);
            serve(bind, port, config_dir, data_dir).await
        }
    }
}

async fn serve(
    bind: Option<IpAddr>,
    port: Option<u16>,
    config_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
) -> Result<()> {
    let paths = Paths::resolve(config_dir.as_deref(), data_dir.as_deref())
        .context("cannot determine config and data folders; pass --config-dir and --data-dir")?;
    paths
        .ensure()
        .context("cannot create config or data folder")?;
    let cfg = Config::load_or_create(&paths.config_file())?;
    // Opening the database runs migrations; the connection is kept for later milestones.
    let _db = patina_store::open(&paths.database_file())?;

    let ip: IpAddr = match bind {
        Some(ip) => ip,
        None => cfg
            .bind
            .parse()
            .with_context(|| format!("invalid bind address {:?}", cfg.bind))?,
    };
    let addr = SocketAddr::new(ip, port.unwrap_or(cfg.port));
    if !ip.is_loopback() {
        anyhow::bail!(
            "refusing to bind {ip}: non-loopback binds need authentication, which arrives in M1"
        );
    }
    let listener = patina_server::bind(addr)
        .await
        .with_context(|| format!("cannot bind {addr}"))?;
    println!("Patina is running at http://{}", listener.local_addr()?);
    patina_server::serve(listener, shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
