//! OpenRouter HTTP client.
//!
//! Hand-rolled per STACK_DECISION §5: no third-party SDK at v0.1, so we
//! aren't blocked on crate maintainers tracking provider changes. We
//! implement just the OpenAI-compatible chat completions surface that
//! strange-loop actually uses.
//!
//! Provider pinning: when the model id starts with `anthropic/`, we
//! attach a `provider` block forcing OpenRouter to route to Anthropic
//! with no fallbacks. Without this, OpenRouter is free to route around
//! a temporarily-unavailable Anthropic shard, and the alternative
//! provider may not honor `cache_control`, which silently makes 100k
//! tokens uncached and ten times more expensive. This is the single
//! most expensive bug class the v0.1 client must prevent.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::types::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, Message, MessageRole, ToolCall,
    ToolChoice, Usage,
};
use crate::LlmClient;

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// OpenRouter client config.
#[derive(Debug, Clone)]
pub struct OpenRouterConfig {
    pub api_key: String,
    pub base_url: String,
    pub default_model: String,
    pub known_models: Vec<String>,
    pub timeout: Duration,
    /// HTTP-Referer header for OpenRouter analytics. Optional but polite.
    pub referer: Option<String>,
    pub title: Option<String>,
}

impl OpenRouterConfig {
    pub fn new(api_key: impl Into<String>, default_model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            default_model: default_model.into(),
            known_models: Vec::new(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            referer: Some("https://github.com/wsylvest/strange-loop".to_string()),
            title: Some("strange-loop".to_string()),
        }
    }
}

/// The client.
pub struct OpenRouterClient {
    cfg: OpenRouterConfig,
    http: reqwest::Client,
}

impl OpenRouterClient {
    pub fn new(cfg: OpenRouterConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .context("building reqwest client")?;
        Ok(Self { cfg, http })
    }

    /// Build the request JSON body.
    fn build_body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(message_to_json).collect();

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "max_tokens": req.max_tokens,
        });

        // Tools (if any). The last tool gets cache_control if no tool
        // already has one; this caches the entire tool list on Anthropic.
        if !req.tools.is_empty() {
            let mut tools_json: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    let mut function = json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    });
                    if let Some(cc) = &t.cache_control {
                        function["cache_control"] = json!({
                            "type": cc.kind,
                            "ttl": cc.ttl,
                        });
                    }
                    json!({ "type": "function", "function": function })
                })
                .collect();
            // If no tool already had cache_control, mark the last one.
            let any_cached = req.tools.iter().any(|t| t.cache_control.is_some());
            if !any_cached {
                if let Some(last) = tools_json.last_mut() {
                    if let Some(func) = last.get_mut("function").and_then(|f| f.as_object_mut()) {
                        func.insert(
                            "cache_control".to_string(),
                            json!({ "type": "ephemeral", "ttl": "1h" }),
                        );
                    }
                }
            }
            body["tools"] = json!(tools_json);
            body["tool_choice"] = match req.tool_choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::None => json!("none"),
                ToolChoice::Required => json!("required"),
            };
        }

        // Reasoning effort, signaled via OpenRouter's extra_body convention.
        body["reasoning"] = json!({
            "effort": req.effort.as_str(),
            "exclude": true,
        });

        // Anthropic provider pinning. Without this, OpenRouter can route
        // to a non-Anthropic backend that doesn't honor cache_control and
        // the prompt cache silently goes away.
        if req.model.starts_with("anthropic/") {
            body["provider"] = json!({
                "order": ["Anthropic"],
                "allow_fallbacks": false,
                "require_parameters": true,
            });
        }

        body
    }
}

