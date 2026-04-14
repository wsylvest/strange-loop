//! strange-loop binary — M0 entry point.
//!
//! Current subcommands:
//!   - `version`              print the version
//!   - `self-test`            open runtime, emit events, verify store
//!   - `charter approve`      (stub in M0, implemented in M3)
//!
//! Future subcommands (tracked in ROADMAP): chat, events, replay,
//! status, cancel, restart, review, bg, creed, backup.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sl_core::{logging, Config, Runtime};

#[derive(Parser)]
#[command(
    name = "strange-loop",
    version,
    about = "A self-modifying LLM agent runtime.",
    long_about = "strange-loop is a self-modifying agent runtime. See docs/TREATISE.md for the why."
)]
struct Cli {
    /// Path to strange-loop.toml. Defaults to ./strange-loop.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the version and exit.
    Version,
    /// Run the built-in self-test: open the store, emit an event, verify.
    SelfTest {
        /// Print a machine-readable JSON report instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Charter management subcommands.
    Charter {
        #[command(subcommand)]
        action: CharterAction,
    },
}

#[derive(Subcommand)]
enum CharterAction {
    /// Approve the current on-disk charter file as the new baseline.
    Approve,
    /// Show the current charter hash (on-disk and on-record).
    Status,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    // Handle version before any IO — useful for CI smoke tests.
    if let Command::Version = cli.command {
        println!("strange-loop {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from("strange-loop.toml"));
    let config = Config::load(&config_path)
        .with_context(|| format!("loading config from {:?}", config_path))?;

    // Logging goes to data_dir/logs — create it before runtime open.
    let _guard = logging::init(&config.agent.data_dir)?;

    match cli.command {
        Command::Version => unreachable!("handled above"),
        Command::SelfTest { json } => cmd_self_test(config, json),
        Command::Charter { action } => cmd_charter(config, action),
    }
}

fn cmd_self_test(config: Config, json: bool) -> Result<ExitCode> {
    let runtime = Runtime::open(config)?;
    let report = runtime.self_test()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("strange-loop self-test: OK");
        println!("  session_id:     {}", report.session_id);
        println!("  cell_backend:   {}", report.cell_backend);
        println!("  db_path:        {}", report.db_path.display());
        println!("  events_written: {}", report.events_written);
    }
    Ok(ExitCode::SUCCESS)
}

fn cmd_charter(config: Config, action: CharterAction) -> Result<ExitCode> {
    use sl_core::governance;

    // Need the store, but not a full Runtime::open — which records a
    // session and performs the charter check itself. For charter
    // management, we open the store directly.
    let db_path = config.agent.data_dir.join("strange-loop.db");
    std::fs::create_dir_all(&config.agent.data_dir)?;
    let store = sl_store::Store::open(&db_path)?;

    let charter_path = if config.governance.charter.is_absolute() {
        config.governance.charter.clone()
    } else {
        config.agent.repo_root.join(&config.governance.charter)
    };

    match action {
        CharterAction::Approve => {
            if !charter_path.exists() {
                anyhow::bail!(
                    "charter file not found at {:?}; nothing to approve",
                    charter_path
                );
            }
            // Read and confirm on stdin.
            let hash = governance::hash_file(&charter_path)?;
            eprintln!(
                "About to approve charter at {:?}\n  hash: {}\n\n\
                 Type 'approve' to confirm:",
                charter_path, hash
            );
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if line.trim() != "approve" {
                eprintln!("aborted.");
                return Ok(ExitCode::from(2));
            }
            let stored = governance::approve_current_charter(&store, &charter_path)?;
            println!("charter approved: {}", stored);
            Ok(ExitCode::SUCCESS)
        }
        CharterAction::Status => {
            let on_disk = if charter_path.exists() {
                Some(governance::hash_file(&charter_path)?)
            } else {
                None
            };
            let on_record = sl_store::kv::get(&store, governance::CHARTER_HASH_KEY)?;
            println!(
                "charter status:\n  path:      {}\n  on-disk:   {}\n  on-record: {}",
                charter_path.display(),
                on_disk.as_deref().unwrap_or("<missing>"),
                on_record.as_deref().unwrap_or("<none>"),
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}
