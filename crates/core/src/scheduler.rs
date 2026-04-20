//! Scheduler — the front door for work.
//!
//! Owns the pending queue, enforces `max_concurrent_tasks`, spawns
//! TaskRunner futures on the tokio runtime, and tracks handles so
//! callers can await completion or cancel.
//!
//! Design:
//!   - A bounded channel (`tasks_tx`) acts as the pending queue.
//!   - A semaphore sized to `max_concurrent_tasks` gates concurrency.
//!   - The scheduler's `run()` future drains the channel, acquires
//!     a permit, spawns a task, and keeps going. It exits when the
//!     channel closes AND all active permits have been returned.
//!   - `submit()` is how adapters / the binary add work.
//!
//! This is the minimal shape that hits the PRD FR-1..FR-3 requirements
//! and the SYSTEM_SPEC §6.1 description. Parallel subtasks (M6) will
//! build on this without rewriting it.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

use crate::task::{self, Task};
use crate::task_runner::{run_task, TaskDeps};

/// The scheduler handle. Clone to submit from multiple places.
#[derive(Clone)]
pub struct Scheduler {
    submit_tx: mpsc::Sender<Task>,
}

/// The running scheduler loop, obtained from `Scheduler::start`.
/// Drop it (or await it) to shut down.
pub struct SchedulerHandle {
    pub loop_handle: JoinHandle<()>,
    pub submit: Scheduler,
}

impl Scheduler {
    /// Start a scheduler loop on the current tokio runtime. Returns
    /// a handle whose `loop_handle` completes when the submit channel
    /// closes and all in-flight tasks finish.
    pub fn start(deps: TaskDeps, max_concurrent: u32, queue_capacity: usize) -> SchedulerHandle {
        let (submit_tx, submit_rx) = mpsc::channel::<Task>(queue_capacity.max(1));
        let submit = Scheduler { submit_tx };
        let loop_handle = tokio::spawn(scheduler_loop(deps, submit_rx, max_concurrent));
        SchedulerHandle { loop_handle, submit }
    }

    /// Enqueue a task. The task should already have been recorded as
    /// pending in the store via `task::record_pending` — this is a
    /// soft contract but making it explicit in the API would mean the
    /// scheduler has to know about the store.
    ///
    /// Returns an error if the scheduler has shut down.
    pub async fn submit(&self, task: Task) -> Result<()> {
        self.submit_tx
            .send(task)
            .await
            .context("scheduler has shut down")
    }

    /// Close the submit channel. The scheduler loop exits after the
    /// queue drains and all in-flight tasks finish.
    pub fn close(self) {
        // Dropping the tx triggers the channel close.
        drop(self.submit_tx);
    }
}

