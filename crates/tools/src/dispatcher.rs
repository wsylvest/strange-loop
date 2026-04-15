//! Tool dispatcher.
//!
//! Takes a `(name, args)` from the LLM and runs the corresponding
//! tool, enforcing per-tool timeouts and routing by isolation class.
//! In M1 only InProc tools are supported; Cell and Edge return
//! `ToolError::NotImplemented` and the loop sees them as tool errors,
//! which is the right behavior — the LLM gets the error in the result
//! message and decides what to do next.
//!
//! Detached invocations and parallel dispatch are wired in M2/M6
//! respectively. M1 dispatches sequentially.

use std::time::{Duration, Instant};

use serde::Serialize;
use sl_store::{events, EventKind};
use tracing::warn;

use crate::registry::Registry;
use crate::tool::{HostClass, ToolCtx, ToolError, ToolResult};

/// Hard cap on how much of a tool result is forwarded to the LLM.
/// Per SYSTEM_SPEC §6.3, default 15,000 characters.
pub const DEFAULT_RESULT_MAX_CHARS: usize = 15_000;

/// The dispatcher.
pub struct Dispatcher {
    registry: Registry,
    result_max_chars: usize,
}

#[derive(Debug, Serialize)]
struct ToolCallPayload<'a> {
    tool: &'a str,
    host_class: &'static str,
    args_preview: String,
}

#[derive(Debug, Serialize)]
struct ToolResultPayload<'a> {
    tool: &'a str,
    ok: bool,
    ms: u128,
    preview: String,
}

#[derive(Debug, Serialize)]
struct ToolErrorPayload<'a> {
    tool: &'a str,
    error: String,
}

#[derive(Debug, Serialize)]
struct ToolTimeoutPayload<'a> {
    tool: &'a str,
    limit_ms: u128,
}

impl Dispatcher {
    pub fn new(registry: Registry) -> Self {
        Self {
            registry,
            result_max_chars: DEFAULT_RESULT_MAX_CHARS,
        }
    }

    pub fn with_max_chars(mut self, n: usize) -> Self {
        self.result_max_chars = n;
        self
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Dispatch one tool call. Writes `tool_call`, `tool_result`,
    /// `tool_error`, or `tool_timeout` events via the store on `ctx`.
    /// Returns the (possibly truncated) result string the LLM will see.
    pub async fn dispatch(
        &self,
        ctx: &ToolCtx,
        name: &str,
        args: serde_json::Value,
    ) -> String {
        let tool = match self.registry.get(name) {
            Some(t) => t,
            None => {
                let err = format!("unknown tool: {}", name);
                self.emit_error(ctx, name, &err);
                return format!("ERROR: {}", err);
            }
        };

        // Reject non-InProc tools at the dispatcher boundary in M1.
        // Letting them fall through to the trait would still work (the
        // tool itself would return NotImplemented), but routing is the
        // dispatcher's job — we want one place to flip when M2 lands.
        match tool.host_class() {
            HostClass::InProc => {}
            other => {
                let err = format!(
                    "tool '{}' requires host class {:?}, not yet implemented",
                    name, other
                );
                self.emit_error(ctx, name, &err);
                return format!("ERROR: {}", err);
            }
        }

        let args_preview = preview_json(&args, 200);
        events::append_payload(
            &ctx.store,
            ctx.session_id.as_str(),
            EventKind::ToolCall,
            Some(ctx.task_id.as_str()),
            &ToolCallPayload {
                tool: name,
                host_class: tool.host_class().as_str(),
                args_preview,
            },
        )
        .ok();

        let started = Instant::now();
        let limit = tool.timeout();
        let outcome = run_with_timeout(limit, async move {
            tool.invoke(ctx, args).await
        })
        .await;
        let elapsed = started.elapsed();

        match outcome {
            DispatchOutcome::Ok(s) => {
                let truncated = truncate_result(&s, self.result_max_chars);
                let preview: String = truncated.chars().take(200).collect();
                events::append_payload(
                    &ctx.store,
                    ctx.session_id.as_str(),
                    EventKind::ToolResult,
                    Some(ctx.task_id.as_str()),
                    &ToolResultPayload {
                        tool: name,
                        ok: true,
                        ms: elapsed.as_millis(),
                        preview,
                    },
                )
                .ok();
                truncated
            }
            DispatchOutcome::ToolErr(err) => {
                let msg = err.to_string();
                self.emit_error(ctx, name, &msg);
                format!("ERROR: {}", msg)
            }
            DispatchOutcome::Timeout => {
                events::append_payload(
                    &ctx.store,
                    ctx.session_id.as_str(),
                    EventKind::ToolTimeout,
                    Some(ctx.task_id.as_str()),
                    &ToolTimeoutPayload {
                        tool: name,
                        limit_ms: limit.as_millis(),
                    },
                )
                .ok();
                warn!(tool = name, ms = elapsed.as_millis(), "tool timed out");
                format!("ERROR: tool '{}' timed out after {}s", name, limit.as_secs())
            }
        }
    }

    fn emit_error(&self, ctx: &ToolCtx, name: &str, msg: &str) {
        events::append_payload(
            &ctx.store,
            ctx.session_id.as_str(),
            EventKind::ToolError,
            Some(ctx.task_id.as_str()),
            &ToolErrorPayload {
                tool: name,
                error: msg.to_string(),
            },
        )
        .ok();
    }
}

enum DispatchOutcome {
    Ok(String),
    ToolErr(ToolError),
    Timeout,
}

async fn run_with_timeout<F>(limit: Duration, fut: F) -> DispatchOutcome
where
    F: std::future::Future<Output = ToolResult>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(s)) => DispatchOutcome::Ok(s),
        Ok(Err(e)) => DispatchOutcome::ToolErr(e),
        Err(_) => DispatchOutcome::Timeout,
    }
}

