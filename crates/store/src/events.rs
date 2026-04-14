//! Event log — the append-only spine of strange-loop observability.
//!
//! Every significant runtime action becomes an Event. The full list of
//! event kinds is in `docs/SYSTEM_SPEC.md` §5; this module defines the
//! Rust enum that mirrors it and the read/write paths that touch the
//! `events` table.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::Store;

/// The complete set of event types strange-loop emits. Stringly-typed
/// via lowercase snake_case when stored so that queries like
/// `WHERE event_type = 'llm_usage'` remain readable in the shell.
///
/// Adding a variant requires only (a) adding it here and (b) emitting
/// it somewhere. The schema is `event_type TEXT`, so there is no
/// migration cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted,
    TaskReceived,
    TaskStarted,
    LlmRound,
    LlmUsage,
    LlmEmptyResponse,
    LlmApiError,
    ToolCall,
    ToolResult,
    ToolError,
    ToolTimeout,
    ToolDetachedLaunched,
    ToolDetachedDone,
    OwnerMessage,
    OwnerMessageInjected,
    AgentMessage,
    ScratchUpdate,
    JournalAppend,
    IdentityUpdate,
    CreedProposalSubmitted,
    CreedProposalDecided,
    KnowledgeWrite,
    TaskMetrics,
    TaskDone,
    TaskCancelled,
    RestartRequested,
    RestartCompleted,
    BudgetDriftWarning,
    ConsciousnessThought,
    ConsciousnessWakeupSet,
    HealthInvariant,
    CriticalStorageEvent,
}

impl EventKind {
    /// Canonical string form used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStarted => "session_started",
            Self::TaskReceived => "task_received",
            Self::TaskStarted => "task_started",
            Self::LlmRound => "llm_round",
            Self::LlmUsage => "llm_usage",
            Self::LlmEmptyResponse => "llm_empty_response",
            Self::LlmApiError => "llm_api_error",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::ToolError => "tool_error",
            Self::ToolTimeout => "tool_timeout",
            Self::ToolDetachedLaunched => "tool_detached_launched",
            Self::ToolDetachedDone => "tool_detached_done",
            Self::OwnerMessage => "owner_message",
            Self::OwnerMessageInjected => "owner_message_injected",
            Self::AgentMessage => "agent_message",
            Self::ScratchUpdate => "scratch_update",
            Self::JournalAppend => "journal_append",
            Self::IdentityUpdate => "identity_update",
            Self::CreedProposalSubmitted => "creed_proposal_submitted",
            Self::CreedProposalDecided => "creed_proposal_decided",
            Self::KnowledgeWrite => "knowledge_write",
            Self::TaskMetrics => "task_metrics",
            Self::TaskDone => "task_done",
            Self::TaskCancelled => "task_cancelled",
            Self::RestartRequested => "restart_requested",
            Self::RestartCompleted => "restart_completed",
            Self::BudgetDriftWarning => "budget_drift_warning",
            Self::ConsciousnessThought => "consciousness_thought",
            Self::ConsciousnessWakeupSet => "consciousness_wakeup_set",
            Self::HealthInvariant => "health_invariant",
            Self::CriticalStorageEvent => "critical_storage_event",
        }
    }
}

/// A single event row. The payload is a JSON blob whose shape depends
/// on `kind`; see the table in `docs/SYSTEM_SPEC.md` §5 for the
/// payload schemas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Option<i64>,
    pub ts: i64,
    pub kind: EventKind,
    pub task_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub session_id: String,
    /// The JSON payload as a string. Callers typically build this from
    /// a serde struct via `serde_json::to_string`; the `append_payload`
    /// helper does that for you.
    pub payload: String,
}

impl Event {
    /// Build a minimal event with the current timestamp and no task
    /// context. Use for session-level events like `SessionStarted`.
    pub fn session(session_id: impl Into<String>, kind: EventKind, payload: String) -> Self {
        Self {
            id: None,
            ts: Utc::now().timestamp_millis(),
            kind,
            task_id: None,
            parent_task_id: None,
            session_id: session_id.into(),
            payload,
        }
    }
}

/// Append an event row. Returns the row id.
pub fn append(store: &Store, event: &Event) -> Result<i64> {
    store.with_conn(|conn| {
        conn.execute(
            "INSERT INTO events (ts, event_type, task_id, parent_task_id, session_id, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.ts,
                event.kind.as_str(),
                event.task_id,
                event.parent_task_id,
                event.session_id,
                event.payload,
            ],
        )
        .context("inserting event row")?;
        Ok(conn.last_insert_rowid())
    })
}

