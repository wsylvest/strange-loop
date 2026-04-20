//! strange-loop binary — M1 surface.
//!
//! Subcommands:
//!   - `version`              print the version
//!   - `self-test`            open runtime, emit events, verify store
//!   - `chat [message]`       run one or more tasks through the tool loop
//!   - `events`               tail the events table
//!   - `charter status`       show on-disk / on-record charter hash
//!   - `charter approve`      record current charter as baseline

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sl_core::{logging, Adapter, Config, Runtime, Scheduler, Task, TaskDeps};

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
    /// Chat with the agent. If `message` is provided, runs one task and
    /// exits. If stdin is piped, reads the piped text as a single
    /// message. Otherwise opens an interactive REPL.
    Chat {
        /// Inline message. If set, this is treated as the sole task
        /// and the process exits after it completes.
        message: Option<String>,

        /// Use the mock LLM client instead of the real OpenRouter
        /// client. Useful for CI and for trying the plumbing without
        /// an API key.
        #[arg(long)]
        mock: bool,
    },
    /// Tail the events table.
    Events {
        /// Filter by event type (e.g. "llm_usage", "tool_call").
        #[arg(long)]
        event_type: Option<String>,
        /// Filter by task_id.
        #[arg(long)]
        task: Option<String>,
        /// Maximum rows to return, newest first.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Emit JSON lines instead of human-readable text.
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
        Command::Chat { message, mock } => {
            // Tokio runtime for the async subcommands.
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cmd_chat(config, message, mock))
        }
        Command::Events {
            event_type,
            task,
            limit,
            json,
        } => cmd_events(config, event_type, task, limit, json),
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

async fn cmd_chat(
    config: Config,
    one_shot: Option<String>,
    mock: bool,
) -> Result<ExitCode> {
    use sl_adapter_cli::{CliAdapter, CliMode};
    use sl_core::task;
    use sl_tools::{Dispatcher, Registry};

    // Open the runtime (this also performs the charter integrity check).
    let runtime = Runtime::open(config.clone())?;
    let session_id = runtime.session_id.clone();

    // Build the LLM client — real or mock per flag / env.
    let llm = build_llm_client(&config, mock)?;

    // Register the M1 core tools.
    let mut registry = Registry::new();
    registry.register(Arc::new(sl_tools::fs::FsRead));
    registry.register(Arc::new(sl_tools::fs::FsList));
    registry.register(Arc::new(sl_tools::fs::FsWrite));
    registry.register(Arc::new(sl_tools::fs::FsDelete));
    let dispatcher = Arc::new(Dispatcher::new(registry));

    // CLI adapter.
    let adapter_mode = if one_shot.is_some() {
        // Explicit message on the command line — act like piped mode
        // (one message, then done).
        CliMode::Piped
    } else {
        CliMode::Auto
    };
    let adapter: Arc<dyn Adapter> = Arc::new(CliAdapter::new(
        adapter_mode,
        runtime.store.clone(),
        session_id.clone(),
    ));
    let adapter_mode_resolved = match adapter_mode {
        CliMode::Auto => {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                CliMode::Interactive
            } else {
                CliMode::Piped
            }
        }
        other => other,
    };

    // TaskDeps is cheap to clone; we use it both for the scheduler and
    // for the one-shot inline path.
    let deps = TaskDeps {
        config: Arc::new(config.clone()),
        store: runtime.store.clone(),
        session_id: Arc::new(session_id.as_str().to_string()),
        llm,
        dispatcher,
        adapters: Arc::new(vec![adapter.clone()]),
        context_soft_cap_tokens: config.tool_loop.context_soft_cap_tokens as usize,
    };

    let sched_handle = Scheduler::start(
        deps,
        config.tool_loop.max_concurrent_tasks,
        32, // pending queue capacity
    );

    // Crash recovery: if a prior process died with tasks in `running`,
    // flip them to `failed` so the UI is honest about what happened.
    let recovered = sl_core::task::recover_crashed_tasks(&runtime.store)?;
    if recovered > 0 {
        eprintln!("note: recovered {recovered} task(s) stuck in running from a prior crash");
    }

    // Seed the first message, either from the CLI arg or by pulling
    // one from the adapter.
    let first_msg = match one_shot {
        Some(m) => Some(m),
        None => {
            let msg = adapter.receive().await?;
            msg.map(|m| m.text)
        }
    };

    if let Some(text) = first_msg {
        let t = Task::from_owner(text, "cli");
        task::record_pending(&runtime.store, session_id.as_str(), &t)
            .context("recording first task")?;
        sched_handle
            .submit
            .submit(t)
            .await
            .context("submitting first task")?;
    }

    // In piped / one-shot mode, close and drain. In interactive mode,
    // loop reading more messages from the adapter.
    match adapter_mode_resolved {
        CliMode::Piped => {
            sched_handle.submit.close();
            sched_handle.loop_handle.await.ok();
        }
        CliMode::Interactive => {
            // Loop: read next message from adapter, submit to scheduler.
            // EOF (Ctrl-D) returns None → we close and drain.
            while let Some(msg) = adapter.receive().await? {
                let t = Task::from_owner(msg.text, "cli");
                task::record_pending(&runtime.store, session_id.as_str(), &t)?;
                sched_handle.submit.submit(t).await?;
            }
            sched_handle.submit.close();
            sched_handle.loop_handle.await.ok();
        }
        CliMode::Auto => unreachable!("resolved above"),
    }

    Ok(ExitCode::SUCCESS)
}

