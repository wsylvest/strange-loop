//! Task lifecycle: create, start, finish, fail, cancel.
//!
//! A Task is the unit the scheduler hands to a TaskRunner. It owns
//! the id, parent linkage, kind, depth, input text, and — as the
//! lifecycle progresses — started_at/finished_at/output/cost_usd/
//! rounds/error.
//!
//! All mutations write to the `tasks` table in the store. Events are
//! emitted at state transitions so the replay path (M5) can reconstruct
//! what happened without reading `tasks` directly.

use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use sl_store::{events, EventKind, Store};

use crate::context::TaskKind;

/// Task state machine:
///   Pending → Running → Done | Failed | Cancelled
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// A task.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub parent_id: Option<String>,
    pub kind: TaskKind,
    pub depth: u32,
    pub priority: i32,
    pub input_text: String,
    pub adapter: String,
    pub state: TaskState,
}

impl Task {
    /// Build a new pending task from an owner message.
    pub fn from_owner(text: impl Into<String>, adapter: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: None,
            kind: TaskKind::User,
            depth: 0,
            priority: 100,
            input_text: text.into(),
            adapter: adapter.into(),
            state: TaskState::Pending,
        }
    }

    /// Build a child task for `schedule_task` (M6). The parent's
    /// depth + 1 is the child's depth.
    pub fn as_child_of(parent: &Task, text: impl Into<String>, kind: TaskKind) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            parent_id: Some(parent.id.clone()),
            kind,
            depth: parent.depth + 1,
            priority: parent.priority + 1,
            input_text: text.into(),
            adapter: parent.adapter.clone(),
            state: TaskState::Pending,
        }
    }
}

#[derive(Serialize)]
struct TaskReceivedPayload<'a> {
    kind: &'a str,
    adapter: &'a str,
    depth: u32,
    input_preview: String,
}

#[derive(Serialize)]
struct TaskStartedPayload<'a> {
    task_id: &'a str,
}

#[derive(Serialize)]
struct TaskDonePayload<'a> {
    ok: bool,
    cost_usd: f64,
    rounds: u32,
    stop_reason: &'a str,
}

#[derive(Serialize)]
struct TaskCancelledPayload<'a> {
    reason: &'a str,
}

/// Insert a freshly-created task into the `tasks` table with state=pending.
/// Emits a `task_received` event.
pub fn record_pending(store: &Store, session_id: &str, task: &Task) -> Result<()> {
    let created_at = Utc::now().timestamp_millis();
    let kind_str = match task.kind {
        TaskKind::User => "user",
        TaskKind::Review => "review",
        TaskKind::Evolution => "evolution",
        TaskKind::Scheduled => "scheduled",
        TaskKind::Consciousness => "consciousness",
    };
    let input_json = serde_json::json!({
        "text": task.input_text,
        "adapter": task.adapter,
    })
    .to_string();

    store.with_conn(|conn| {
        conn.execute(
            "INSERT INTO tasks (id, parent_id, kind, state, depth, priority,
                                created_at, started_at, finished_at,
                                input, output, cost_usd, rounds, error)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6, NULL, NULL, ?7, NULL, 0, 0, NULL)",
            rusqlite::params![
                task.id,
                task.parent_id,
                kind_str,
                task.depth,
                task.priority,
                created_at,
                input_json,
            ],
        )?;
        Ok(())
    })?;

    events::append_payload(
        store,
        session_id,
        EventKind::TaskReceived,
        Some(&task.id),
        &TaskReceivedPayload {
            kind: kind_str,
            adapter: &task.adapter,
            depth: task.depth,
            input_preview: task.input_text.chars().take(200).collect(),
        },
    )?;
    Ok(())
}

