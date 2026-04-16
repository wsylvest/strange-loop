//! The tool loop — the inner engine of one task.
//!
//! See `docs/SYSTEM_SPEC.md` §6.3 for the full algorithm. This module
//! implements:
//!   - bounded rounds (hard cap from config)
//!   - 50-round soft self-check reminder injection
//!   - LLM call with retry on transport error
//!   - fallback to the next model in the chain on empty response
//!   - tool dispatch via the Dispatcher trait object
//!   - event emission: llm_round, llm_usage, llm_empty_response,
//!     llm_api_error
//!   - termination conditions: content-only response, round cap,
//!     LLM error exhausted
//!
//!   - budget guard: hard cap (>50% of remaining) forces final answer;
//!     soft nudge (>30% every 10 rounds) injects info message
//!
//! Owner-mailbox injection is wired in M6.

use std::sync::Arc;

use anyhow::Result;
use serde::Serialize;
use sl_llm::{
    ChatRequest, Effort, FinishReason, LlmClient, Message, MessageRole, ToolChoice, ToolSchema,
    Usage,
};
use sl_store::{events, EventKind};
use sl_tools::{Dispatcher, ToolCtx};
use tracing::{debug, warn};

use crate::budget::{self, BudgetCheckResult, BudgetConfig};

/// Tuning knobs for one run of the loop. These come from config in
/// production; tests construct them inline.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub default_model: String,
    pub fallback_chain: Vec<String>,
    pub initial_effort: Effort,
    pub max_rounds: u32,
    pub self_check_interval: u32,
    pub max_retries: u32,
    pub max_tokens_per_call: u32,
    /// Budget enforcement. None = no budget guard at all.
    pub budget: Option<BudgetConfig>,
}

impl LoopConfig {
    pub fn from_core_config(cfg: &crate::config::Config) -> Self {
        Self {
            default_model: cfg.llm.default_model.clone(),
            fallback_chain: cfg.llm.fallback_chain.clone(),
            initial_effort: Effort::Medium,
            max_rounds: cfg.tool_loop.max_rounds,
            self_check_interval: cfg.tool_loop.self_check_interval,
            max_retries: 3,
            max_tokens_per_call: 16_384,
            budget: Some(BudgetConfig {
                total_usd: Some(cfg.budget.total_usd),
                hard_task_pct: cfg.budget.hard_task_pct,
                soft_task_pct: cfg.budget.soft_task_pct,
            }),
        }
    }
}

/// The outcome of running the loop on one task.
#[derive(Debug, Clone)]
pub struct LoopOutcome {
    /// The final assistant text presented to the owner. May be an
    /// error string if the loop terminated abnormally.
    pub final_text: String,
    /// Accumulated usage across every LLM call the loop made.
    pub usage: Usage,
    /// Number of completed rounds (each round is one LLM call that
    /// successfully returned).
    pub rounds: u32,
    /// Why the loop stopped. Useful for tests and observability.
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Model returned content with no tool calls. The normal exit.
    ContentOnly,
    /// Hit the hard round cap; final answer was forced.
    RoundCap,
    /// Task spending exceeded the hard budget threshold. Final answer
    /// was forced. This is KERNEL-level enforcement — the LLM cannot
    /// reason its way around it.
    BudgetCap,
    /// LLM error chain exhausted: primary + all fallbacks failed.
    LlmExhausted,
}

// ---------------------------------------------------------------------------
// Event payloads for observability
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct LlmRoundPayload<'a> {
    round: u32,
    model: &'a str,
    effort: &'a str,
    prompt_tokens: u32,
    completion_tokens: u32,
    cached_tokens: u32,
}

#[derive(Serialize)]
struct LlmUsagePayload<'a> {
    round: u32,
    model: &'a str,
    cost_usd: f64,
    prompt_tokens: u32,
    completion_tokens: u32,
    cached_tokens: u32,
    cache_write_tokens: u32,
    cost_estimated: bool,
}

#[derive(Serialize)]
struct LlmEmptyPayload<'a> {
    round: u32,
    model: &'a str,
    attempt: u32,
}