fn build_llm_client(
    config: &Config,
    force_mock: bool,
) -> Result<Arc<dyn sl_llm::LlmClient>> {
    if force_mock {
        return Ok(mock_client());
    }
    let key = std::env::var(&config.llm.api_key_env).ok();
    match key {
        Some(k) if !k.is_empty() => {
            let cfg = sl_llm::openrouter::OpenRouterConfig::new(k, &config.llm.default_model);
            let client = sl_llm::openrouter::OpenRouterClient::new(cfg)?;
            Ok(Arc::new(client))
        }
        _ => {
            eprintln!(
                "note: {} not set; using mock LLM (pass --mock to silence this)",
                config.llm.api_key_env
            );
            Ok(mock_client())
        }
    }
}

/// A small mock used by default when no API key is configured.
/// Canned response — echoes that it heard the message.
fn mock_client() -> Arc<dyn sl_llm::LlmClient> {
    use sl_llm::mock::{MockLlmClient, ScriptStep, ScriptedResponse};
    let responses: Vec<ScriptStep> = (0..32)
        .map(|_| {
            ScriptStep::Respond(ScriptedResponse::text(
                "(mock) I received your message. \
                 Configure OPENROUTER_API_KEY for a real response.",
            ))
        })
        .collect();
    Arc::new(MockLlmClient::new("mock", responses))
}

fn cmd_events(
    config: Config,
    event_type: Option<String>,
    task: Option<String>,
    limit: usize,
    json: bool,
) -> Result<ExitCode> {
    let db_path = config.agent.data_dir.join("strange-loop.db");
    if !db_path.exists() {
        anyhow::bail!(
            "no database at {:?}; run `strange-loop self-test` first to initialize",
            db_path
        );
    }
    let store = sl_store::Store::open(&db_path)?;

    // Build query dynamically based on filters.
    let mut sql = String::from(
        "SELECT ts, event_type, task_id, session_id, payload
         FROM events WHERE 1=1",
    );
    if event_type.is_some() {
        sql.push_str(" AND event_type = :et");
    }
    if task.is_some() {
        sql.push_str(" AND task_id = :tid");
    }
    sql.push_str(" ORDER BY ts DESC LIMIT :lim");

    let rows: Vec<(i64, String, Option<String>, String, String)> = store
        .with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();
            if let Some(et) = &event_type {
                params.push((":et", et));
            }
            if let Some(tid) = &task {
                params.push((":tid", tid));
            }
            let lim_val = limit as i64;
            params.push((":lim", &lim_val));
            let rows = stmt
                .query_map(params.as_slice(), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .context("querying events")?;

    if json {
        for (ts, et, tid, sid, payload) in rows {
            let v = serde_json::json!({
                "ts": ts,
                "event_type": et,
                "task_id": tid,
                "session_id": sid,
                "payload": serde_json::from_str::<serde_json::Value>(&payload)
                    .unwrap_or(serde_json::Value::String(payload)),
            });
            println!("{}", serde_json::to_string(&v)?);
        }
    } else {
        for (ts, et, tid, _sid, payload) in rows.iter().rev() {
            let dt = chrono::DateTime::from_timestamp_millis(*ts)
                .map(|d| d.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_else(|| ts.to_string());
            let preview: String = payload.chars().take(120).collect();
            println!(
                "[{dt}] {et} task={} {preview}",
                tid.as_deref().unwrap_or("-")
            );
        }
    }

    Ok(ExitCode::SUCCESS)
}

fn cmd_charter(config: Config, action: CharterAction) -> Result<ExitCode> {
    use sl_core::governance;

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

// Suppress an unused-import false positive that shows up when the
// compiler can't tell LlmClient is used through a trait object.
#[allow(dead_code)]
fn _ensure_traits_imported(c: Arc<dyn sl_llm::LlmClient>) -> &'static str {
    let _ = c;
    "compiled"
}