/// Transition a task to `running` and emit `task_started`.
pub fn mark_running(store: &Store, session_id: &str, task_id: &str) -> Result<()> {
    let now = Utc::now().timestamp_millis();
    store.with_conn(|conn| {
        conn.execute(
            "UPDATE tasks SET state = 'running', started_at = ?1 WHERE id = ?2",
            rusqlite::params![now, task_id],
        )?;
        Ok(())
    })?;
    events::append_payload(
        store,
        session_id,
        EventKind::TaskStarted,
        Some(task_id),
        &TaskStartedPayload { task_id },
    )?;
    Ok(())
}

/// Transition a task to `done` with the final output, cost, and rounds.
/// Emits `task_done`.
pub fn mark_done(
    store: &Store,
    session_id: &str,
    task_id: &str,
    output: &str,
    cost_usd: f64,
    rounds: u32,
    stop_reason: &str,
) -> Result<()> {
    let now = Utc::now().timestamp_millis();
    store.with_conn(|conn| {
        conn.execute(
            "UPDATE tasks SET state = 'done', finished_at = ?1, output = ?2,
                              cost_usd = ?3, rounds = ?4
             WHERE id = ?5",
            rusqlite::params![now, output, cost_usd, rounds, task_id],
        )?;
        Ok(())
    })?;
    events::append_payload(
        store,
        session_id,
        EventKind::TaskDone,
        Some(task_id),
        &TaskDonePayload {
            ok: true,
            cost_usd,
            rounds,
            stop_reason,
        },
    )?;
    Ok(())
}

/// Mark as failed with an error string. Emits `task_done { ok: false }`.
pub fn mark_failed(
    store: &Store,
    session_id: &str,
    task_id: &str,
    error: &str,
) -> Result<()> {
    let now = Utc::now().timestamp_millis();
    store.with_conn(|conn| {
        conn.execute(
            "UPDATE tasks SET state = 'failed', finished_at = ?1, error = ?2
             WHERE id = ?3",
            rusqlite::params![now, error, task_id],
        )?;
        Ok(())
    })?;
    events::append_payload(
        store,
        session_id,
        EventKind::TaskDone,
        Some(task_id),
        &TaskDonePayload {
            ok: false,
            cost_usd: 0.0,
            rounds: 0,
            stop_reason: "error",
        },
    )?;
    Ok(())
}

/// Mark as cancelled. Emits `task_cancelled`.
pub fn mark_cancelled(
    store: &Store,
    session_id: &str,
    task_id: &str,
    reason: &str,
) -> Result<()> {
    let now = Utc::now().timestamp_millis();
    store.with_conn(|conn| {
        conn.execute(
            "UPDATE tasks SET state = 'cancelled', finished_at = ?1, error = ?2
             WHERE id = ?3",
            rusqlite::params![now, reason, task_id],
        )?;
        Ok(())
    })?;
    events::append_payload(
        store,
        session_id,
        EventKind::TaskCancelled,
        Some(task_id),
        &TaskCancelledPayload { reason },
    )?;
    Ok(())
}

