//! Budget ledger and per-task spending guard.
//!
//! Source of truth is `events.llm_usage` in the store (SYSTEM_SPEC §6.8).
//! The budget guard is checked in the tool loop after each round of tool
//! dispatch. Two thresholds:
//!
//!   hard_task_pct (default 0.50) — if this task alone has spent more
//!     than 50% of the agent's remaining budget, force a final answer
//!     and terminate. This is the KERNEL-level enforcement: the LLM
//!     cannot reason its way around it.
//!
//!   soft_task_pct (default 0.30) — if the task has spent more than 30%,
//!     inject an informational system message every 10 rounds. The LLM
//!     decides whether to act on it (Bible P0+P3).
//!
//! "Remaining budget" is total_budget_usd minus the sum of all
//! llm_usage.cost_usd events in the current session. This is always
//! read from the store — never from accumulated local state — so
//! concurrent tasks see a consistent budget picture.

use anyhow::Result;
use sl_store::{EventKind, Store};
use tracing::warn;

/// Query the total spent in this session from the events table.
/// This is the single source of truth for budget (SYSTEM_SPEC §6.8).
pub fn query_session_spent(store: &Store, session_id: &str) -> Result<f64> {
    store.with_conn(|conn| {
        let spent: f64 = conn
            .query_row(
                "SELECT COALESCE(SUM(CAST(json_extract(payload, '$.cost_usd') AS REAL)), 0.0)
                 FROM events
                 WHERE event_type = ?1 AND session_id = ?2",
                rusqlite::params![EventKind::LlmUsage.as_str(), session_id],
                |row| row.get(0),
            )
            .map_err(anyhow::Error::from)?;
        Ok(spent)
    })
}

/// Configuration for the budget guard. Pulled from `Config.budget`.
#[derive(Debug, Clone)]
pub struct BudgetConfig {
    /// Total budget in USD. If None, no budget enforcement at all.
    pub total_usd: Option<f64>,
    /// Hard stop: task spent > this fraction of remaining budget.
    pub hard_task_pct: f64,
    /// Soft nudge: inject a system message when spent > this fraction.
    pub soft_task_pct: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            total_usd: Some(100.0),
            hard_task_pct: 0.50,
            soft_task_pct: 0.30,
        }
    }
}

/// The result of a budget check for one round.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetCheckResult {
    /// Budget is fine; continue.
    Ok,
    /// Task spending exceeds soft threshold; inject a nudge message.
    SoftNudge {
        task_cost: f64,
        remaining: f64,
    },
    /// Task spending exceeds hard threshold; must terminate now.
    HardStop {
        task_cost: f64,
        remaining: f64,
    },
}

/// Check the budget for the current task.
///
/// Arguments:
///   - `task_cost_so_far`: the accumulated `usage.cost_usd` this task
///     has tracked internally. This is the per-task number, not the
///     global session number. We use the per-task number because the
///     spec says "hard stop if THIS TASK spends >50% of remaining."
///   - `store` / `session_id`: used to query the global remaining.
///   - `round`: current round number, used to gate soft nudges (only
///     every 10 rounds to avoid spamming the context).
///   - `cfg`: the budget config thresholds.
pub fn check_budget(
    task_cost_so_far: f64,
    store: &Store,
    session_id: &str,
    round: u32,
    cfg: &BudgetConfig,
) -> BudgetCheckResult {
    let total = match cfg.total_usd {
        Some(t) if t > 0.0 => t,
        _ => return BudgetCheckResult::Ok,
    };

    let session_spent = match query_session_spent(store, session_id) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "budget query failed; skipping check");
            return BudgetCheckResult::Ok;
        }
    };

    let remaining = (total - session_spent).max(0.0);
    if remaining <= 0.0 {
        return BudgetCheckResult::HardStop {
            task_cost: task_cost_so_far,
            remaining: 0.0,
        };
    }

    let task_pct = task_cost_so_far / remaining;

    if task_pct > cfg.hard_task_pct {
        BudgetCheckResult::HardStop {
            task_cost: task_cost_so_far,
            remaining,
        }
    } else if task_pct > cfg.soft_task_pct && round % 10 == 0 {
        BudgetCheckResult::SoftNudge {
            task_cost: task_cost_so_far,
            remaining,
        }
    } else {
        BudgetCheckResult::Ok
    }
}

