//! Transport adapter trait.
//!
//! An adapter bridges a transport (CLI, Telegram, Slack, ...) to the
//! core's owner-message channel. The core doesn't know what a terminal
//! or a Telegram chat is; it knows how to receive `OwnerMessage`s and
//! send `AgentMessage`s, and the adapter maps both directions.
//!
//! The trait is deliberately small. Bigger capabilities (image I/O,
//! typing indicators, supervisor commands) are separate methods with
//! reasonable defaults so an adapter that can't do them doesn't have
//! to implement them.
//!
//! See `docs/SYSTEM_SPEC.md` §7 for the adapter design.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// One message from the owner, coming in through a transport.
#[derive(Debug, Clone)]
pub struct OwnerMessage {
    /// Unique id for dedup. For CLI, this is a monotonic counter;
    /// for Telegram it's the Telegram message_id.
    pub msg_id: String,
    /// Which adapter produced this message.
    pub adapter: String,
    /// The text content. Adapters strip transport-specific markup.
    pub text: String,
    /// When the adapter received it.
    pub received_at: DateTime<Utc>,
}

impl OwnerMessage {
    pub fn new(adapter: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            msg_id: uuid::Uuid::new_v4().to_string(),
            adapter: adapter.into(),
            text: text.into(),
            received_at: Utc::now(),
        }
    }
}

/// One message from the agent, going out through a transport.
#[derive(Debug, Clone)]
pub struct AgentMessage {
    /// The task this response belongs to, if any.
    pub task_id: Option<String>,
    /// The text to present to the owner.
    pub text: String,
    /// What kind of message this is. Proactive messages from
    /// background consciousness are rate-limited separately.
    pub kind: AgentMessageKind,
}

impl AgentMessage {
    pub fn response(task_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            task_id: Some(task_id.into()),
            text: text.into(),
            kind: AgentMessageKind::Response,
        }
    }

    pub fn proactive(text: impl Into<String>) -> Self {
        Self {
            task_id: None,
            text: text.into(),
            kind: AgentMessageKind::Proactive,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessageKind {
    /// Reply to an owner message.
    Response,
    /// Unsolicited message from background consciousness.
    Proactive,
    /// Progress update during a long task (streamed, optional).
    Progress,
    /// Error message surfaced to the owner.
    Error,
}

impl AgentMessageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::Proactive => "proactive",
            Self::Progress => "progress",
            Self::Error => "error",
        }
    }
}

/// The transport adapter trait. Implementations bridge one transport
/// (stdin/stdout, Telegram, Slack) to the core's message channel.
#[async_trait]
pub trait Adapter: Send + Sync {
    /// Short name for logs: "cli", "telegram", etc.
    fn name(&self) -> &str;

    /// Send an agent message out through this transport. This is the
    /// primary call path; every adapter must implement it.
    async fn send(&self, msg: AgentMessage) -> Result<()>;

    /// Receive the next owner message. Blocks until one arrives.
    /// Returns `Ok(None)` when the transport shuts down cleanly
    /// (EOF on stdin, Telegram bot stopped, etc.).
    async fn receive(&self) -> Result<Option<OwnerMessage>>;

    /// Signal that the agent is typing. Optional — the default
    /// is a no-op for adapters that don't support it.
    async fn typing(&self) -> Result<()> {
        Ok(())
    }
}
