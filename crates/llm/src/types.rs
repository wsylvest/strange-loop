//! Cross-provider message and usage types.
//!
//! These mirror the OpenAI/Anthropic Messages API shape closely enough
//! that converting to provider-native is mechanical, while remaining
//! Rust-idiomatic (enums for roles, typed cache-control, structured
//! usage). The OpenRouter client is the canonical bidirectional
//! converter; other providers are written against this same vocabulary.

use serde::{Deserialize, Serialize};

/// Role of a message in a chat conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

/// Cache control hint, attached to a `ContentBlock` to indicate that
/// the provider should treat its prefix as a cacheable boundary.
/// Currently only Anthropic acts on this; other providers ignore it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CacheControl {
    /// Always "ephemeral" today; the field exists for forward compat.
    #[serde(rename = "type")]
    pub kind: String,
    /// Optional TTL hint ("5m", "1h"). Provider may ignore.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            kind: "ephemeral".to_string(),
            ttl: None,
        }
    }
    pub fn ephemeral_ttl(ttl: impl Into<String>) -> Self {
        Self {
            kind: "ephemeral".to_string(),
            ttl: Some(ttl.into()),
        }
    }
}

/// One block of content inside a message. Messages with simple text
/// can use a single `Text` block; multi-part messages (text + image,
/// or cache-broken system prompts with multiple blocks) use several.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// A base64-encoded image with a mime type. URL form is also
    /// supported via `ImageUrl`.
    ImageBase64 {
        mime: String,
        data: String,
    },
    ImageUrl {
        url: String,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text {
            text: s.into(),
            cache_control: None,
        }
    }
    pub fn text_cached(s: impl Into<String>, cache: CacheControl) -> Self {
        Self::Text {
            text: s.into(),
            cache_control: Some(cache),
        }
    }
}

/// One chat message. Content is `Vec<ContentBlock>` even for plain text;
/// the `Message::user_text` and `Message::system_text` constructors hide
/// this when callers don't care.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
    /// Tool calls emitted by an assistant message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For tool-result messages, the id of the call this is responding to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user_text(s: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: vec![ContentBlock::text(s)],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn system_text(s: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: vec![ContentBlock::text(s)],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant_text(s: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::text(s)],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant_with_tools(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content
                .map(|s| vec![ContentBlock::text(s)])
                .unwrap_or_default(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: vec![ContentBlock::text(output)],
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }

    /// Concatenate all text blocks. Useful for logging previews and
    /// for token estimation. Image blocks are skipped.
    pub fn text_concat(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let ContentBlock::Text { text, .. } = block {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }
}

/// A tool call emitted by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments. Kept as a string because that is how
    /// every provider reports it; the dispatcher parses on demand.
    pub arguments: String,
}

/// JSON-Schema for one tool, advertised to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON-Schema object describing the parameters.
    pub parameters: serde_json::Value,
    /// If set, attach this cache-control to the tool when serializing
    /// to the provider. Used to mark the last tool in the list so that
    /// Anthropic prompt caching covers all tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl ToolSchema {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            cache_control: None,
        }
    }
}

/// Tool choice hint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    /// Let the model decide. The default and what we use almost always.
    #[default]
    Auto,
    /// Force the model to NOT call a tool this turn. Used for the
    /// final-answer round after a budget hard stop.
    None,
    /// Force the model to call any tool. Rarely used.
    Required,
}

/// Reasoning effort hint (relevant for o-series, Claude extended
/// thinking, and Gemini thinking models). Providers that don't honor
/// this just ignore it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
}

impl Effort {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    /// Numeric rank for comparison ("did the LLM raise or lower effort?").
    pub fn rank(&self) -> u8 {
        match self {
            Self::None => 0,
            Self::Minimal => 1,
            Self::Low => 2,
            Self::Medium => 3,
            Self::High => 4,
            Self::Xhigh => 5,
        }
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Model produced an end-of-turn marker normally.
    Stop,
    /// Hit max_tokens.
    Length,
    /// Model emitted tool calls that the host should now execute.
    ToolUse,
    /// Provider safety filter or content policy.
    ContentFilter,
    /// Anything we can't classify.
    Other,
}

/// Token + cost usage for one LLM call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cached_tokens: u32,
    pub cache_write_tokens: u32,
    /// USD cost as reported by the provider, or estimated by the client
    /// from a pricing table if the provider doesn't return one.
    pub cost_usd: f64,
    /// True when `cost_usd` was estimated rather than provider-reported.
    /// This bit is what the budget-drift detector compares against.
    pub cost_estimated: bool,
}

impl Usage {
    pub fn zero() -> Self {
        Self::default()
    }

    /// Accumulate another usage into this one.
    pub fn add(&mut self, other: &Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.cost_usd += other.cost_usd;
        // If any leg was estimated, mark the total estimated.
        self.cost_estimated = self.cost_estimated || other.cost_estimated;
    }
}

/// One request to the LLM client.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub tool_choice: ToolChoice,
    pub effort: Effort,
    pub max_tokens: u32,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            effort: Effort::Medium,
            max_tokens: 16_384,
        }
    }
}

/// One response from the LLM client.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// The assistant message returned.
    pub message: Message,
    pub usage: Usage,
    pub finish_reason: FinishReason,
    /// Provider-specific id, useful for replay and for late cost
    /// reconciliation via OpenRouter's /generation endpoint.
    pub provider_id: Option<String>,
}

impl ChatResponse {
    /// Was this an empty response — no content and no tool calls?
    /// The tool loop uses this to decide whether to retry or fall back.
    pub fn is_empty(&self) -> bool {
        self.message.tool_calls.is_empty() && self.message.text_concat().trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_text_concat_joins_blocks() {
        let mut msg = Message::system_text("first");
        msg.content.push(ContentBlock::text("second"));
        assert_eq!(msg.text_concat(), "first\nsecond");
    }

    #[test]
    fn usage_add_accumulates() {
        let mut a = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            cost_usd: 0.01,
            cost_estimated: false,
            ..Default::default()
        };
        let b = Usage {
            prompt_tokens: 200,
            completion_tokens: 75,
            cost_usd: 0.02,
            cost_estimated: true,
            ..Default::default()
        };
        a.add(&b);
        assert_eq!(a.prompt_tokens, 300);
        assert_eq!(a.completion_tokens, 125);
        assert!((a.cost_usd - 0.03).abs() < 1e-9);
        assert!(a.cost_estimated, "estimated bit should be sticky");
    }

    #[test]
    fn effort_rank_ordering() {
        assert!(Effort::Low.rank() < Effort::High.rank());
        assert!(Effort::Medium.rank() < Effort::Xhigh.rank());
        assert_eq!(Effort::default(), Effort::Medium);
    }

    #[test]
    fn empty_chat_response_detected() {
        let resp = ChatResponse {
            message: Message::assistant_text(""),
            usage: Usage::zero(),
            finish_reason: FinishReason::Stop,
            provider_id: None,
        };
        assert!(resp.is_empty());

        let resp2 = ChatResponse {
            message: Message::assistant_text("hello"),
            usage: Usage::zero(),
            finish_reason: FinishReason::Stop,
            provider_id: None,
        };
        assert!(!resp2.is_empty());
    }

    #[test]
    fn cache_control_ephemeral_helpers() {
        let cc = CacheControl::ephemeral();
        assert_eq!(cc.kind, "ephemeral");
        assert!(cc.ttl.is_none());
        let cc2 = CacheControl::ephemeral_ttl("1h");
        assert_eq!(cc2.ttl.as_deref(), Some("1h"));
    }
}