#[async_trait]
impl LlmClient for OpenRouterClient {
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.cfg.base_url);
        let body = self.build_body(&req);
        debug!(
            url = %url,
            model = %req.model,
            messages = req.messages.len(),
            tools = req.tools.len(),
            "openrouter chat request"
        );

        let mut http_req = self
            .http
            .post(&url)
            .bearer_auth(&self.cfg.api_key)
            .json(&body);
        if let Some(r) = &self.cfg.referer {
            http_req = http_req.header("HTTP-Referer", r);
        }
        if let Some(t) = &self.cfg.title {
            http_req = http_req.header("X-Title", t);
        }

        let resp = http_req
            .send()
            .await
            .context("sending openrouter chat request")?;
        let status = resp.status();
        let bytes = resp.bytes().await.context("reading response body")?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(anyhow!(
                "openrouter http {}: {}",
                status.as_u16(),
                body.chars().take(500).collect::<String>()
            ));
        }

        let parsed: OpenRouterResponse = serde_json::from_slice(&bytes)
            .with_context(|| {
                let preview: String = String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(300)
                    .collect();
                format!("parsing openrouter response: {}", preview)
            })?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("openrouter returned no choices"))?;

        let message = openrouter_message_to_internal(choice.message);
        let finish_reason = parse_finish_reason(choice.finish_reason.as_deref());

        let usage_raw = parsed.usage.unwrap_or_default();
        let cost_usd = usage_raw.cost.unwrap_or(0.0);
        let cost_estimated = usage_raw.cost.is_none();
        let cached_tokens = usage_raw
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        let cache_write_tokens = usage_raw
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| {
                d.cache_write_tokens
                    .or(d.cache_creation_tokens)
                    .or(d.cache_creation_input_tokens)
            })
            .unwrap_or(0);

        if cost_estimated {
            warn!(
                model = %req.model,
                "openrouter response did not include cost; budget will rely on local pricing table"
            );
        }

        let usage = Usage {
            prompt_tokens: usage_raw.prompt_tokens.unwrap_or(0),
            completion_tokens: usage_raw.completion_tokens.unwrap_or(0),
            cached_tokens,
            cache_write_tokens,
            cost_usd,
            cost_estimated,
        };

        Ok(ChatResponse {
            message,
            usage,
            finish_reason,
            provider_id: parsed.id,
        })
    }

    fn default_model(&self) -> &str {
        &self.cfg.default_model
    }

    fn list_models(&self) -> Vec<String> {
        if self.cfg.known_models.is_empty() {
            vec![self.cfg.default_model.clone()]
        } else {
            self.cfg.known_models.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// JSON ↔ internal type conversions
// ---------------------------------------------------------------------------

fn message_to_json(msg: &Message) -> Value {
    let role = match msg.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };

    // For tool result messages, OpenAI-compat takes a string `content`
    // and a `tool_call_id`. For everything else, content is an array
    // of blocks (text and image), or — when there's just one plain
    // text block with no cache control — a flat string for brevity.
    let content_value: Value = if msg.role == MessageRole::Tool {
        Value::String(msg.text_concat())
    } else if msg.content.len() == 1 {
        match &msg.content[0] {
            ContentBlock::Text {
                text,
                cache_control: None,
            } => Value::String(text.clone()),
            block => Value::Array(vec![block_to_json(block)]),
        }
    } else {
        Value::Array(msg.content.iter().map(block_to_json).collect())
    };

    let mut out = serde_json::Map::new();
    out.insert("role".into(), Value::String(role.into()));
    out.insert("content".into(), content_value);
    if !msg.tool_calls.is_empty() {
        out.insert(
            "tool_calls".into(),
            Value::Array(
                msg.tool_calls
                    .iter()
                    .map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments,
                            }
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(id) = &msg.tool_call_id {
        out.insert("tool_call_id".into(), Value::String(id.clone()));
    }
    Value::Object(out)
}

fn block_to_json(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text {
            text,
            cache_control,
        } => {
            let mut o = json!({ "type": "text", "text": text });
            if let Some(cc) = cache_control {
                o["cache_control"] = json!({ "type": cc.kind, "ttl": cc.ttl });
            }
            o
        }
        ContentBlock::ImageBase64 { mime, data } => json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", mime, data) }
        }),
        ContentBlock::ImageUrl { url } => json!({
            "type": "image_url",
            "image_url": { "url": url }
        }),
    }
}

