//! strange-loop LLM client — provider abstraction.
//!
//! This crate is the only place in the system that knows how to talk
//! to a model provider. The rest of the codebase depends on the
//! `LlmClient` trait and the message/usage/tool types defined here.
//!
//! See `docs/SYSTEM_SPEC.md` §6.4 for the design rationale.

pub mod mock;
pub mod openrouter;
pub mod types;

pub use types::{
    CacheControl, ChatRequest, ChatResponse, ContentBlock, Effort, FinishReason, Message,
    MessageRole, ToolCall, ToolChoice, ToolSchema, Usage,
};

use anyhow::Result;
use async_trait::async_trait;

/// The LLM client trait. All providers (OpenRouter, native Anthropic,
/// the mock used in tests) implement this.
///
/// Implementations must be `Send + Sync` because the tool loop holds a
/// shared reference to the client across async tasks.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Send one chat request and await the response.
    ///
    /// Errors propagate from the transport (HTTP, JSON parse, mock
    /// script ran out of canned responses). Empty model responses
    /// (no content + no tool calls) are NOT errors here — they are
    /// returned as `ChatResponse` with an empty `content` and empty
    /// `tool_calls`, and the tool loop is responsible for retrying.
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse>;

    /// The default model id this client is configured with.
    fn default_model(&self) -> &str;

    /// All known models for this client. Used by `switch_model` tool to
    /// validate target ids before applying an override.
    fn list_models(&self) -> Vec<String>;
}