fn preview_json(value: &serde_json::Value, max: usize) -> String {
    let s = value.to_string();
    if s.chars().count() <= max {
        s
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

fn truncate_result(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    let original = s.chars().count();
    format!("{head}\n... [truncated: {} chars total]", original)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{HostClass, Tool, ToolCtx, ToolResult};
    use async_trait::async_trait;
    use serde_json::json;
    use sl_llm::ToolSchema;
    use sl_store::Store;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    fn ctx() -> ToolCtx {
        let store = Store::open_in_memory().unwrap();
        ToolCtx {
            store,
            repo_root: Arc::new(PathBuf::from("/tmp/repo")),
            data_dir: Arc::new(PathBuf::from("/tmp/data")),
            protected_paths: Arc::new(vec![]),
            session_id: Arc::new("s1".into()),
            task_id: Arc::new("t1".into()),
        }
    }

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("echo", "echo args", json!({"type":"object"}))
        }
        async fn invoke(&self, _ctx: &ToolCtx, args: serde_json::Value) -> ToolResult {
            Ok(args.to_string())
        }
    }

    struct FailTool;
    #[async_trait]
    impl Tool for FailTool {
        fn name(&self) -> &str {
            "fail"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("fail", "fail", json!({"type":"object"}))
        }
        async fn invoke(&self, _ctx: &ToolCtx, _args: serde_json::Value) -> ToolResult {
            Err(ToolError::Runtime("nope".into()))
        }
    }

    struct SlowTool;
    #[async_trait]
    impl Tool for SlowTool {
        fn name(&self) -> &str {
            "slow"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("slow", "slow", json!({"type":"object"}))
        }
        fn timeout(&self) -> Duration {
            Duration::from_millis(20)
        }
        async fn invoke(&self, _ctx: &ToolCtx, _args: serde_json::Value) -> ToolResult {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok("done".into())
        }
    }

    struct CellTool;
    #[async_trait]
    impl Tool for CellTool {
        fn name(&self) -> &str {
            "needs_cell"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new("needs_cell", "cell", json!({"type":"object"}))
        }
        fn host_class(&self) -> HostClass {
            HostClass::Cell
        }
        async fn invoke(&self, _ctx: &ToolCtx, _args: serde_json::Value) -> ToolResult {
            Ok("should not run".into())
        }
    }

    fn dispatcher_with(tools: Vec<Arc<dyn Tool>>) -> Dispatcher {
        let mut reg = Registry::new();
        for t in tools {
            reg.register(t);
        }
        Dispatcher::new(reg)
    }

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_error() {
        let d = dispatcher_with(vec![]);
        let ctx = ctx();
        let out = d.dispatch(&ctx, "missing", json!({})).await;
        assert!(out.starts_with("ERROR: unknown tool"));
        let n = events::count_by_kind(&ctx.store, "s1", EventKind::ToolError).unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn dispatch_ok_writes_call_and_result_events() {
        let d = dispatcher_with(vec![Arc::new(EchoTool)]);
        let ctx = ctx();
        let out = d.dispatch(&ctx, "echo", json!({"x": 1})).await;
        assert!(out.contains("\"x\":1"));
        assert_eq!(
            events::count_by_kind(&ctx.store, "s1", EventKind::ToolCall).unwrap(),
            1
        );
        assert_eq!(
            events::count_by_kind(&ctx.store, "s1", EventKind::ToolResult).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn dispatch_tool_error_writes_error_event() {
        let d = dispatcher_with(vec![Arc::new(FailTool)]);
        let ctx = ctx();
        let out = d.dispatch(&ctx, "fail", json!({})).await;
        assert!(out.starts_with("ERROR: tool runtime error: nope"));
        assert_eq!(
            events::count_by_kind(&ctx.store, "s1", EventKind::ToolError).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn dispatch_timeout_writes_timeout_event() {
        let d = dispatcher_with(vec![Arc::new(SlowTool)]);
        let ctx = ctx();
        let out = d.dispatch(&ctx, "slow", json!({})).await;
        assert!(out.contains("timed out"), "got {}", out);
        assert_eq!(
            events::count_by_kind(&ctx.store, "s1", EventKind::ToolTimeout).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn cell_tools_are_rejected_in_m1() {
        let d = dispatcher_with(vec![Arc::new(CellTool)]);
        let ctx = ctx();
        let out = d.dispatch(&ctx, "needs_cell", json!({})).await;
        assert!(out.contains("not yet implemented"));
        assert_eq!(
            events::count_by_kind(&ctx.store, "s1", EventKind::ToolError).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn truncation_kicks_in_above_max() {
        let d = dispatcher_with(vec![Arc::new(EchoTool)]).with_max_chars(50);
        let ctx = ctx();
        let big = "x".repeat(500);
        let args = json!({ "blob": big });
        let out = d.dispatch(&ctx, "echo", args).await;
        assert!(out.contains("[truncated:"));
    }
}