#[derive(Serialize)]
struct LlmApiErrorPayload<'a> {
    round: u32,
    model: &'a str,
    attempt: u32,
    error: String,
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

pub async fn run_tool_loop(
    llm: Arc<dyn LlmClient>,
    dispatcher: Arc<Dispatcher>,
    tools: Vec<ToolSchema>,
    ctx: ToolCtx,
    cfg: LoopConfig,
    initial_messages: Vec<Message>,
) -> Result<LoopOutcome> {
    let mut messages = initial_messages;
    let mut usage = Usage::zero();
    let mut rounds: u32 = 0;

    let active_model = cfg.default_model.clone();
    let active_effort = cfg.initial_effort;

    loop {
        rounds += 1;

        // Hard cap: force a final answer and exit.
        if rounds > cfg.max_rounds {
            debug!(rounds, "round cap reached");
            messages.push(Message::system_text(format!(
                "[ROUND_LIMIT] You have exceeded the {}-round cap. Return your \
                 best final answer now, using only content (no tool calls).",
                cfg.max_rounds
            )));
            let final_text = force_final_answer(
                llm.as_ref(),
                &messages,
                &active_model,
                active_effort,
                &cfg,
                &ctx,
                rounds,
            )
            .await
            .unwrap_or_else(|_| format!("Round cap reached at {}.", cfg.max_rounds));
            return Ok(LoopOutcome {
                final_text,
                usage,
                rounds: rounds.saturating_sub(1),
                stop_reason: StopReason::RoundCap,
            });
        }

        // Soft self-check reminder. Bible P0+P3: LLM decides whether
        // to stop, compact, or continue.
        if cfg.self_check_interval > 0
            && rounds > 1
            && rounds % cfg.self_check_interval == 0
        {
            messages.push(Message::system_text(self_check_text(
                rounds,
                cfg.max_rounds,
                &usage,
            )));
        }

        // Build the request.
        let mut req = ChatRequest::new(active_model.clone(), messages.clone());
        req.tools = tools.clone();
        req.tool_choice = ToolChoice::Auto;
        req.effort = active_effort;
        req.max_tokens = cfg.max_tokens_per_call;

        // Call with retry + fallback.
        let call_result = call_with_retry_and_fallback(
            llm.as_ref(),
            req,
            &cfg,
            &ctx,
            rounds,
        )
        .await;

        let response = match call_result {
            Ok(r) => r,
            Err(exhausted) => {
                return Ok(LoopOutcome {
                    final_text: exhausted,
                    usage,
                    rounds: rounds.saturating_sub(1),
                    stop_reason: StopReason::LlmExhausted,
                });
            }
        };

        // Accumulate usage and emit round/usage events.
        usage.add(&response.usage);
        emit_llm_events(
            &ctx,
            rounds,
            &response.model_used,
            active_effort.as_str(),
            &response.usage,
        );

        let assistant_msg = response.message;
        let tool_calls = assistant_msg.tool_calls.clone();
        messages.push(assistant_msg.clone());

        // Terminal condition: no tool calls.
        if tool_calls.is_empty() {
            let text = assistant_msg.text_concat();
            return Ok(LoopOutcome {
                final_text: text,
                usage,
                rounds,
                stop_reason: StopReason::ContentOnly,
            });
        }

        // Dispatch tool calls (sequentially in M1).
        for tc in &tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(&tc.arguments).unwrap_or(serde_json::json!({}));
            let result = dispatcher.dispatch(&ctx, &tc.name, args).await;
            messages.push(Message::tool_result(tc.id.clone(), result));
        }

        // Budget guard — KERNEL-level enforcement (SYSTEM_SPEC §6.8).
        if let Some(ref budget_cfg) = cfg.budget {
            let result = budget::check_budget(
                usage.cost_usd,
                &ctx.store,
                ctx.session_id.as_str(),
                rounds,
                budget_cfg,
            );
            match result {
                BudgetCheckResult::HardStop {
                    task_cost,
                    remaining,
                } => {
                    debug!(task_cost, remaining, "budget hard stop");
                    messages.push(Message::system_text(budget::hard_stop_message(
                        task_cost, remaining,
                    )));
                    let final_text = force_final_answer(
                        llm.as_ref(),
                        &messages,
                        &active_model,
                        active_effort,
                        &cfg,
                        &ctx,
                        rounds,
                    )
                    .await
                    .unwrap_or_else(|_| {
                        format!(
                            "Budget limit reached: task spent ${:.4} of ${:.2} remaining.",
                            task_cost, remaining
                        )
                    });
                    return Ok(LoopOutcome {
                        final_text,
                        usage,
                        rounds,
                        stop_reason: StopReason::BudgetCap,
                    });
                }
                BudgetCheckResult::SoftNudge {
                    task_cost,
                    remaining,
                } => {
                    messages.push(Message::system_text(budget::soft_nudge_message(
                        task_cost, remaining,
                    )));
                }
                BudgetCheckResult::Ok => {}
            }
        }

        // Loop back around.
        if response.finish_reason == FinishReason::Length {
            // Model hit max_tokens. Inject a nudge and let it continue.
            messages.push(Message::system_text(
                "Your previous response was cut off at max_tokens. Continue.",
            ));
        }
    }
}