async fn scheduler_loop(
    deps: TaskDeps,
    mut submit_rx: mpsc::Receiver<Task>,
    max_concurrent: u32,
) {
    let sem = Arc::new(Semaphore::new(max_concurrent as usize));
    let mut in_flight: Vec<JoinHandle<()>> = Vec::new();

    debug!(max_concurrent, "scheduler loop started");

    while let Some(task) = submit_rx.recv().await {
        // Reap finished handles so the vector doesn't grow forever.
        in_flight.retain(|h| !h.is_finished());

        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "semaphore closed; dropping task");
                continue;
            }
        };

        let run_deps = deps.clone();
        let task_id_for_log = task.id.clone();
        let handle = tokio::spawn(async move {
            let _permit = permit;
            match run_task(run_deps.clone(), task.clone()).await {
                Ok(_) => {}
                Err(e) => {
                    warn!(task_id = %task.id, error = %e, "task failed");
                    // Ensure the task is marked failed if the runner
                    // didn't manage to (e.g. crashed before its own
                    // mark_failed call).
                    let _ = task::mark_failed(
                        &run_deps.store,
                        run_deps.session_id.as_str(),
                        &task.id,
                        &format!("{e:#}"),
                    );
                }
            }
            debug!(task_id = %task_id_for_log, "runner exited");
        });
        in_flight.push(handle);
    }

    debug!("submit channel closed; awaiting in-flight tasks");

    for h in in_flight {
        let _ = h.await;
    }

    debug!("scheduler loop exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{Adapter, AgentMessage, OwnerMessage};
    use crate::context::TaskKind;
    use crate::Config;
    use async_trait::async_trait;
    use serde_json::json;
    use sl_llm::mock::{MockLlmClient, ScriptStep, ScriptedResponse};
    use sl_llm::{LlmClient, ToolSchema};
    use sl_store::Store;
    use sl_tools::{Dispatcher, HostClass, Registry, Tool, ToolCtx as InnerToolCtx, ToolResult};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct MemAdapter {
        name: &'static str,
        sent: Mutex<Vec<AgentMessage>>,
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
        async fn receive(&self) -> Result<Option<OwnerMessage>> {
            Ok(None)
        }
    }

    struct Noop;
    #[async_trait]
    impl Tool for Noop {
        fn name(&self) -> &str {
            "noop"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("noop", "noop", json!({"type":"object"}))
        }
        fn host_class(&self) -> HostClass {
            HostClass::InProc
        }
        async fn invoke(&self, _c: &InnerToolCtx, _a: serde_json::Value) -> ToolResult {
            Ok("ok".into())
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
            "sl-sched-{}-{:x}-{:?}-{}",
            std::process::id(),
            nanos,
            std::thread::current().id(),
            n
        ));
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::create_dir_all(root.join("journal")).unwrap();
        std::fs::write(root.join("VERSION"), "0\n").unwrap();
        std::fs::write(root.join("prompts/CHARTER.md"), "c").unwrap();
        std::fs::write(root.join("prompts/CREED.md"), "c").unwrap();
        std::fs::write(root.join("prompts/doctrine.toml"), "[repo]\n").unwrap();
        std::fs::write(root.join("prompts/scratch.md"), "").unwrap();
        root
    }

    fn deps_with_mock(
        repo: &std::path::Path,
        store: &Store,
        script: Vec<ScriptStep>,
        adapters: Vec<Arc<dyn Adapter>>,
    ) -> TaskDeps {
        let mut cfg = Config::default();
        cfg.agent.repo_root = repo.to_path_buf();
        cfg.agent.data_dir = repo.join("data");
        cfg.budget.total_usd = 1_000_000.0;

        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", script));

        let mut reg = Registry::new();
        reg.register(Arc::new(Noop));
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
    async fn submit_and_run_one_task() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let adapter = Arc::new(MemAdapter {
            name: "cli",
            sent: Mutex::new(Vec::new()),
        });
        let script = vec![ScriptStep::Respond(ScriptedResponse::text("ok"))];
        let deps = deps_with_mock(
            &repo,
            &store,
            script,
            vec![adapter.clone() as Arc<dyn Adapter>],
        );

        let handle = Scheduler::start(deps, 2, 32);

        let task = Task::from_owner("hi", "cli");
        task::record_pending(&store, "s1", &task).unwrap();
        handle.submit.submit(task.clone()).await.unwrap();

        handle.submit.close();
        handle.loop_handle.await.unwrap();

        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "ok");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn respects_concurrency_limit() {
        // Submit 4 tasks with max_concurrent=2 and assert that at no
        // point were more than 2 running. We measure by having each
        // LLM call delay briefly and counting "in flight" via a
        // shared atomic.
        use std::sync::atomic::AtomicU32;

        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let adapter = Arc::new(MemAdapter {
            name: "cli",
            sent: Mutex::new(Vec::new()),
        });

        // Delay tool that blocks long enough for the scheduler to
        // reveal its concurrency.
        struct DelayTool {
            in_flight: Arc<AtomicU32>,
            max_seen: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Tool for DelayTool {
            fn name(&self) -> &str {
                "delay"
            }
            fn schema(&self) -> ToolSchema {
                ToolSchema::new("delay", "delay", json!({"type":"object"}))
            }
            fn host_class(&self) -> HostClass {
                HostClass::InProc
            }
            async fn invoke(&self, _c: &InnerToolCtx, _a: serde_json::Value) -> ToolResult {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok("done".into())
            }
        }

        let in_flight = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));

        // Each task: one tool_call round that delays, then content-only.
        // With 4 tasks and max_concurrent=2, at least one point should
        // have 2 in flight, never 3.
        let tool_call_resp = || {
            ScriptedResponse::tool_calls(
                None,
                vec![sl_llm::ToolCall {
                    id: "c1".into(),
                    name: "delay".into(),
                    arguments: "{}".into(),
                }],
            )
        };
        let done_resp = || ScriptedResponse::text("done");
        // Per-task: delay round + final round = 2 scripted responses.
        // 4 tasks × 2 = 8 responses total. But each task uses its own
        // MockLlmClient instance — we'd need 4 mocks. Simpler: use a
        // long script shared across all 4 (MockLlmClient has internal
        // counter but is shared). Actually no, MockLlmClient is shared
        // via Arc and the counter is shared; that works.
        let shared_script: Vec<ScriptStep> = (0..4)
            .flat_map(|_| {
                vec![
                    ScriptStep::Respond(tool_call_resp()),
                    ScriptStep::Respond(done_resp()),
                ]
            })
            .collect();

        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            shared_script,
        ));

        let mut cfg = Config::default();
        cfg.agent.repo_root = repo.clone();
        cfg.agent.data_dir = repo.join("data");
        cfg.budget.total_usd = 1_000_000.0;

        let mut reg = Registry::new();
        reg.register(Arc::new(DelayTool {
            in_flight: in_flight.clone(),
            max_seen: max_seen.clone(),
        }));
        let dispatcher = Arc::new(Dispatcher::new(reg));

        let deps = TaskDeps {
            config: Arc::new(cfg),
            store: store.clone(),
            session_id: Arc::new("s1".into()),
            llm,
            dispatcher,
            adapters: Arc::new(vec![adapter.clone() as Arc<dyn Adapter>]),
            context_soft_cap_tokens: 0,
        };

        let handle = Scheduler::start(deps, 2, 32);

        for i in 0..4 {
            let t = Task::from_owner(format!("task {i}"), "cli");
            task::record_pending(&store, "s1", &t).unwrap();
            handle.submit.submit(t).await.unwrap();
        }

        handle.submit.close();
        handle.loop_handle.await.unwrap();

        let max = max_seen.load(Ordering::SeqCst);
        assert!(max >= 1, "at least one task must have run");
        assert!(
            max <= 2,
            "max concurrent must not exceed 2, observed {max}"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn task_kind_is_exported() {
        // Sanity: make sure the re-exports work the way callers expect.
        let _ = TaskKind::User;
    }
}
