use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use patina_core::{Config, Paths};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;

mod doctor;
mod run;

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
        /// The preview listener's port, on loopback. 0 picks a free one. Set preview_port = 0 in config.toml to
        /// turn the preview off
        #[arg(long)]
        preview_port: Option<u16>,
        #[arg(long)]
        config_dir: Option<PathBuf>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "human")]
        log_format: LogFormat,
    },
    /// Run a team or one agent on a project from the terminal, printing its events and asking for approvals here
    Run {
        /// The project folder: the top level of a git repository
        #[arg(long)]
        project: PathBuf,
        /// The team to run, or a single agent
        #[arg(long, alias = "agent")]
        team: String,
        /// Skip approvals. Accepted only when a sandbox is active
        #[arg(long)]
        yes: bool,
        /// Print each event as a line of JSON instead of readable text
        #[arg(long)]
        json: bool,
        #[arg(long)]
        max_steps: Option<u32>,
        #[arg(long)]
        max_tokens: Option<u64>,
        #[arg(long)]
        max_cost_usd: Option<f64>,
        #[arg(long)]
        max_wall_clock_s: Option<u64>,
        #[arg(long)]
        config_dir: Option<PathBuf>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
        prompt: String,
    },
    /// Internal: restrict this process with Landlock, then run a program. Used by the Landlock sandbox
    #[cfg(target_os = "linux")]
    #[command(name = "sandbox-exec", hide = true)]
    SandboxExec {
        #[arg(long)]
        rw: Vec<PathBuf>,
        #[arg(long)]
        ro: Vec<PathBuf>,
        #[arg(long)]
        dev: Vec<PathBuf>,
        #[arg(long)]
        block_tcp: bool,
        #[arg(long)]
        bind_port: Option<u16>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Check git, the sandbox, the container runtime, ports, config files and leftover worktrees
    Doctor {
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
        #[arg(long)]
        config_dir: Option<PathBuf>,
        #[arg(long)]
        data_dir: Option<PathBuf>,
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

fn main() -> Result<ExitCode> {
    let command = Cli::parse().command;
    // The sandbox helper runs before any thread exists: the restriction covers this process and all it starts.
    #[cfg(target_os = "linux")]
    if let Command::SandboxExec {
        rw,
        ro,
        dev,
        block_tcp,
        bind_port,
        command,
    } = command
    {
        let policy = patina_tools::sandbox::landlock::Policy {
            rw,
            ro,
            dev,
            block_tcp,
            bind_port,
        };
        let why = patina_tools::sandbox::landlock::exec_restricted(&policy, std::path::Path::new(&command[0]), &command[1..]);
        eprintln!("patina: {why}");
        // 126: the command was found but could not be run.
        return Ok(ExitCode::from(126));
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_command(command))
}

async fn run_command(command: Command) -> Result<ExitCode> {
    match command {
        #[cfg(target_os = "linux")]
        Command::SandboxExec { .. } => unreachable!("handled before the runtime starts"),
        Command::Run {
            project,
            team,
            yes,
            json,
            max_steps,
            max_tokens,
            max_cost_usd,
            max_wall_clock_s,
            config_dir,
            data_dir,
            prompt,
        } => {
            // Logs go to stderr and only warnings, so stdout carries nothing but the run.
            let filter =
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
            let paths = open_paths(config_dir, data_dir)?;
            let args = run::RunArgs {
                project,
                team,
                prompt,
                yes,
                json,
                max_steps,
                max_tokens,
                max_cost_usd,
                max_wall_clock_s,
            };
            return Ok(ExitCode::from(run::run(paths, args).await?));
        }
        Command::Doctor {
            json,
            config_dir,
            data_dir,
        } => {
            // Doctor only looks: the folders are not created here.
            let paths = Paths::resolve(config_dir.as_deref(), data_dir.as_deref())
                .context("cannot determine config and data folders; pass --config-dir and --data-dir")?;
            let report = doctor::report(&paths).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                report.print(&mut std::io::stdout().lock())?;
            }
            return Ok(if report.healthy() { ExitCode::SUCCESS } else { ExitCode::from(1) });
        }
        Command::Version => {
            println!("patina {}", env!("CARGO_PKG_VERSION"));
        }
        Command::ResetPassword { config_dir, data_dir } => {
            let paths = open_paths(config_dir, data_dir)?;
            let db = patina_store::Db::open(&paths.database_file())?;
            let token = patina_server::reset_password(&db).await?;
            println!("Password cleared and all sessions signed out.\nSetup token: {token}");
            println!("If the daemon is running, open it in a browser and enter this token to set a new password.");
        }
        Command::Serve {
            bind,
            port,
            preview_port,
            config_dir,
            data_dir,
            log_format,
        } => {
            init_logging(log_format);
            serve(bind, port, preview_port, config_dir, data_dir).await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn open_paths(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> Result<Paths> {
    let paths = Paths::resolve(config_dir.as_deref(), data_dir.as_deref())
        .context("cannot determine config and data folders; pass --config-dir and --data-dir")?;
    paths.ensure().context("cannot create config or data folder")?;
    Ok(paths)
}

async fn serve(
    bind: Option<IpAddr>,
    port: Option<u16>,
    preview_port: Option<u16>,
    config_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
) -> Result<()> {
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
    // The preview gets its own origin on a second port, always on loopback. Remote access goes through a tunnel
    // such as `tailscale serve`, as for the app. A port that is taken turns the preview off, not the daemon.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let configured = state.config.preview_port;
    let preview = match preview_port.or((configured != 0).then_some(configured)) {
        Some(p) => match patina_server::bind(SocketAddr::new(IpAddr::from([127, 0, 0, 1]), p)).await {
            Ok(l) => {
                println!("Preview is served at http://{}", l.local_addr()?);
                let mut rx = stop_rx.clone();
                Some(tokio::spawn(patina_server::serve_preview(l, state.clone(), async move {
                    let _ = rx.wait_for(|stop| *stop).await;
                })))
            }
            Err(e) => {
                tracing::warn!(port = p, error = %e, "the preview port is not free; the preview is off");
                None
            }
        },
        None => None,
    };
    patina_server::serve(listener, state, async move {
        shutdown_signal().await;
        let _ = stop_tx.send(true);
    })
    .await?;
    if let Some(p) = preview {
        let _ = p.await;
    }
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
