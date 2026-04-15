//! Mock LLM client.
//!
//! The mock is at least as load-bearing as the real client. Almost every
//! integration test in the workspace depends on it: the tool loop, the
//! task runner, the budget ledger, the CLI adapter, the self-modification
//! flow, the background consciousness loop. If the mock is wrong or
//! unergonomic, every test pays the price.
//!
//! Design rules:
//!   1. Deterministic. Same script in, same calls out.
//!   2. Scripted, not generative. The test author writes the exact
//!      sequence of responses; the mock plays them back in order.
//!   3. Records every request it receives so tests can assert against
//!      both the input and the output.
//!   4. Fails loudly when the script runs out — silent fall-through
//!      to a default response is how mocks lie to you.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::types::{
    ChatRequest, ChatResponse, FinishReason, Message, ToolCall, Usage,
};
use crate::LlmClient;

/// One scripted response.
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    pub message: Message,
    pub usage: Usage,
    pub finish_reason: FinishReason,
}

impl ScriptedResponse {
    /// Build a content-only response (no tool calls). The `Stop` reason
    /// is appropriate; the loop will treat this as a final answer.
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            message: Message::assistant_text(s),
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 20,
                cost_usd: 0.001,
                cost_estimated: false,
                ..Default::default()
            },
            finish_reason: FinishReason::Stop,
        }
    }

    /// Build a tool-call response with optional preamble content.
    pub fn tool_calls(content: Option<&str>, calls: Vec<ToolCall>) -> Self {
        Self {
            message: Message::assistant_with_tools(content.map(|s| s.to_string()), calls),
            usage: Usage {
                prompt_tokens: 200,
                completion_tokens: 50,
                cost_usd: 0.002,
                cost_estimated: false,
                ..Default::default()
            },
            finish_reason: FinishReason::ToolUse,
        }
    }

    /// Empty response — no content, no tool calls. Used to drive the
    /// retry-and-fallback path in tool loop tests.
    pub fn empty() -> Self {
        Self {
            message: Message::assistant_text(""),
            usage: Usage::default(),
            finish_reason: FinishReason::Other,
        }
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }
}

/// What kind of failure to inject.
#[derive(Debug, Clone)]
pub enum ScriptedError {
    /// The chat call returns Err. Useful for testing retry + fallback.
    Transport(String),
}

/// One element of the script: either a successful response or an error.
#[derive(Debug, Clone)]
pub enum ScriptStep {
    Respond(ScriptedResponse),
    Fail(ScriptedError),
}

/// A captured request the mock received. Tests assert against this.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    pub model: String,
    pub message_count: usize,
    pub tool_count: usize,
    pub last_user_text: Option<String>,
}

impl CapturedRequest {
    fn from(req: &ChatRequest) -> Self {
        let last_user_text = req
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::types::MessageRole::User))
            .map(|m| m.text_concat());
        Self {
            model: req.model.clone(),
            message_count: req.messages.len(),
            tool_count: req.tools.len(),
            last_user_text,
        }
    }
}

/// The mock client itself.
#[derive(Clone)]
pub struct MockLlmClient {
    inner: Arc<Mutex<MockState>>,
    default_model: String,
    models: Vec<String>,
}

struct MockState {
    script: Vec<ScriptStep>,
    cursor: usize,
    requests: Vec<CapturedRequest>,
}

impl MockLlmClient {
    /// Build a new mock with a known default model and a script.
    pub fn new(default_model: impl Into<String>, script: Vec<ScriptStep>) -> Self {
        let default_model = default_model.into();
        Self {
            inner: Arc::new(Mutex::new(MockState {
                script,
                cursor: 0,
                requests: Vec::new(),
            })),
            models: vec![default_model.clone()],
            default_model,
        }
    }

    /// Convenience: a mock that always returns one text response.
    pub fn always_text(default_model: impl Into<String>, text: impl Into<String>) -> Self {
        let r = ScriptedResponse::text(text);
        // Wrap in a long-enough script to survive being polled many
        // times by long-running tests. We clone the same response.
        let script = (0..1024)
            .map(|_| ScriptStep::Respond(r.clone()))
            .collect();
        Self::new(default_model, script)
    }

    /// Configure additional advertised models (for `list_models`).
    pub fn with_models(mut self, models: Vec<String>) -> Self {
        self.models = models;
        self
    }

    /// Snapshot of all requests received so far. Cheap clone.
    pub fn captured(&self) -> Vec<CapturedRequest> {
        self.inner.lock().unwrap().requests.clone()
    }

    /// How many requests have been served (including failures).
    pub fn call_count(&self) -> usize {
        self.inner.lock().unwrap().cursor
    }