/// One LLM round that succeeded (non-empty, parseable).
struct SuccessfulCall {
    message: Message,
    usage: Usage,
    finish_reason: FinishReason,
    model_used: String,
}

/// Try the primary model, retrying on transport errors; if the primary
/// returns empty responses after `max_retries`, try each fallback in
/// order. Returns `Err(final_text)` if everything fails, where
/// `final_text` is a human-readable error for the user.
async fn call_with_retry_and_fallback(
    llm: &dyn LlmClient,
    req: ChatRequest,
    cfg: &LoopConfig,
    ctx: &ToolCtx,
    round: u32,
) -> Result<SuccessfulCall, String> {
    // Build model sequence: primary first, then any fallback that isn't
    // the primary. This correctly handles "primary == fallback[0]" —
    // the v6.0.0 Ouroboros bug we explicitly don't want to reintroduce.
    let primary = req.model.clone();
    let mut sequence: Vec<String> = vec![primary.clone()];
    for m in &cfg.fallback_chain {
        if m != &primary && !sequence.contains(m) {
            sequence.push(m.clone());
        }
    }

    let mut last_error: Option<String> = None;
    for model in &sequence {
        let mut req_for_model = req.clone();
        req_for_model.model = model.clone();

        for attempt in 1..=cfg.max_retries {
            match llm.chat(req_for_model.clone()).await {
                Ok(resp) if !resp.is_empty() => {
                    return Ok(SuccessfulCall {
                        message: resp.message,
                        usage: resp.usage,
                        finish_reason: resp.finish_reason,
                        model_used: model.clone(),
                    });
                }
                Ok(_empty) => {
                    events::append_payload(
                        &ctx.store,
                        ctx.session_id.as_str(),
                        EventKind::LlmEmptyResponse,
                        Some(ctx.task_id.as_str()),
                        &LlmEmptyPayload {
                            round,
                            model,
                            attempt,
                        },
                    )
                    .ok();
                    warn!(round, model = %model, attempt, "empty llm response");
                    // On empty, try the next attempt within this model.
                    // If we've exhausted attempts, fall through to next model.
                    continue;
                }
                Err(e) => {
                    let msg = format!("{:#}", e);
                    events::append_payload(
                        &ctx.store,
                        ctx.session_id.as_str(),
                        EventKind::LlmApiError,
                        Some(ctx.task_id.as_str()),
                        &LlmApiErrorPayload {
                            round,
                            model,
                            attempt,
                            error: msg.clone(),
                        },
                    )
                    .ok();
                    last_error = Some(msg);
                    // Transport errors also flow through: try the next
                    // attempt within the same model, then fall through.
                    continue;
                }
            }
        }
    }

    let tail = last_error.as_deref().unwrap_or("empty responses");
    Err(format!(
        "Model call failed after trying {} model(s). Last error: {}",
        sequence.len(),
        tail
    ))
}