fn openrouter_message_to_internal(msg: OpenRouterMessage) -> Message {
    let content_blocks = match msg.content {
        Some(OrContent::Text(s)) if !s.is_empty() => vec![ContentBlock::text(s)],
        Some(OrContent::Text(_)) => Vec::new(),
        Some(OrContent::Blocks(blocks)) => blocks
            .into_iter()
            .filter_map(|b| match b {
                OrBlock::Text { text } => Some(ContentBlock::text(text)),
                _ => None,
            })
            .collect(),
        None => Vec::new(),
    };
    let tool_calls = msg
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|tc| ToolCall {
            id: tc.id,
            name: tc.function.name,
            arguments: tc.function.arguments,
        })
        .collect();
    Message {
        role: MessageRole::Assistant,
        content: content_blocks,
        tool_calls,
        tool_call_id: None,
    }
}

fn parse_finish_reason(s: Option<&str>) -> FinishReason {
    match s {
        Some("stop") | Some("end_turn") => FinishReason::Stop,
        Some("length") | Some("max_tokens") => FinishReason::Length,
        Some("tool_calls") | Some("tool_use") => FinishReason::ToolUse,
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

// ---------------------------------------------------------------------------
// OpenRouter wire types (deserialize-side only)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct OpenRouterResponse {
    id: Option<String>,
    choices: Vec<OpenRouterChoice>,
    usage: Option<OpenRouterUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChoice {
    message: OpenRouterMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterMessage {
    #[allow(dead_code)]
    role: Option<String>,
    content: Option<OrContent>,
    tool_calls: Option<Vec<OrToolCall>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OrContent {
    Text(String),
    Blocks(Vec<OrBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OrBlock {
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct OrToolCall {
    id: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    kind: Option<String>,
    function: OrToolCallFunction,
}

#[derive(Debug, Deserialize)]
struct OrToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Default, Deserialize)]
struct OpenRouterUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    #[allow(dead_code)]
    total_tokens: Option<u32>,
    cost: Option<f64>,
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetails {
    cached_tokens: Option<u32>,
    cache_write_tokens: Option<u32>,
    cache_creation_tokens: Option<u32>,
    cache_creation_input_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatRequest, Message, ToolSchema};

    fn cfg() -> OpenRouterConfig {
        OpenRouterConfig::new("sk-test", "anthropic/claude-sonnet-4.6")
    }

    #[test]
    fn build_body_includes_anthropic_provider_pin() {
        let client = OpenRouterClient::new(cfg()).unwrap();
        let req = ChatRequest::new(
            "anthropic/claude-sonnet-4.6",
            vec![Message::user_text("hi")],
        );
        let body = client.build_body(&req);
        let provider = body.get("provider").expect("provider pin present");
        assert_eq!(provider["allow_fallbacks"], false);
        assert_eq!(provider["order"][0], "Anthropic");
    }

    #[test]
    fn build_body_omits_provider_pin_for_non_anthropic() {
        let client = OpenRouterClient::new(cfg()).unwrap();
        let req = ChatRequest::new("openai/gpt-4.1", vec![Message::user_text("hi")]);
        let body = client.build_body(&req);
        assert!(
            body.get("provider").is_none(),
            "non-anthropic models should not be pinned"
        );
    }

    #[test]
    fn build_body_marks_last_tool_with_cache_control() {
        let client = OpenRouterClient::new(cfg()).unwrap();
        let mut req = ChatRequest::new(
            "anthropic/claude-sonnet-4.6",
            vec![Message::user_text("hi")],
        );
        req.tools.push(ToolSchema::new("a", "first", json!({})));
        req.tools.push(ToolSchema::new("b", "second", json!({})));
        let body = client.build_body(&req);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        // Last tool should carry cache_control; first should not.
        assert!(tools[0]["function"].get("cache_control").is_none());
        let cc = tools[1]["function"]
            .get("cache_control")
            .expect("last tool cache_control present");
        assert_eq!(cc["type"], "ephemeral");
    }

    #[test]
    fn build_body_passes_reasoning_effort() {
        let client = OpenRouterClient::new(cfg()).unwrap();
        let mut req = ChatRequest::new(
            "anthropic/claude-sonnet-4.6",
            vec![Message::user_text("hi")],
        );
        req.effort = crate::types::Effort::High;
        let body = client.build_body(&req);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["exclude"], true);
    }

    #[test]
    fn message_to_json_inlines_single_text_block_as_string() {
        let msg = Message::user_text("hello");
        let v = message_to_json(&msg);
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hello");
    }

    #[test]
    fn tool_result_message_uses_string_content_and_tool_call_id() {
        let msg = Message::tool_result("call_42", "result text");
        let v = message_to_json(&msg);
        assert_eq!(v["role"], "tool");
        assert_eq!(v["content"], "result text");
        assert_eq!(v["tool_call_id"], "call_42");
    }

    #[test]
    fn assistant_with_tool_calls_serializes_calls() {
        let calls = vec![ToolCall {
            id: "c1".into(),
            name: "fs_read".into(),
            arguments: r#"{"path":"VERSION"}"#.into(),
        }];
        let msg = Message::assistant_with_tools(Some("reading".into()), calls);
        let v = message_to_json(&msg);
        let tcs = v["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["function"]["name"], "fs_read");
        assert_eq!(tcs[0]["function"]["arguments"], r#"{"path":"VERSION"}"#);
        assert_eq!(tcs[0]["type"], "function");
    }

    #[test]
    fn parse_finish_reason_handles_anthropic_and_openai_styles() {
        assert!(matches!(parse_finish_reason(Some("stop")), FinishReason::Stop));
        assert!(matches!(parse_finish_reason(Some("end_turn")), FinishReason::Stop));
        assert!(matches!(
            parse_finish_reason(Some("tool_use")),
            FinishReason::ToolUse
        ));
        assert!(matches!(
            parse_finish_reason(Some("tool_calls")),
            FinishReason::ToolUse
        ));
        assert!(matches!(parse_finish_reason(Some("length")), FinishReason::Length));
        assert!(matches!(parse_finish_reason(None), FinishReason::Other));
    }

    #[test]
    fn openrouter_response_parses_typical_anthropic_body() {
        let body = r#"{
            "id": "gen-abc",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hello world"
                },
                "finish_reason": "end_turn"
            }],
            "usage": {
                "prompt_tokens": 1234,
                "completion_tokens": 56,
                "cost": 0.0123,
                "prompt_tokens_details": {
                    "cached_tokens": 1000
                }
            }
        }"#;
        let parsed: OpenRouterResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("gen-abc"));
        assert_eq!(parsed.choices.len(), 1);
        let usage = parsed.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(1234));
        assert_eq!(usage.cost, Some(0.0123));
        assert_eq!(
            usage.prompt_tokens_details.unwrap().cached_tokens,
            Some(1000)
        );
    }

    #[test]
    fn openrouter_response_handles_missing_cost() {
        let body = r#"{
            "choices": [{ "message": {"role":"assistant","content":"x"}, "finish_reason":"stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
        }"#;
        let parsed: OpenRouterResponse = serde_json::from_str(body).unwrap();
        let usage = parsed.usage.unwrap();
        assert!(usage.cost.is_none(), "missing cost should be None for estimation fallback");
    }

    #[test]
    fn openrouter_response_handles_tool_calls() {
        let body = r#"{
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "tc1",
                        "type": "function",
                        "function": { "name": "fs_read", "arguments": "{\"path\":\"v\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "cost": 0.0 }
        }"#;
        let parsed: OpenRouterResponse = serde_json::from_str(body).unwrap();
        let msg = openrouter_message_to_internal(parsed.choices.into_iter().next().unwrap().message);
        assert_eq!(msg.tool_calls.len(), 1);
        assert_eq!(msg.tool_calls[0].name, "fs_read");
        assert!(msg.text_concat().is_empty());
    }
}
