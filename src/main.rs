mod install;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
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
    /// Install, enable, and restart the unprivileged server service (requires root).
    InstallService {
        #[arg(long)]
        user: String,
    },
}

fn execute() -> Result<()> {
    let cli = Cli::parse();
    if let Command::InstallService { user } = &cli.command {
        let env = cli
            .env_file
            .as_ref()
            .context("--env-file is required to install a service")?;
        return install::install(env, user);
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