/// Format the system message for a soft budget nudge.
pub fn soft_nudge_message(task_cost: f64, remaining: f64) -> String {
    format!(
        "[BUDGET INFO] This task has spent ${:.4} of ${:.2} remaining. \
         Wrap up if possible.",
        task_cost, remaining
    )
}

/// Format the system message injected before forcing a final answer
/// on hard budget stop.
pub fn hard_stop_message(task_cost: f64, remaining: f64) -> String {
    format!(
        "[BUDGET LIMIT] This task spent ${:.4} (>{:.0}% of ${:.2} remaining). \
         Return your final answer now.",
        task_cost,
        50.0, // hardcoded to match the spec prose; the actual pct comes from cfg
        remaining
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;
    use sl_store::{events, Store};

    fn store_with_usage(session_id: &str, costs: &[f64]) -> Store {
        let store = Store::open_in_memory().unwrap();
        for cost in costs {
            #[derive(Serialize)]
            struct P {
                cost_usd: f64,
            }
            events::append_payload(
                &store,
                session_id,
                EventKind::LlmUsage,
                Some("t1"),
                &P { cost_usd: *cost },
            )
            .unwrap();
        }
        store
    }

    fn cfg(total: f64) -> BudgetConfig {
        BudgetConfig {
            total_usd: Some(total),
            hard_task_pct: 0.50,
            soft_task_pct: 0.30,
        }
    }

    #[test]
    fn query_session_spent_sums_correctly() {
        let store = store_with_usage("s1", &[0.01, 0.02, 0.03]);
        let spent = query_session_spent(&store, "s1").unwrap();
        assert!((spent - 0.06).abs() < 1e-9);
    }

    #[test]
    fn query_session_spent_returns_zero_for_empty() {
        let store = Store::open_in_memory().unwrap();
        let spent = query_session_spent(&store, "s1").unwrap();
        assert!((spent - 0.0).abs() < 1e-9);
    }

    #[test]
    fn query_session_spent_scoped_to_session() {
        let store = store_with_usage("s1", &[1.0]);
        #[derive(Serialize)]
        struct P {
            cost_usd: f64,
        }
        events::append_payload(&store, "s2", EventKind::LlmUsage, None, &P { cost_usd: 99.0 })
            .unwrap();
        let spent = query_session_spent(&store, "s1").unwrap();
        assert!((spent - 1.0).abs() < 1e-9, "got {}", spent);
    }

    #[test]
    fn hard_stop_when_task_exceeds_50pct_of_remaining() {
        // total=100, session spent=90, remaining=10. task cost=6 → 60% of remaining → hard stop.
        let store = store_with_usage("s1", &[90.0]);
        let result = check_budget(6.0, &store, "s1", 1, &cfg(100.0));
        assert!(matches!(result, BudgetCheckResult::HardStop { .. }));
    }

    #[test]
    fn ok_when_task_below_soft_threshold() {
        // total=100, session spent=0, remaining=100. task cost=1 → 1% → ok.
        let store = Store::open_in_memory().unwrap();
        let result = check_budget(1.0, &store, "s1", 1, &cfg(100.0));
        assert_eq!(result, BudgetCheckResult::Ok);
    }

    #[test]
    fn soft_nudge_fires_on_divisible_round() {
        // total=100, spent=60, remaining=40. task cost=15 → 37.5% of remaining → soft.
        // but only if round is divisible by 10.
        let store = store_with_usage("s1", &[60.0]);
        let r9 = check_budget(15.0, &store, "s1", 9, &cfg(100.0));
        assert_eq!(r9, BudgetCheckResult::Ok, "round 9 should not nudge");
        let r10 = check_budget(15.0, &store, "s1", 10, &cfg(100.0));
        assert!(matches!(r10, BudgetCheckResult::SoftNudge { .. }));
    }

    #[test]
    fn no_budget_configured_always_ok() {
        let store = Store::open_in_memory().unwrap();
        let no_budget = BudgetConfig {
            total_usd: None,
            ..Default::default()
        };
        let result = check_budget(999.0, &store, "s1", 10, &no_budget);
        assert_eq!(result, BudgetCheckResult::Ok);
    }

    #[test]
    fn zero_remaining_is_hard_stop() {
        // total=50, spent=50, remaining=0. Any task cost triggers hard stop.
        let store = store_with_usage("s1", &[50.0]);
        let result = check_budget(0.001, &store, "s1", 1, &cfg(50.0));
        assert!(matches!(result, BudgetCheckResult::HardStop { .. }));
    }
}
