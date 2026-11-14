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
    /// Forget the owner password and print a new one-time setup token
    ResetPassword {
        #[arg(long)]
        config_dir: Option<PathBuf>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Print version and build info
    Version,
}

fn init_logging(format: LogFormat) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
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
        Command::ResetPassword { config_dir, data_dir } => {
            let paths = open_paths(config_dir, data_dir)?;
            let db = patina_store::Db::open(&paths.database_file())?;
            let token = patina_server::reset_password(&db).await?;
            println!("Password cleared and all sessions signed out.\nSetup token: {token}");
            println!("If the daemon is running, open it in a browser and enter this token to set a new password.");
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

fn open_paths(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> Result<Paths> {
    let paths = Paths::resolve(config_dir.as_deref(), data_dir.as_deref())
        .context("cannot determine config and data folders; pass --config-dir and --data-dir")?;
    paths.ensure().context("cannot create config or data folder")?;
    Ok(paths)
}

async fn serve(bind: Option<IpAddr>, port: Option<u16>, config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> Result<()> {
    let paths = open_paths(config_dir, data_dir)?;
    let cfg = Config::load_or_create(&paths.config_file())?;
    let db = patina_store::Db::open(&paths.database_file())?;
    let key = patina_store::secrets::master_key(&paths.data_dir)?;
    let secrets = patina_store::secrets::Secrets::new(db.clone(), key);

    let ip: IpAddr = match bind {
        Some(ip) => ip,
        None => cfg.bind.parse().with_context(|| format!("invalid bind address {:?}", cfg.bind))?,
    };
    let addr = SocketAddr::new(ip, port.unwrap_or(cfg.port));
    if !ip.is_loopback() {
        // Closed by default: a network-reachable daemon needs a password and an explicitly allowed host name.
        if !patina_server::has_password(&db).await? {
            anyhow::bail!("refusing to bind {ip}: set the owner password on localhost first");
        }
        let listed = cfg
            .allowed_hosts
            .iter()
            .any(|h| !matches!(h.as_str(), "localhost" | "127.0.0.1" | "[::1]"));
        if !listed {
            anyhow::bail!("refusing to bind {ip}: add the host name clients will use to allowed_hosts in config.toml");
        }
    }
    let setup_token = patina_server::ensure_setup_token(&db).await?;
    let state = patina_server::state::AppState::new(paths, cfg, db, secrets, patina_server::genai_factory());
    let listener = patina_server::bind(addr).await.with_context(|| format!("cannot bind {addr}"))?;
    println!("Patina is running at http://{}", listener.local_addr()?);
    if let Some(t) = setup_token {
        println!("First run: open that address and enter this one-time setup token to set your password:\n  {t}");
    }
    patina_server::serve(listener, state, shutdown_signal()).await?;
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