    /// How many script steps remain.
    pub fn remaining(&self) -> usize {
        let st = self.inner.lock().unwrap();
        st.script.len().saturating_sub(st.cursor)
    }
}

#[async_trait]
impl LlmClient for MockLlmClient {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        let captured = CapturedRequest::from(&req);
        let mut st = self.inner.lock().unwrap();
        st.requests.push(captured);

        if st.cursor >= st.script.len() {
            return Err(anyhow!(
                "MockLlmClient: script exhausted at call {} (no more responses queued)",
                st.cursor
            ));
        }
        let step = st.script[st.cursor].clone();
        st.cursor += 1;
        drop(st);

        match step {
            ScriptStep::Respond(r) => Ok(ChatResponse {
                message: r.message,
                usage: r.usage,
                finish_reason: r.finish_reason,
                provider_id: Some(format!("mock-{}", self.call_count())),
            }),
            ScriptStep::Fail(ScriptedError::Transport(msg)) => {
                Err(anyhow!("MockLlmClient transport error: {}", msg))
            }
        }
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn list_models(&self) -> Vec<String> {
        self.models.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    fn req(text: &str) -> ChatRequest {
        ChatRequest::new("anthropic/claude-sonnet-4.6", vec![Message::user_text(text)])
    }

    #[tokio::test]
    async fn always_text_returns_same_string() {
        let mock = MockLlmClient::always_text("anthropic/claude-sonnet-4.6", "hello");
        let resp = mock.chat(req("hi")).await.unwrap();
        assert_eq!(resp.message.text_concat(), "hello");
        assert!(matches!(resp.finish_reason, FinishReason::Stop));
        assert!(!resp.is_empty());
    }

    #[tokio::test]
    async fn script_plays_in_order() {
        let mock = MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            vec![
                ScriptStep::Respond(ScriptedResponse::text("first")),
                ScriptStep::Respond(ScriptedResponse::text("second")),
            ],
        );
        assert_eq!(mock.chat(req("a")).await.unwrap().message.text_concat(), "first");
        assert_eq!(mock.chat(req("b")).await.unwrap().message.text_concat(), "second");
    }

    #[tokio::test]
    async fn exhausted_script_errors_loudly() {
        let mock = MockLlmClient::new(
            "anthropic/claude-sonnet-4.6",
            vec![ScriptStep::Respond(ScriptedResponse::text("only"))],
        );
        let _ = mock.chat(req("a")).await.unwrap();
        let err = mock.chat(req("b")).await.unwrap_err();
        assert!(err.to_string().contains("script exhausted"));
    }

    #[tokio::test]
    async fn captures_requests() {
        let mock = MockLlmClient::always_text("m", "ok");
        mock.chat(req("hello")).await.unwrap();
        mock.chat(req("world")).await.unwrap();
        let cap = mock.captured();
        assert_eq!(cap.len(), 2);
        assert_eq!(cap[0].last_user_text.as_deref(), Some("hello"));
        assert_eq!(cap[1].last_user_text.as_deref(), Some("world"));
    }

    #[tokio::test]
    async fn empty_response_round_trips_through_is_empty() {
        let mock = MockLlmClient::new(
            "m",
            vec![ScriptStep::Respond(ScriptedResponse::empty())],
        );
        let resp = mock.chat(req("x")).await.unwrap();
        assert!(resp.is_empty());
    }

    #[tokio::test]
    async fn transport_failure_returns_err() {
        let mock = MockLlmClient::new(
            "m",
            vec![ScriptStep::Fail(ScriptedError::Transport(
                "network down".into(),
            ))],
        );
        let err = mock.chat(req("x")).await.unwrap_err();
        assert!(err.to_string().contains("network down"));
    }

    #[tokio::test]
    async fn tool_call_response_round_trips() {
        let calls = vec![ToolCall {
            id: "call_1".into(),
            name: "fs_read".into(),
            arguments: r#"{"path":"VERSION"}"#.into(),
        }];
        let mock = MockLlmClient::new(
            "m",
            vec![ScriptStep::Respond(ScriptedResponse::tool_calls(
                Some("let me read VERSION"),
                calls.clone(),
            ))],
        );
        let resp = mock.chat(req("read VERSION")).await.unwrap();
        assert_eq!(resp.message.tool_calls.len(), 1);
        assert_eq!(resp.message.tool_calls[0].name, "fs_read");
        assert_eq!(resp.message.text_concat(), "let me read VERSION");
        assert!(matches!(resp.finish_reason, FinishReason::ToolUse));
    }
}