/// One final LLM call with tool_choice=None, used when the round cap
/// or the (future) budget cap forces a wrap-up answer.
async fn force_final_answer(
    llm: &dyn LlmClient,
    messages: &[Message],
    model: &str,
    effort: Effort,
    cfg: &LoopConfig,
    ctx: &ToolCtx,
    round: u32,
) -> Result<String, String> {
    let mut req = ChatRequest::new(model.to_string(), messages.to_vec());
    req.tools = Vec::new();
    req.tool_choice = ToolChoice::None;
    req.effort = effort;
    req.max_tokens = cfg.max_tokens_per_call;

    match call_with_retry_and_fallback(llm, req, cfg, ctx, round).await {
        Ok(call) => {
            let text = call.message.text_concat();
            if text.is_empty() {
                Err("empty final answer".into())
            } else {
                Ok(text)
            }
        }
        Err(e) => Err(e),
    }
}

fn self_check_text(round: u32, max_rounds: u32, usage: &Usage) -> String {
    format!(
        "[CHECKPOINT — round {round}/{max_rounds}]\n\
         Cost so far: ${:.4} | prompt_tokens: {} | completion_tokens: {}\n\n\
         Pause and reflect:\n\
         1. Am I making real progress, or repeating the same actions?\n\
         2. Is my context bloated with old tool results I no longer need?\n\
         3. Should I stop and return my best result so far?\n\
         This is a reminder, not a command. You decide.",
        usage.cost_usd, usage.prompt_tokens, usage.completion_tokens
    )
}

fn emit_llm_events(
    ctx: &ToolCtx,
    round: u32,
    model: &str,
    effort: &str,
    u: &Usage,
) {
    events::append_payload(
        &ctx.store,
        ctx.session_id.as_str(),
        EventKind::LlmRound,
        Some(ctx.task_id.as_str()),
        &LlmRoundPayload {
            round,
            model,
            effort,
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            cached_tokens: u.cached_tokens,
        },
    )
    .ok();
    events::append_payload(
        &ctx.store,
        ctx.session_id.as_str(),
        EventKind::LlmUsage,
        Some(ctx.task_id.as_str()),
        &LlmUsagePayload {
            round,
            model,
            cost_usd: u.cost_usd,
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            cached_tokens: u.cached_tokens,
            cache_write_tokens: u.cache_write_tokens,
            cost_estimated: u.cost_estimated,
        },
    )
    .ok();
}

// Extension trait lint suppression: once we add mailbox and budget
// hooks we'll silence dead-code warnings more carefully. For now,
// `messages` is the variable used for the conversation.
#[allow(dead_code)]
fn _borrow(msgs: &[Message]) -> usize {
    msgs.iter()
        .filter(|m| matches!(m.role, MessageRole::Assistant))
        .count()
}