/// On boot, any task stuck in `running` is from a prior crash.
/// Mark them `failed` with a clear error. Returns count cleaned up.
pub fn recover_crashed_tasks(store: &Store) -> Result<u32> {
    let now = Utc::now().timestamp_millis();
    store.with_conn(|conn| {
        let count = conn.execute(
            "UPDATE tasks SET state = 'failed', finished_at = ?1,
                              error = 'crash recovery: task was running at process exit'
             WHERE state = 'running'",
            rusqlite::params![now],
        )?;
        Ok(count as u32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn task_from_owner_has_default_fields() {
        let t = Task::from_owner("hello", "cli");
        assert_eq!(t.state, TaskState::Pending);
        assert_eq!(t.depth, 0);
        assert_eq!(t.kind, TaskKind::User);
        assert_eq!(t.adapter, "cli");
        assert_eq!(t.input_text, "hello");
        assert!(t.parent_id.is_none());
    }

    #[test]
    fn child_task_inherits_depth_plus_one() {
        let parent = Task::from_owner("parent", "cli");
        let child = Task::as_child_of(&parent, "child", TaskKind::Scheduled);
        assert_eq!(child.depth, 1);
        assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(child.adapter, "cli");
        assert_eq!(child.kind, TaskKind::Scheduled);
    }

    #[test]
    fn record_pending_inserts_row_and_emits_event() {
        let store = open_store();
        let task = Task::from_owner("do a thing", "cli");
        record_pending(&store, "s1", &task).unwrap();

        // tasks row present with state=pending
        let state: String = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT state FROM tasks WHERE id = ?1",
                    rusqlite::params![task.id],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        assert_eq!(state, "pending");

        assert_eq!(
            events::count_by_kind(&store, "s1", EventKind::TaskReceived).unwrap(),
            1
        );
    }

    #[test]
    fn state_transitions_pending_to_running_to_done() {
        let store = open_store();
        let task = Task::from_owner("work", "cli");
        record_pending(&store, "s1", &task).unwrap();

        mark_running(&store, "s1", &task.id).unwrap();
        let state: String = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT state FROM tasks WHERE id = ?1",
                    rusqlite::params![task.id],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        assert_eq!(state, "running");

        mark_done(&store, "s1", &task.id, "final answer", 0.01, 3, "content_only").unwrap();
        let (state, output, cost, rounds): (String, Option<String>, f64, u32) = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT state, output, cost_usd, rounds FROM tasks WHERE id = ?1",
                    rusqlite::params![task.id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, f64>(2)?,
                            row.get::<_, u32>(3)?,
                        ))
                    },
                )?)
            })
            .unwrap();
        assert_eq!(state, "done");
        assert_eq!(output.as_deref(), Some("final answer"));
        assert!((cost - 0.01).abs() < 1e-9);
        assert_eq!(rounds, 3);

        // task_received + task_started + task_done = 3 events
        assert_eq!(
            events::count_by_kind(&store, "s1", EventKind::TaskReceived).unwrap(),
            1
        );
        assert_eq!(
            events::count_by_kind(&store, "s1", EventKind::TaskStarted).unwrap(),
            1
        );
        assert_eq!(
            events::count_by_kind(&store, "s1", EventKind::TaskDone).unwrap(),
            1
        );
    }

    #[test]
    fn mark_failed_records_error() {
        let store = open_store();
        let task = Task::from_owner("x", "cli");
        record_pending(&store, "s1", &task).unwrap();
        mark_running(&store, "s1", &task.id).unwrap();
        mark_failed(&store, "s1", &task.id, "something broke").unwrap();

        let (state, err): (String, Option<String>) = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT state, error FROM tasks WHERE id = ?1",
                    rusqlite::params![task.id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(state, "failed");
        assert_eq!(err.as_deref(), Some("something broke"));
    }

    #[test]
    fn mark_cancelled_emits_cancel_event() {
        let store = open_store();
        let task = Task::from_owner("x", "cli");
        record_pending(&store, "s1", &task).unwrap();
        mark_cancelled(&store, "s1", &task.id, "owner said stop").unwrap();

        assert_eq!(
            events::count_by_kind(&store, "s1", EventKind::TaskCancelled).unwrap(),
            1
        );
    }

    #[test]
    fn recover_crashed_tasks_flips_running_to_failed() {
        let store = open_store();
        let t1 = Task::from_owner("a", "cli");
        let t2 = Task::from_owner("b", "cli");
        record_pending(&store, "s1", &t1).unwrap();
        record_pending(&store, "s1", &t2).unwrap();
        mark_running(&store, "s1", &t1.id).unwrap();
        mark_running(&store, "s1", &t2.id).unwrap();

        let n = recover_crashed_tasks(&store).unwrap();
        assert_eq!(n, 2);

        let state: String = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT state FROM tasks WHERE id = ?1",
                    rusqlite::params![t1.id],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .unwrap();
        assert_eq!(state, "failed");
    }

    #[test]
    fn task_state_round_trip_string_form() {
        for s in ["pending", "running", "done", "failed", "cancelled"] {
            let parsed = TaskState::parse(s).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!(TaskState::parse("nonsense").is_none());
    }
}
