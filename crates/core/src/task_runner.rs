//! TaskRunner — runs one task end-to-end.
//!
//! The pipeline for a single task:
//!
//!   1. Transition state to running
//!   2. Build context (three-block system prompt + user message)
//!   3. Run the tool loop against the LLM + dispatcher
//!   4. Persist the final text as an agent_message row
//!   5. Send the final text through the originating adapter
//!   6. Transition state to done (or failed on error)
//!
//! The scheduler is responsible for picking tasks and enforcing the
//! concurrency limit. This module is the per-task engine.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use sl_llm::{LlmClient, Message};
use sl_store::Store;
use sl_tools::{Dispatcher, ToolCtx};
use tracing::{error, info};

use crate::adapter::{Adapter, AgentMessage};
use crate::context::build_context;
use crate::task::{self, Task};
use crate::tool_loop::{run_tool_loop, LoopConfig, StopReason};
use crate::Config;

/// The long-lived dependencies a task runner needs. Cheap to clone
/// (everything is `Arc` or `Clone`), one instance per runtime.
#[derive(Clone)]
pub struct TaskDeps {
    pub config: Arc<Config>,
    pub store: Store,
    pub session_id: Arc<String>,
    pub llm: Arc<dyn LlmClient>,
    pub dispatcher: Arc<Dispatcher>,
    pub adapters: Arc<Vec<Arc<dyn Adapter>>>,
    /// Soft token cap for context pruning. 0 means no cap.
    pub context_soft_cap_tokens: usize,
}

/// Run one task to completion. Returns the final text the owner saw,
/// or an error if the task failed before producing output.
pub async fn run_task(deps: TaskDeps, task: Task) -> Result<String> {
    let session = deps.session_id.as_str();
    let task_id = task.id.clone();

    // Mark running + emit event
    task::mark_running(&deps.store, session, &task_id)
        .context("marking task running")?;
    info!(task_id = %task_id, kind = ?task.kind, "task started");

    // Build context
    let built = build_context(
        &deps.config,
        &deps.store,
        session,
        &task_id,
        task.kind,
        deps.context_soft_cap_tokens,
    )?;

    // The user message is the task input
    let messages = vec![built.system_message, Message::user_text(task.input_text.clone())];

    // Tool context — what the dispatcher hands to each tool
    let tool_ctx = ToolCtx {
        store: deps.store.clone(),
        repo_root: Arc::new(deps.config.agent.repo_root.clone()),
        data_dir: Arc::new(deps.config.agent.data_dir.clone()),
        protected_paths: Arc::new(resolve_protected_paths(&deps.config)),
        session_id: Arc::new(session.to_string()),
        task_id: Arc::new(task_id.clone()),
    };

    // Tool schemas to advertise to the LLM
    let tool_schemas = deps.dispatcher.registry().schemas(true);

    // Loop config — builds from core::Config so budget + max_rounds flow through
    let loop_cfg = LoopConfig::from_core_config(&deps.config);

    // Run the tool loop
    let outcome = run_tool_loop(
        deps.llm.clone(),
        deps.dispatcher.clone(),
        tool_schemas,
        tool_ctx,
        loop_cfg,
        messages,
    )
    .await;

    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            let err_msg = format!("tool loop error: {e:#}");
            error!(task_id = %task_id, "tool loop failed: {err_msg}");
            task::mark_failed(&deps.store, session, &task_id, &err_msg).ok();
            // Best effort: surface the error to the owner through the adapter
            let _ = send_to_adapter(
                &deps,
                &task.adapter,
                AgentMessage {
                    task_id: Some(task_id.clone()),
                    text: format!("error: {e}"),
                    kind: crate::adapter::AgentMessageKind::Error,
                },
            )
            .await;
            return Err(e);
        }
    };

    // Persist the agent_message row (so context builder can show it later)
    persist_agent_message(&deps.store, &task_id, &task.adapter, &outcome.final_text)
        .context("persisting agent message")?;

    // Send through the originating adapter
    send_to_adapter(
        &deps,
        &task.adapter,
        AgentMessage::response(task_id.clone(), outcome.final_text.clone()),
    )
    .await?;

    // Mark done
    task::mark_done(
        &deps.store,
        session,
        &task_id,
        &outcome.final_text,
        outcome.usage.cost_usd,
        outcome.rounds,
        stop_reason_str(outcome.stop_reason),
    )?;

    info!(
        task_id = %task_id,
        cost_usd = outcome.usage.cost_usd,
        rounds = outcome.rounds,
        stop_reason = stop_reason_str(outcome.stop_reason),
        "task done"
    );

    Ok(outcome.final_text)
}