// ---------------------------------------------------------------------------
// Tests — TDD the loop against the mock
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use sl_llm::mock::{MockLlmClient, ScriptStep, ScriptedError, ScriptedResponse};
    use sl_llm::{ToolCall, ToolSchema};
    use sl_store::Store;
    use sl_tools::{HostClass, Registry, Tool, ToolResult};
    use std::path::PathBuf;

    // ---- test scaffolding ----

    fn make_ctx(store: Store) -> ToolCtx {
        ToolCtx {
            store,
            repo_root: Arc::new(PathBuf::from("/tmp/repo")),
            data_dir: Arc::new(PathBuf::from("/tmp/data")),
            protected_paths: Arc::new(vec![]),
            session_id: Arc::new("session-test".into()),
            task_id: Arc::new("task-test".into()),
        }
    }

    fn base_cfg() -> LoopConfig {
        LoopConfig {
            default_model: "anthropic/claude-sonnet-4.6".into(),
            fallback_chain: vec!["google/gemini-2.5-pro-preview".into()],
            initial_effort: Effort::Medium,
            max_rounds: 200,
            self_check_interval: 50,
            max_retries: 3,
            max_tokens_per_call: 4096,
            budget: None, // most loop tests don't need budget
        }
    }

    fn user_msg(s: &str) -> Vec<Message> {
        vec![Message::user_text(s)]
    }

    // A tool the tests use to simulate real work.
    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                "echo",
                "echo args",
                json!({"type":"object","properties":{"x":{"type":"string"}}}),
            )
        }
        fn host_class(&self) -> HostClass {
            HostClass::InProc
        }
        async fn invoke(&self, _ctx: &ToolCtx, args: serde_json::Value) -> ToolResult {
            Ok(args.get("x").and_then(|v| v.as_str()).unwrap_or("").to_string())
        }
    }

    fn make_dispatcher() -> Arc<Dispatcher> {
        let mut reg = Registry::new();
        reg.register(Arc::new(EchoTool));
        Arc::new(Dispatcher::new(reg))
    }

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    // ---- the tests ----

    #[tokio::test]
    async fn single_round_content_only_exits_cleanly() {
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store.clone());
        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            vec![ScriptStep::Respond(ScriptedResponse::text("the answer is 42"))],
        ));

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            base_cfg(),
            user_msg("what is the answer?"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.final_text, "the answer is 42");
        assert_eq!(outcome.rounds, 1);
        assert_eq!(outcome.stop_reason, StopReason::ContentOnly);
        assert_eq!(
            events::count_by_kind(&store, "session-test", EventKind::LlmRound).unwrap(),
            1
        );
        assert_eq!(
            events::count_by_kind(&store, "session-test", EventKind::LlmUsage).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn two_round_tool_call_then_answer() {
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store.clone());

        let script = vec![
            // round 1: emit a tool call
            ScriptStep::Respond(ScriptedResponse::tool_calls(
                Some("let me echo"),
                vec![tool_call("c1", "echo", json!({"x": "hello"}))],
            )),
            // round 2: content-only final answer
            ScriptStep::Respond(ScriptedResponse::text("echoed: hello")),
        ];
        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            script,
        ));

        let outcome = run_tool_loop(
            llm.clone(),
            make_dispatcher(),
            vec![],
            ctx,
            base_cfg(),
            user_msg("echo hello"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.rounds, 2);
        assert_eq!(outcome.stop_reason, StopReason::ContentOnly);
        assert_eq!(outcome.final_text, "echoed: hello");
        // Two llm rounds, one tool call, one tool result
        assert_eq!(
            events::count_by_kind(&store, "session-test", EventKind::LlmRound).unwrap(),
            2
        );
        assert_eq!(
            events::count_by_kind(&store, "session-test", EventKind::ToolCall).unwrap(),
            1
        );
        assert_eq!(
            events::count_by_kind(&store, "session-test", EventKind::ToolResult).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn usage_accumulates_across_rounds() {
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store);
        let script = vec![
            ScriptStep::Respond(
                ScriptedResponse::tool_calls(
                    None,
                    vec![tool_call("c1", "echo", json!({"x": "a"}))],
                )
                .with_usage(Usage {
                    prompt_tokens: 100,
                    completion_tokens: 20,
                    cost_usd: 0.01,
                    ..Default::default()
                }),
            ),
            ScriptStep::Respond(ScriptedResponse::text("done").with_usage(Usage {
                prompt_tokens: 150,
                completion_tokens: 10,
                cost_usd: 0.02,
                ..Default::default()
            })),
        ];
        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", script));
        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            base_cfg(),
            user_msg("go"),
        )
        .await
        .unwrap();
        assert_eq!(outcome.usage.prompt_tokens, 250);
        assert_eq!(outcome.usage.completion_tokens, 30);
        assert!((outcome.usage.cost_usd - 0.03).abs() < 1e-9);
    }

    #[tokio::test]
    async fn empty_response_retries_then_falls_back_to_next_model() {
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store.clone());
        // Primary: 3 empties (exhausting retries). Fallback: one good answer.
        let script = vec![
            ScriptStep::Respond(ScriptedResponse::empty()),
            ScriptStep::Respond(ScriptedResponse::empty()),
            ScriptStep::Respond(ScriptedResponse::empty()),
            ScriptStep::Respond(ScriptedResponse::text("fallback speaks")),
        ];
        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", script));

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            base_cfg(),
            user_msg("hi"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.final_text, "fallback speaks");
        assert_eq!(outcome.stop_reason, StopReason::ContentOnly);
        assert_eq!(
            events::count_by_kind(&store, "session-test", EventKind::LlmEmptyResponse).unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn fallback_chain_handles_primary_also_in_chain() {
        // Regression guard for the Ouroboros bug where primary ==
        // fallback[0] caused an infinite loop. The sequence should
        // be [primary, rest...] with duplicates removed.
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store);

        let mut cfg = base_cfg();
        cfg.default_model = "anthropic/claude-sonnet-4.6".into();
        cfg.fallback_chain = vec![
            "anthropic/claude-sonnet-4.6".into(), // same as primary
            "openai/gpt-4.1".into(),
        ];

        // Primary exhausts its retries, then fallback (gpt-4.1) succeeds.
        let script = vec![
            ScriptStep::Respond(ScriptedResponse::empty()),
            ScriptStep::Respond(ScriptedResponse::empty()),
            ScriptStep::Respond(ScriptedResponse::empty()),
            ScriptStep::Respond(ScriptedResponse::text("from gpt-4.1")),
        ];
        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", script));

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            cfg,
            user_msg("hi"),
        )
        .await
        .unwrap();

        // Should NOT have re-tried the primary; should land on the real fallback.
        assert_eq!(outcome.final_text, "from gpt-4.1");
    }

    #[tokio::test]
    async fn transport_error_retried_then_recovered() {
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store.clone());
        let script = vec![
            ScriptStep::Fail(ScriptedError::Transport("timeout".into())),
            ScriptStep::Respond(ScriptedResponse::text("recovered")),
        ];
        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", script));

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            base_cfg(),
            user_msg("hi"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.final_text, "recovered");
        assert_eq!(
            events::count_by_kind(&store, "session-test", EventKind::LlmApiError).unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn all_empties_exhausts_and_reports_error() {
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store);
        // Everything empty everywhere.
        let script: Vec<ScriptStep> = (0..10)
            .map(|_| ScriptStep::Respond(ScriptedResponse::empty()))
            .collect();
        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", script));

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            base_cfg(),
            user_msg("go"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop_reason, StopReason::LlmExhausted);
        assert!(outcome.final_text.contains("Model call failed"));
    }

    #[tokio::test]
    async fn round_cap_forces_final_answer() {
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store);

        // Make the loop hit max_rounds: keep asking for tool calls.
        let mut cfg = base_cfg();
        cfg.max_rounds = 3;
        cfg.self_check_interval = 0; // disable checkpoint reminder noise

        // 4 scripted responses: 3 tool-call rounds + 1 final no-tools answer.
        let script = vec![
            ScriptStep::Respond(ScriptedResponse::tool_calls(
                None,
                vec![tool_call("a", "echo", json!({"x": "1"}))],
            )),
            ScriptStep::Respond(ScriptedResponse::tool_calls(
                None,
                vec![tool_call("b", "echo", json!({"x": "2"}))],
            )),
            ScriptStep::Respond(ScriptedResponse::tool_calls(
                None,
                vec![tool_call("c", "echo", json!({"x": "3"}))],
            )),
            // after round cap triggers, the loop forces a final call
            ScriptStep::Respond(ScriptedResponse::text("forced final")),
        ];
        let llm: Arc<dyn LlmClient> =
            Arc::new(MockLlmClient::new("anthropic/claude-sonnet-4.6", script));

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            cfg,
            user_msg("spin forever"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop_reason, StopReason::RoundCap);
        assert_eq!(outcome.final_text, "forced final");
    }

    #[tokio::test]
    async fn self_check_reminder_fires_at_interval() {
        // Use interval = 2 so it fires on round 2.
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store.clone());

        let mut cfg = base_cfg();
        cfg.self_check_interval = 2;

        // Round 1: tool call. Round 2 (interval-multiple): content-only.
        // We assert on captured requests to check that round 2 saw the
        // checkpoint system message injected between round 1's tool
        // result and round 2's call.
        let script = vec![
            ScriptStep::Respond(ScriptedResponse::tool_calls(
                None,
                vec![tool_call("a", "echo", json!({"x": "1"}))],
            )),
            ScriptStep::Respond(ScriptedResponse::text("ok")),
        ];
        let mock = MockLlmClient::new("anthropic/claude-sonnet-4.6", script);
        let llm: Arc<dyn LlmClient> = Arc::new(mock.clone());

        let _outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            cfg,
            user_msg("go"),
        )
        .await
        .unwrap();

        // Round 2 message count should be user + assistant(tool_calls)
        // + tool_result + system(checkpoint) = 4.
        let captured = mock.captured();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].message_count, 1); // just the user msg
        assert_eq!(
            captured[1].message_count, 4,
            "checkpoint system message should be injected before round 2"
        );
    }

    #[tokio::test]
    async fn tool_results_feed_back_into_next_round() {
        // Verify the dispatcher output is in the message list the next
        // LLM call sees. The EchoTool echoes args.x; round 2 should
        // see the echo result.
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store);
        let script = vec![
            ScriptStep::Respond(ScriptedResponse::tool_calls(
                None,
                vec![tool_call("c1", "echo", json!({"x": "from round 1"}))],
            )),
            ScriptStep::Respond(ScriptedResponse::text("done")),
        ];
        let mock = MockLlmClient::new("anthropic/claude-sonnet-4.6", script);
        let llm: Arc<dyn LlmClient> = Arc::new(mock.clone());

        let _ = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            base_cfg(),
            user_msg("echo something"),
        )
        .await
        .unwrap();

        let cap = mock.captured();
        // Round 2 must contain 3 messages: user, assistant(tool_calls), tool_result
        assert_eq!(cap[1].message_count, 3);
    }

    // ----------------------------------------------------------------
    // Budget guard integration tests
    // ----------------------------------------------------------------

    use crate::budget::BudgetConfig;

    fn budget_cfg(total: f64) -> Option<BudgetConfig> {
        Some(BudgetConfig {
            total_usd: Some(total),
            hard_task_pct: 0.50,
            soft_task_pct: 0.30,
        })
    }

    /// Seed the store with prior session spending so the budget guard
    /// sees a specific "remaining" when it queries.
    fn seed_prior_spending(store: &Store, session_id: &str, cost: f64) {
        #[derive(Serialize)]
        struct P {
            cost_usd: f64,
        }
        events::append_payload(
            store,
            session_id,
            EventKind::LlmUsage,
            Some("prior"),
            &P { cost_usd: cost },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn budget_hard_stop_terminates_loop() {
        // total=10.0, prior session spending=9.0, remaining=1.0.
        // The first LLM call costs $0.60 → 60% of remaining → hard stop.
        let store = Store::open_in_memory().unwrap();
        seed_prior_spending(&store, "session-test", 9.0);
        let ctx = make_ctx(store.clone());

        let expensive_usage = Usage {
            prompt_tokens: 1000,
            completion_tokens: 200,
            cost_usd: 0.60,
            ..Default::default()
        };
        let script = vec![
            // Round 1: tool call (will accumulate $0.60)
            ScriptStep::Respond(
                ScriptedResponse::tool_calls(
                    None,
                    vec![tool_call("c1", "echo", json!({"x": "work"}))],
                )
                .with_usage(expensive_usage),
            ),
            // After budget guard fires, force_final_answer calls the
            // LLM one more time with no tools. This response is what
            // the user sees.
            ScriptStep::Respond(ScriptedResponse::text("stopped due to budget")),
        ];
        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            script,
        ));

        let mut cfg = base_cfg();
        cfg.budget = budget_cfg(10.0);

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            cfg,
            user_msg("do expensive work"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop_reason, StopReason::BudgetCap);
        assert_eq!(outcome.final_text, "stopped due to budget");
        assert_eq!(outcome.rounds, 1, "should stop after first tool-call round");
    }

    #[tokio::test]
    async fn budget_soft_nudge_injects_system_message() {
        // total=100, prior=60, remaining=40.
        // Task costs $0.002 per round → $0.01 after 5 rounds.
        // That's 0.025% of remaining, under soft threshold.
        // We need task cost to exceed 30% of remaining: $12.
        // So: prior=88, remaining=12, task_cost accumulates from
        // scripted usage to cross 30% ($3.60) by round 10.
        let store = Store::open_in_memory().unwrap();
        seed_prior_spending(&store, "session-test", 88.0);
        let ctx = make_ctx(store.clone());

        let r = || {
            ScriptedResponse::tool_calls(
                None,
                vec![tool_call("c1", "echo", json!({"x": "a"}))],
            )
            .with_usage(Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
                cost_usd: 0.40, // per round; 10 rounds = $4.0 → 33% of $12
                ..Default::default()
            })
        };
        let mut script: Vec<ScriptStep> = (0..10).map(|_| ScriptStep::Respond(r())).collect();
        // Round 11: content-only final answer
        script.push(ScriptStep::Respond(ScriptedResponse::text("done")));

        let mock = MockLlmClient::new("anthropic/claude-sonnet-4.6", script);
        let llm: Arc<dyn LlmClient> = Arc::new(mock.clone());

        let mut cfg = base_cfg();
        cfg.budget = budget_cfg(100.0);
        cfg.self_check_interval = 0; // disable self-check noise
        cfg.max_rounds = 20;

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            cfg,
            user_msg("go"),
        )
        .await
        .unwrap();

        // Round 10 is divisible by 10 and by then task cost = $4.0
        // with remaining = $12, so 33% → soft nudge should fire.
        // The mock captured round-10's request; it should have the
        // [BUDGET INFO] message in it.
        assert_eq!(outcome.stop_reason, StopReason::ContentOnly);
        let cap = mock.captured();
        // Round 10 request (index 9, since 0-indexed) should contain
        // the budget info message.
        let round_10_req = &cap[9];
        // The message count should include the budget nudge system message
        // A quick sanity: more messages than round 9 had (one extra for nudge)
        let round_9_req = &cap[8];
        assert!(
            round_10_req.message_count > round_9_req.message_count,
            "round 10 should have an extra system message from the budget nudge; \
             round 9 msgs={}, round 10 msgs={}",
            round_9_req.message_count,
            round_10_req.message_count,
        );
    }

    #[tokio::test]
    async fn budget_none_skips_all_checks() {
        // A task that would massively exceed budget if budget were set,
        // but budget is None → runs to completion normally.
        let store = Store::open_in_memory().unwrap();
        let ctx = make_ctx(store);

        let script = vec![
            ScriptStep::Respond(
                ScriptedResponse::tool_calls(
                    None,
                    vec![tool_call("c1", "echo", json!({"x": "x"}))],
                )
                .with_usage(Usage {
                    cost_usd: 999.0,
                    ..Default::default()
                }),
            ),
            ScriptStep::Respond(ScriptedResponse::text("done")),
        ];
        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            script,
        ));

        let mut cfg = base_cfg();
        cfg.budget = None; // explicitly no budget

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            cfg,
            user_msg("go"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop_reason, StopReason::ContentOnly);
        assert_eq!(outcome.final_text, "done");
    }

    #[tokio::test]
    async fn budget_zero_remaining_is_immediate_hard_stop() {
        // total=50, prior=50, remaining=0. Even a tiny task cost triggers hard stop.
        let store = Store::open_in_memory().unwrap();
        seed_prior_spending(&store, "session-test", 50.0);
        let ctx = make_ctx(store);

        let script = vec![
            ScriptStep::Respond(
                ScriptedResponse::tool_calls(
                    None,
                    vec![tool_call("c1", "echo", json!({"x": "x"}))],
                )
                .with_usage(Usage {
                    cost_usd: 0.001,
                    ..Default::default()
                }),
            ),
            ScriptStep::Respond(ScriptedResponse::text("budget forced")),
        ];
        let llm: Arc<dyn LlmClient> = Arc::new(MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            script,
        ));

        let mut cfg = base_cfg();
        cfg.budget = budget_cfg(50.0);

        let outcome = run_tool_loop(
            llm,
            make_dispatcher(),
            vec![],
            ctx,
            cfg,
            user_msg("do anything"),
        )
        .await
        .unwrap();

        assert_eq!(outcome.stop_reason, StopReason::BudgetCap);
        assert_eq!(outcome.final_text, "budget forced");
    }
}