/// Convenience: append an event whose payload is any `Serialize`.
pub fn append_payload<T: Serialize>(
    store: &Store,
    session_id: &str,
    kind: EventKind,
    task_id: Option<&str>,
    payload: &T,
) -> Result<i64> {
    let payload = serde_json::to_string(payload).context("serializing event payload")?;
    let event = Event {
        id: None,
        ts: Utc::now().timestamp_millis(),
        kind,
        task_id: task_id.map(|s| s.to_string()),
        parent_task_id: None,
        session_id: session_id.to_string(),
        payload,
    };
    append(store, &event)
}

/// Count events of a given kind in the session. Used by tests and by
/// the `strange-loop events` subcommand.
pub fn count_by_kind(store: &Store, session_id: &str, kind: EventKind) -> Result<i64> {
    store.with_conn(|conn| {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND event_type = ?2",
                params![session_id, kind.as_str()],
                |row| row.get(0),
            )
            .context("counting events")?;
        Ok(count)
    })
}

/// Tail the most recent events across all kinds for a session.
pub fn tail(store: &Store, session_id: &str, limit: usize) -> Result<Vec<Event>> {
    store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT id, ts, event_type, task_id, parent_task_id, session_id, payload
             FROM events
             WHERE session_id = ?1
             ORDER BY ts DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![session_id, limit as i64], |row| {
                let kind_str: String = row.get(2)?;
                Ok(Event {
                    id: Some(row.get(0)?),
                    ts: row.get(1)?,
                    kind: parse_event_kind(&kind_str).unwrap_or(EventKind::HealthInvariant),
                    task_id: row.get(3)?,
                    parent_task_id: row.get(4)?,
                    session_id: row.get(5)?,
                    payload: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

fn parse_event_kind(s: &str) -> Option<EventKind> {
    // Build a small match; keeps us from importing serde for a one-shot
    // deserialization at read time.
    use EventKind::*;
    Some(match s {
        "session_started" => SessionStarted,
        "task_received" => TaskReceived,
        "task_started" => TaskStarted,
        "llm_round" => LlmRound,
        "llm_usage" => LlmUsage,
        "llm_empty_response" => LlmEmptyResponse,
        "llm_api_error" => LlmApiError,
        "tool_call" => ToolCall,
        "tool_result" => ToolResult,
        "tool_error" => ToolError,
        "tool_timeout" => ToolTimeout,
        "tool_detached_launched" => ToolDetachedLaunched,
        "tool_detached_done" => ToolDetachedDone,
        "owner_message" => OwnerMessage,
        "owner_message_injected" => OwnerMessageInjected,
        "agent_message" => AgentMessage,
        "scratch_update" => ScratchUpdate,
        "journal_append" => JournalAppend,
        "identity_update" => IdentityUpdate,
        "creed_proposal_submitted" => CreedProposalSubmitted,
        "creed_proposal_decided" => CreedProposalDecided,
        "knowledge_write" => KnowledgeWrite,
        "task_metrics" => TaskMetrics,
        "task_done" => TaskDone,
        "task_cancelled" => TaskCancelled,
        "restart_requested" => RestartRequested,
        "restart_completed" => RestartCompleted,
        "budget_drift_warning" => BudgetDriftWarning,
        "consciousness_thought" => ConsciousnessThought,
        "consciousness_wakeup_set" => ConsciousnessWakeupSet,
        "health_invariant" => HealthInvariant,
        "critical_storage_event" => CriticalStorageEvent,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct TestPayload<'a> {
        note: &'a str,
    }

    #[test]
    fn append_and_count_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let id = append_payload(
            &store,
            "session-abc",
            EventKind::SessionStarted,
            None,
            &TestPayload { note: "first boot" },
        )
        .unwrap();
        assert!(id > 0);
        let count = count_by_kind(&store, "session-abc", EventKind::SessionStarted).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn tail_returns_events_newest_first() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            append_payload(
                &store,
                "s",
                EventKind::LlmUsage,
                Some("t1"),
                &TestPayload { note: &format!("e{}", i) },
            )
            .unwrap();
        }
        let events = tail(&store, "s", 3).unwrap();
        assert_eq!(events.len(), 3);
        // newest-first ordering by id when ts is identical
        assert!(events[0].id.unwrap() > events[1].id.unwrap());
    }

    #[test]
    fn event_kind_str_mapping_is_stable() {
        assert_eq!(EventKind::LlmUsage.as_str(), "llm_usage");
        assert_eq!(EventKind::JournalAppend.as_str(), "journal_append");
        assert_eq!(
            parse_event_kind("llm_usage"),
            Some(EventKind::LlmUsage)
        );
    }
}