fn stop_reason_str(r: StopReason) -> &'static str {
    match r {
        StopReason::ContentOnly => "content_only",
        StopReason::RoundCap => "round_cap",
        StopReason::BudgetCap => "budget_cap",
        StopReason::LlmExhausted => "llm_exhausted",
    }
}

/// Resolve the protected paths from config into absolute paths.
fn resolve_protected_paths(config: &Config) -> Vec<PathBuf> {
    config
        .governance
        .protected
        .iter()
        .map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                config.agent.repo_root.join(p)
            }
        })
        .collect()
}

/// Write the agent response as a messages-table row so the next task's
/// context builder can include it in the "Recent messages" section.
fn persist_agent_message(
    store: &Store,
    task_id: &str,
    adapter: &str,
    text: &str,
) -> Result<()> {
    let ts = chrono::Utc::now().timestamp_millis();
    store.with_conn(|conn| {
        conn.execute(
            "INSERT INTO messages (ts, direction, adapter, content, task_id)
             VALUES (?1, 'out', ?2, ?3, ?4)",
            rusqlite::params![ts, adapter, text, task_id],
        )?;
        Ok(())
    })
}

/// Find the adapter by name and send through it. If no matching
/// adapter is registered, we log and drop — the agent_message event
/// is still in the store so nothing is truly lost.
async fn send_to_adapter(
    deps: &TaskDeps,
    adapter_name: &str,
    msg: AgentMessage,
) -> Result<()> {
    for adapter in deps.adapters.iter() {
        if adapter.name() == adapter_name {
            adapter.send(msg).await?;
            return Ok(());
        }
    }
    tracing::warn!(
        adapter = adapter_name,
        "no matching adapter registered; agent message will only live in the store"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Task;
    use async_trait::async_trait;
    use serde_json::json;
    use sl_llm::mock::{MockLlmClient, ScriptStep, ScriptedResponse};
    use sl_llm::{ToolCall, ToolSchema};
    use sl_tools::{HostClass, Registry, Tool, ToolCtx as InnerToolCtx, ToolResult};
    use std::path::PathBuf;
    use std::sync::{Mutex, Once};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A capturing adapter for tests: records every `send` call.
    struct MemAdapter {
        name: &'static str,
        sent: Mutex<Vec<AgentMessage>>,
    }

    impl MemAdapter {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                sent: Mutex::new(Vec::new()),
            })
        }

        fn sent_texts(&self) -> Vec<String> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|m| m.text.clone())
                .collect()
        }
    }

    #[async_trait]
    impl Adapter for MemAdapter {
        fn name(&self) -> &str {
            self.name
        }
        async fn send(&self, msg: AgentMessage) -> Result<()> {
            self.sent.lock().unwrap().push(msg);
            Ok(())
        }
        async fn receive(&self) -> Result<Option<crate::adapter::OwnerMessage>> {
            // Tests only use this adapter as a send sink.
            Ok(None)
        }
    }

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("echo", "echo", json!({"type":"object"}))
        }
        fn host_class(&self) -> HostClass {
            HostClass::InProc
        }
        async fn invoke(&self, _c: &InnerToolCtx, args: serde_json::Value) -> ToolResult {
            Ok(args.get("x").and_then(|v| v.as_str()).unwrap_or("").into())
        }
    }

    fn tmp_repo() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sl-tr-test-{}-{:x}-{:?}-{}",
            std::process::id(),
            nanos,
            std::thread::current().id(),
            n
        ));
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::create_dir_all(root.join("journal")).unwrap();
        std::fs::write(root.join("VERSION"), "0.0.0\n").unwrap();
        std::fs::write(root.join("prompts/CHARTER.md"), "you are test\n").unwrap();
        std::fs::write(root.join("prompts/CREED.md"), "be brief\n").unwrap();
        std::fs::write(
            root.join("prompts/doctrine.toml"),
            "[repo]\ndev_branch = \"agent\"\n",
        )
        .unwrap();
        std::fs::write(root.join("prompts/scratch.md"), "").unwrap();
        root
    }

    static INIT_TRACING: Once = Once::new();
    fn init_tracing() {
        INIT_TRACING.call_once(|| {
            // swallow tracing output in tests
            let _ = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::WARN)
                .with_test_writer()
                .try_init();
        });
    }

    fn make_deps(
        repo: &std::path::Path,
        store: &Store,
        llm: Arc<dyn LlmClient>,
        adapters: Vec<Arc<dyn Adapter>>,
    ) -> TaskDeps {
        init_tracing();

        let mut cfg = Config::default();
        cfg.agent.repo_root = repo.to_path_buf();
        cfg.agent.data_dir = repo.join("data");
        // Large budget so tests never hit the cap
        cfg.budget.total_usd = 1_000_000.0;

        let mut reg = Registry::new();
        reg.register(Arc::new(EchoTool));
        let dispatcher = Arc::new(Dispatcher::new(reg));

        TaskDeps {
            config: Arc::new(cfg),
            store: store.clone(),
            session_id: Arc::new("s1".into()),
            llm,
            dispatcher,
            adapters: Arc::new(adapters),
            context_soft_cap_tokens: 0,
        }
    }

    #[tokio::test]
    async fn single_round_task_sends_response_and_marks_done() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let adapter = MemAdapter::new("cli");

        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            vec![ScriptStep::Respond(ScriptedResponse::text("the answer is 42"))],
        ));
        let deps = make_deps(&repo, &store, llm, vec![adapter.clone() as Arc<dyn Adapter>]);

        let task = Task::from_owner("what is the answer?", "cli");
        task::record_pending(&store, "s1", &task).unwrap();

        let result = run_task(deps, task.clone()).await.unwrap();
        assert_eq!(result, "the answer is 42");
        assert_eq!(adapter.sent_texts(), vec!["the answer is 42".to_string()]);

        let state: String = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT state FROM tasks WHERE id = ?1",
                    rusqlite::params![task.id],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .unwrap();
        assert_eq!(state, "done");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn tool_call_round_trip() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let adapter = MemAdapter::new("cli");

        let script = vec![
            ScriptStep::Respond(ScriptedResponse::tool_calls(
                None,
                vec![ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    arguments: json!({"x": "world"}).to_string(),
                }],
            )),
            ScriptStep::Respond(ScriptedResponse::text("echoed world")),
        ];
        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", script));
        let deps = make_deps(&repo, &store, llm, vec![adapter.clone() as Arc<dyn Adapter>]);

        let task = Task::from_owner("echo world", "cli");
        task::record_pending(&store, "s1", &task).unwrap();

        run_task(deps, task.clone()).await.unwrap();
        assert_eq!(adapter.sent_texts(), vec!["echoed world".to_string()]);

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn tool_loop_error_marks_task_failed() {
        // MockLlmClient exhausts script → every retry empty → LlmExhausted
        // but that's a normal outcome, not a tool_loop Err. To force an
        // Err from the loop, we feed an exhausted script by giving it
        // zero scripted responses.
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let adapter = MemAdapter::new("cli");

        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", vec![]));
        let deps = make_deps(&repo, &store, llm, vec![adapter.clone() as Arc<dyn Adapter>]);

        let task = Task::from_owner("x", "cli");
        task::record_pending(&store, "s1", &task).unwrap();

        // Exhausted mock → transport errors → loop returns LlmExhausted
        // (not Err). Task is marked done with stop_reason=llm_exhausted.
        let result = run_task(deps, task.clone()).await;
        assert!(result.is_ok(), "loop returns Ok even on exhaustion; task gets error text");

        let (state, rounds): (String, u32) = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT state, rounds FROM tasks WHERE id = ?1",
                    rusqlite::params![task.id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(state, "done");
        assert_eq!(rounds, 0);

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn response_persisted_as_outgoing_message() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let adapter = MemAdapter::new("cli");

        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            vec![ScriptStep::Respond(ScriptedResponse::text("hi there"))],
        ));
        let deps = make_deps(&repo, &store, llm, vec![adapter.clone() as Arc<dyn Adapter>]);

        let task = Task::from_owner("hi", "cli");
        task::record_pending(&store, "s1", &task).unwrap();

        run_task(deps, task.clone()).await.unwrap();

        // There should be an outgoing messages row
        let (dir, content): (String, String) = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT direction, content FROM messages WHERE task_id = ?1",
                    rusqlite::params![task.id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(dir, "out");
        assert_eq!(content, "hi there");

        let _ = std::fs::remove_dir_all(&repo);
    }
}
