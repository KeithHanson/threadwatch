mod install;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Threadwatch: Rust bot reporting and htmx inspector")]
struct Cli {
    /// Load the selected environment file before starting the runtime.
    #[arg(long, global = true)]
    env_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the dashboard and ingestion API.
    Serve,
    /// Collect one bridge's activity and deliver it to the server.
    Collect {
        #[arg(long)]
        once: bool,
    },
    /// Install, enable, and restart an unprivileged systemd service (requires root).
    InstallService {
        #[arg(value_enum)]
        role: Role,
        #[arg(long)]
        user: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Role {
    Server,
    Collector,
}

fn execute() -> Result<()> {
    let cli = Cli::parse();
    if let Command::InstallService { role, user } = &cli.command {
        let env = cli
            .env_file
            .as_ref()
            .context("--env-file is required to install a service")?;
        return install::install(*role, env, user);
    }
    // dotenv is loaded before Tokio creates any worker threads.
    if let Some(path) = &cli.env_file {
        dotenvy::from_path_override(path).context("Could not load selected environment file")?;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match cli.command {
        Command::Serve => runtime.block_on(threadwatch::server::run(
            threadwatch::server::Config::from_env()?,
        )),
        Command::Collect { once } => runtime.block_on(threadwatch::collector::run(
            threadwatch::collector::Config::from_env()?,
            once,
        )),
        Command::InstallService { .. } => unreachable!(),
    }
}

fn main() {
    if let Err(error) = execute() {
        // Error contexts are deliberately operational; never print config values or backtraces.
        eprintln!("Threadwatch: {error}");
        std::process::exit(1);
    }
}
