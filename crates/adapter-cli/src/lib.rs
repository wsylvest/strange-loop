//! CLI adapter — first-class transport for strange-loop.
//!
//! Two modes:
//!
//!   Interactive: stdin is a TTY. Print "you: " prompt, read a line,
//!     return it as an OwnerMessage. Agent responses are printed to
//!     stdout prefixed with "agent: ".
//!
//!   Piped: stdin is not a TTY (e.g. `echo "hi" | strange-loop chat`).
//!     Read all of stdin as a single message; after the agent responds,
//!     return None from the next `receive()` to signal EOF. This is
//!     what the M1 exit criterion (`echo "read VERSION" | strange-loop
//!     chat`) exercises.
//!
//! The adapter writes every message it receives or sends to the
//! `messages` table so the context builder's "Recent messages" section
//! can include them.

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sl_core::{Adapter, AgentMessage, AgentMessageKind, OwnerMessage};
use sl_core::SessionId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// How the CLI adapter reads input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliMode {
    /// stdin is a TTY — prompt, read lines, loop forever.
    Interactive,
    /// stdin is piped — read all of it as one message, then EOF.
    Piped,
    /// Auto-detect from stdin at startup.
    Auto,
}

pub struct CliAdapter {
    mode: CliMode,
    /// Shared store handle for persisting messages (so the context
    /// builder can include them in the "Recent messages" section).
    store: sl_store::Store,
    session_id: Arc<String>,
    /// Serializes stdout writes so concurrent `send` calls don't
    /// interleave output.
    stdout: Arc<Mutex<tokio::io::Stdout>>,
    /// For Piped mode, we return Some once and None thereafter.
    piped_message_consumed: Arc<tokio::sync::Mutex<bool>>,
}

impl CliAdapter {
    pub fn new(mode: CliMode, store: sl_store::Store, session_id: SessionId) -> Self {
        let resolved_mode = match mode {
            CliMode::Auto => {
                if std::io::stdin().is_terminal() {
                    CliMode::Interactive
                } else {
                    CliMode::Piped
                }
            }
            other => other,
        };
        Self {
            mode: resolved_mode,
            store,
            session_id: Arc::new(session_id.as_str().to_string()),
            stdout: Arc::new(Mutex::new(tokio::io::stdout())),
            piped_message_consumed: Arc::new(tokio::sync::Mutex::new(false)),
        }
    }

    /// Current resolved mode (useful for the binary to know whether
    /// to close the scheduler after piped input).
    pub fn mode(&self) -> CliMode {
        self.mode
    }

    /// Persist an incoming message to the `messages` table.
    fn persist_in(&self, text: &str) -> Result<()> {
        let ts = chrono::Utc::now().timestamp_millis();
        self.store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO messages (ts, direction, adapter, content)
                     VALUES (?1, 'in', 'cli', ?2)",
                    rusqlite::params![ts, text],
                )?;
                Ok(())
            })
            .context("persisting inbound cli message")
    }
}

#[async_trait]
impl Adapter for CliAdapter {
    fn name(&self) -> &str {
        "cli"
    }

    async fn send(&self, msg: AgentMessage) -> Result<()> {
        let _ = &self.session_id; // keep for future event emission paths
        let prefix = match msg.kind {
            AgentMessageKind::Response => "agent",
            AgentMessageKind::Proactive => "agent (proactive)",
            AgentMessageKind::Progress => "agent …",
            AgentMessageKind::Error => "agent error",
        };
        let line = format!("{prefix}: {}\n", msg.text);
        let mut out = self.stdout.lock().await;
        out.write_all(line.as_bytes())
            .await
            .context("writing to stdout")?;
        out.flush().await.context("flushing stdout")?;
        Ok(())
    }

    async fn receive(&self) -> Result<Option<OwnerMessage>> {
        match self.mode {
            CliMode::Piped | CliMode::Auto => {
                // Piped: read all of stdin at once, return it once, then EOF.
                let mut consumed = self.piped_message_consumed.lock().await;
                if *consumed {
                    return Ok(None);
                }
                let mut buf = String::new();
                let mut stdin = BufReader::new(tokio::io::stdin());
                loop {
                    let mut chunk = String::new();
                    let n = stdin
                        .read_line(&mut chunk)
                        .await
                        .context("reading piped stdin")?;
                    if n == 0 {
                        break;
                    }
                    buf.push_str(&chunk);
                }
                *consumed = true;
                let trimmed = buf.trim().to_string();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                self.persist_in(&trimmed)?;
                Ok(Some(OwnerMessage::new("cli", trimmed)))
            }
            CliMode::Interactive => {
                // Interactive: print prompt, read one line. Return None on EOF (Ctrl-D).
                {
                    let mut out = self.stdout.lock().await;
                    out.write_all(b"you: ").await.context("writing prompt")?;
                    out.flush().await.ok();
                }
                let stdin = tokio::io::stdin();
                let mut reader = BufReader::new(stdin);
                let mut line = String::new();
                let n = reader
                    .read_line(&mut line)
                    .await
                    .context("reading interactive stdin")?;
                if n == 0 {
                    // EOF
                    return Ok(None);
                }
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() {
                    // Empty line → loop again. Tail-call via recursion
                    // would work but Box::pin isn't needed if we just
                    // return a zero-result the binary handles — simpler
                    // to call ourselves once.
                    return Box::pin(self.receive()).await;
                }
                self.persist_in(&trimmed)?;
                Ok(Some(OwnerMessage::new("cli", trimmed)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_persistence_is_handled_by_runner_not_adapter() {
        // The adapter writes inbound messages but outbound messages
        // are written by the TaskRunner (see task_runner.rs
        // `persist_agent_message`). This test documents that contract
        // by building an adapter, sending a message through it, and
        // asserting that the messages table is untouched.
        let store = sl_store::Store::open_in_memory().unwrap();
        let session = SessionId::new();
        let adapter = CliAdapter::new(CliMode::Piped, store.clone(), session);

        adapter
            .send(AgentMessage::response("t1", "hello"))
            .await
            .unwrap();

        let count: i64 = store
            .with_conn(|c| {
                Ok(c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))?)
            })
            .unwrap();
        assert_eq!(count, 0, "adapter.send does not write to messages table");
    }

    #[test]
    fn mode_is_exposed() {
        let store = sl_store::Store::open_in_memory().unwrap();
        let adapter = CliAdapter::new(CliMode::Piped, store, SessionId::new());
        assert_eq!(adapter.mode(), CliMode::Piped);
    }
}
