//! Context builder — assembles the system prompt for one task.
//!
//! The system prompt is built as a single system message with three
//! `ContentBlock`s. Each block has a different `cache_control` hint:
//!
//!   Block 1 (static, 1h TTL): CHARTER.md
//!   Block 2 (semi-stable, ephemeral): CREED.md + DOCTRINE + KB index
//!   Block 3 (dynamic, uncached): kv snapshot, runtime, journal tail,
//!     scratch, recent messages, recent events summary, health invariants
//!
//! On Anthropic via OpenRouter, only blocks 1 and 2 are cached. Block 3
//! is recomputed every round. This is the single largest cost
//! optimization in the system — ~100k cached tokens at $0.30 vs $3.
//!
//! A soft token cap prunes block 3 in priority order when the
//! assembled prompt exceeds the configured limit. Blocks 1 and 2
//! are never pruned.
//!
//! See `docs/SYSTEM_SPEC.md` §6.5.

use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use sl_llm::{CacheControl, ContentBlock, Message};
use sl_store::Store;
use tracing::debug;

use crate::budget;
use crate::config::Config;

/// What kind of task is being served. Affects which optional sections
/// are included in block 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    User,
    Review,
    Evolution,
    Scheduled,
    Consciousness,
}

impl TaskKind {
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "review" => Self::Review,
            "evolution" => Self::Evolution,
            "scheduled" => Self::Scheduled,
            "consciousness" => Self::Consciousness,
            _ => Self::User,
        }
    }
}

/// The assembled context, ready to feed to the tool loop.
pub struct BuiltContext {
    /// The system message with three content blocks.
    pub system_message: Message,
    /// Estimated token count before any pruning.
    pub estimated_tokens_before: usize,
    /// Estimated token count after pruning (if any).
    pub estimated_tokens_after: usize,
    /// Which sections were pruned, if any.
    pub pruned_sections: Vec<String>,
}

/// Build the full LLM context for a task.
pub fn build_context(
    config: &Config,
    store: &Store,
    session_id: &str,
    task_id: &str,
    task_kind: TaskKind,
    soft_cap_tokens: usize,
) -> Result<BuiltContext> {
    let repo = &config.agent.repo_root;

    // --- Block 1: static (cached 1h) ---
    let charter_text = read_soul_file(repo, &config.governance.charter)
        .unwrap_or_else(|_| "(CHARTER not found)".into());

    let block1_text = format!(
        "# CHARTER\n\n{charter_text}",
    );

    // --- Block 2: semi-stable (cached ephemeral) ---
    let creed_text = read_soul_file(repo, &config.governance.creed)
        .unwrap_or_else(|_| "(CREED not found)".into());

    let doctrine_text = build_doctrine_section(repo, &config.governance.doctrine);

    let kb_index = build_knowledge_index(store);

    let block2_text = format!(
        "# CREED\n\n{creed_text}\n\n# DOCTRINE\n\n{doctrine_text}\n\n\
         # Knowledge base\n\n{kb_index}"
    );

    // --- Block 3: dynamic (uncached) ---
    let mut dynamic_sections: Vec<(&str, String)> = Vec::new();

    // kv snapshot
    let kv_snapshot = build_kv_snapshot(store, session_id, config);
    dynamic_sections.push(("kv_snapshot", format!("## Runtime state\n\n{kv_snapshot}")));

    // Runtime context
    let runtime = build_runtime_section(config, task_id, task_kind);
    dynamic_sections.push(("runtime", format!("## Runtime context\n\n{runtime}")));

    // Journal tail
    let journal = build_journal_tail(store, session_id, task_kind);
    if !journal.is_empty() {
        dynamic_sections.push(("journal", format!("## Journal (recent)\n\n{journal}")));
    }

    // Scratch
    let scratch = read_soul_file(repo, &config.governance.scratch).unwrap_or_default();
    if !scratch.trim().is_empty() {
        dynamic_sections.push(("scratch", format!("## Scratch\n\n{scratch}")));
    }

    // Recent messages
    let messages = build_recent_messages(store, 20);
    if !messages.is_empty() {
        dynamic_sections.push(("recent_messages", format!("## Recent messages\n\n{messages}")));
    }

    // Recent events summary
    let events_summary = build_events_summary(store, session_id);
    if !events_summary.is_empty() {
        dynamic_sections.push(("recent_events", format!("## Recent events\n\n{events_summary}")));
    }

    // Health invariants
    let health = build_health_invariants(store, config, session_id);
    if !health.is_empty() {
        dynamic_sections.push(("health", format!("## Health invariants\n\n{health}")));
    }

    let block3_text: String = dynamic_sections.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join("\n\n");

    // --- Token estimation and pruning ---
    let estimate = |s: &str| -> usize { s.len() / 4 }; // rough: 1 token ≈ 4 chars
    let total_before = estimate(&block1_text) + estimate(&block2_text) + estimate(&block3_text);

    let mut pruned: Vec<String> = Vec::new();
    let mut current_dynamic = dynamic_sections.clone();

    // Pruning priority: recent_events → recent_messages → journal → scratch → runtime
    let prune_order = ["recent_events", "recent_messages", "journal", "scratch"];

    if soft_cap_tokens > 0 {
        let static_tokens = estimate(&block1_text) + estimate(&block2_text);
        let mut dynamic_tokens = estimate(&block3_text);

        for key in &prune_order {
            if static_tokens + dynamic_tokens <= soft_cap_tokens {
                break;
            }
            if let Some(pos) = current_dynamic.iter().position(|(k, _)| k == key) {
                let removed = current_dynamic.remove(pos);
                dynamic_tokens -= estimate(&removed.1);
                pruned.push(key.to_string());
            }
        }
    }

    let final_block3: String = current_dynamic
        .iter()
        .map(|(_, s)| s.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let total_after = estimate(&block1_text) + estimate(&block2_text) + estimate(&final_block3);

    if !pruned.is_empty() {
        debug!(
            pruned = ?pruned,
            before = total_before,
            after = total_after,
            "context builder pruned sections to fit soft cap"
        );
    }

    // --- Assemble the system message ---
    let system_message = Message {
        role: sl_llm::MessageRole::System,
        content: vec![
            ContentBlock::text_cached(
                block1_text,
                CacheControl::ephemeral_ttl("1h"),
            ),
            ContentBlock::text_cached(
                block2_text,
                CacheControl::ephemeral(),
            ),
            ContentBlock::text(final_block3),
        ],
        tool_calls: Vec::new(),
        tool_call_id: None,
    };

    Ok(BuiltContext {
        system_message,
        estimated_tokens_before: total_before,
        estimated_tokens_after: total_after,
        pruned_sections: pruned,
    })
}

// ---------------------------------------------------------------------------
// Section builders
// ---------------------------------------------------------------------------

fn read_soul_file(repo_root: &Path, rel_path: &Path) -> Result<String> {
    let full = if rel_path.is_absolute() {
        rel_path.to_path_buf()
    } else {
        repo_root.join(rel_path)
    };
    std::fs::read_to_string(&full)
        .map_err(|e| anyhow::anyhow!("reading soul file {:?}: {}", full, e))
}

fn build_doctrine_section(repo_root: &Path, doctrine_path: &Path) -> String {
    // Try the TOML first and render a summary; fall back to the markdown rendering.
    let full = if doctrine_path.is_absolute() {
        doctrine_path.to_path_buf()
    } else {
        repo_root.join(doctrine_path)
    };

    if let Ok(text) = std::fs::read_to_string(&full) {
        if full.extension().map(|e| e == "toml").unwrap_or(false) {
            return format!("(from doctrine.toml)\n\n```toml\n{text}\n```");
        }
        return text;
    }

    // Try the .md sibling
    let md = full.with_extension("md");
    if let Ok(text) = std::fs::read_to_string(&md) {
        return text;
    }

    "(DOCTRINE not found)".into()
}

fn build_knowledge_index(store: &Store) -> String {
    let result = store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT topic, COALESCE(summary, '') FROM knowledge ORDER BY topic",
        )?;
        let rows = stmt.query_map([], |row| {
            let topic: String = row.get(0)?;
            let summary: String = row.get(1)?;
            Ok((topic, summary))
        })?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    });

    match result {
        Ok(rows) if !rows.is_empty() => {
            let mut out = String::new();
            for (topic, summary) in &rows {
                if summary.is_empty() {
                    out.push_str(&format!("- **{topic}**\n"));
                } else {
                    out.push_str(&format!("- **{topic}**: {summary}\n"));
                }
            }
            out
        }
        _ => "(no topics yet)".into(),
    }
}

fn build_kv_snapshot(store: &Store, session_id: &str, config: &Config) -> String {
    let get = |k: &str| -> String {
        sl_store::kv::get(store, k)
            .ok()
            .flatten()
            .unwrap_or_else(|| "-".into())
    };

    let spent = budget::query_session_spent(store, session_id).unwrap_or(0.0);
    let remaining = (config.budget.total_usd - spent).max(0.0);

    let obj = json!({
        "version": get("version"),
        "session_id": session_id,
        "charter_hash": get("charter_hash"),
        "current_branch": get("current_branch"),
        "current_sha": get("current_sha"),
        "budget_total_usd": config.budget.total_usd,
        "budget_spent_usd": format!("{:.4}", spent),
        "budget_remaining_usd": format!("{:.2}", remaining),
    });

    serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".into())
}

fn build_runtime_section(config: &Config, task_id: &str, task_kind: TaskKind) -> String {
    let kind_str = match task_kind {
        TaskKind::User => "user",
        TaskKind::Review => "review",
        TaskKind::Evolution => "evolution",
        TaskKind::Scheduled => "scheduled",
        TaskKind::Consciousness => "consciousness",
    };

    let obj = json!({
        "utc_now": Utc::now().to_rfc3339(),
        "repo_root": config.agent.repo_root.display().to_string(),
        "data_dir": config.agent.data_dir.display().to_string(),
        "task_id": task_id,
        "task_kind": kind_str,
        "cell_backend": config.resolved_cell_backend().as_str(),
        "max_concurrent_tasks": config.tool_loop.max_concurrent_tasks,
    });

    serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".into())
}

fn build_journal_tail(store: &Store, _session_id: &str, task_kind: TaskKind) -> String {
    let limit: i64 = match task_kind {
        TaskKind::Review | TaskKind::Evolution => 20,
        TaskKind::Consciousness => 5,
        _ => 10,
    };

    let result = store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT ts, text FROM journal ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit], |row| {
            let ts: i64 = row.get(0)?;
            let text: String = row.get(1)?;
            Ok((ts, text))
        })?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    });

    match result {
        Ok(rows) if !rows.is_empty() => {
            let entries: Vec<String> = rows
                .into_iter()
                .rev() // oldest-first for narrative reading
                .map(|(ts, text)| {
                    let dt = chrono::DateTime::from_timestamp_millis(ts)
                        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| ts.to_string());
                    format!("[{dt}] {text}")
                })
                .collect();
            entries.join("\n\n")
        }
        _ => String::new(),
    }
}

fn build_recent_messages(store: &Store, limit: usize) -> String {
    let result = store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT ts, direction, adapter, content FROM messages ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            let ts: i64 = row.get(0)?;
            let dir: String = row.get(1)?;
            let adapter: String = row.get(2)?;
            let content: String = row.get(3)?;
            Ok((ts, dir, adapter, content))
        })?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    });

    match result {
        Ok(rows) if !rows.is_empty() => {
            let entries: Vec<String> = rows
                .into_iter()
                .rev() // oldest-first
                .map(|(ts, dir, adapter, content)| {
                    let dt = chrono::DateTime::from_timestamp_millis(ts)
                        .map(|d| d.format("%H:%M").to_string())
                        .unwrap_or_default();
                    let arrow = if dir == "in" { "→" } else { "←" };
                    let preview: String = content.chars().take(200).collect();
                    format!("[{dt}] {arrow} ({adapter}) {preview}")
                })
                .collect();
            entries.join("\n")
        }
        _ => String::new(),
    }
}

fn build_events_summary(store: &Store, session_id: &str) -> String {
    let thirty_min_ago = Utc::now().timestamp_millis() - 30 * 60 * 1000;

    let result = store.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT event_type, COUNT(*) FROM events
             WHERE session_id = ?1 AND ts >= ?2
             GROUP BY event_type ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id, thirty_min_ago], |row| {
            let et: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((et, count))
        })?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    });

    match result {
        Ok(rows) if !rows.is_empty() => {
            let entries: Vec<String> = rows
                .into_iter()
                .map(|(et, count)| format!("- {et}: {count}"))
                .collect();
            format!("Last 30 minutes:\n{}", entries.join("\n"))
        }
        _ => String::new(),
    }
}

fn build_health_invariants(store: &Store, config: &Config, session_id: &str) -> String {
    let mut checks: Vec<String> = Vec::new();

    // 1. Version sync: VERSION file vs config
    let version_file = read_soul_file(&config.agent.repo_root, Path::new("VERSION"))
        .map(|s| s.trim().to_string());
    match version_file {
        Ok(v) if !v.is_empty() => {
            checks.push(format!("- OK: version {v}"));
        }
        _ => {
            checks.push("- WARNING: VERSION file missing or empty".into());
        }
    }

    // 2. Budget drift (placeholder — real drift check queries OpenRouter in M5)
    let spent = budget::query_session_spent(store, session_id).unwrap_or(0.0);
    if spent > 0.0 {
        checks.push(format!(
            "- OK: session spent ${:.4} (drift check not yet implemented)",
            spent
        ));
    }

    // 3. Charter hash
    let charter_hash = sl_store::kv::get(store, "charter_hash")
        .ok()
        .flatten();
    match charter_hash {
        Some(h) => checks.push(format!("- OK: charter pinned ({}…)", &h[..8.min(h.len())])),
        None => checks.push("- WARNING: no charter baseline recorded".into()),
    }

    // 4. Stale journal
    let last_journal = store.with_conn(|conn| {
        conn.query_row(
            "SELECT MAX(ts) FROM journal",
            [],
            |row| row.get::<_, Option<i64>>(0),
        ).map_err(anyhow::Error::from)
    });
    match last_journal {
        Ok(Some(ts)) => {
            let age_hours = (Utc::now().timestamp_millis() - ts) as f64 / 3_600_000.0;
            if age_hours > 8.0 {
                checks.push(format!(
                    "- WARNING: STALE JOURNAL — last entry {:.0}h ago",
                    age_hours
                ));
            } else {
                checks.push("- OK: journal recent".into());
            }
        }
        _ => checks.push("- INFO: journal empty".into()),
    }

    checks.join("\n")
}

// ---------------------------------------------------------------------------
// Token estimation (rough)
// ---------------------------------------------------------------------------

/// Rough token estimate: 1 token ≈ 4 characters. This is intentionally
/// crude — it is used only for the soft-cap pruning decision, not for
/// cost calculation (which uses the real token counts from the provider).
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use sl_store::Store;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_repo() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "sl-ctx-test-{}-{:x}-{:?}-{}",
            std::process::id(),
            nanos,
            std::thread::current().id(),
            n
        ));
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::create_dir_all(root.join("journal")).unwrap();
        std::fs::write(root.join("VERSION"), "0.0.0\n").unwrap();
        std::fs::write(root.join("prompts/CHARTER.md"), "You are strange-loop.\n").unwrap();
        std::fs::write(root.join("prompts/CREED.md"), "Be honest.\n").unwrap();
        std::fs::write(
            root.join("prompts/doctrine.toml"),
            "[repo]\ndev_branch = \"agent\"\n",
        )
        .unwrap();
        std::fs::write(root.join("prompts/scratch.md"), "working on tests\n").unwrap();
        root
    }

    fn cfg_for(repo: &Path) -> Config {
        let mut cfg = Config::default();
        cfg.agent.repo_root = repo.to_path_buf();
        cfg.agent.data_dir = repo.join("data");
        cfg
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn builds_three_blocks() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 0)
            .unwrap();

        // Three content blocks on the system message
        assert_eq!(ctx.system_message.content.len(), 3);

        // Block 1 contains charter
        let b1 = &ctx.system_message.content[0];
        match b1 {
            ContentBlock::Text { text, cache_control } => {
                assert!(text.contains("You are strange-loop"), "block 1 should have charter");
                let cc = cache_control.as_ref().expect("block 1 should have cache_control");
                assert_eq!(cc.ttl.as_deref(), Some("1h"));
            }
            _ => panic!("block 1 should be Text"),
        }

        // Block 2 contains creed + doctrine + kb
        let b2 = &ctx.system_message.content[1];
        match b2 {
            ContentBlock::Text { text, cache_control } => {
                assert!(text.contains("Be honest"), "block 2 should have creed");
                assert!(text.contains("doctrine.toml"), "block 2 should have doctrine");
                assert!(text.contains("Knowledge base"), "block 2 should have kb index");
                assert!(cache_control.is_some(), "block 2 should have ephemeral cache");
            }
            _ => panic!("block 2 should be Text"),
        }

        // Block 3 contains runtime and scratch
        let b3 = &ctx.system_message.content[2];
        match b3 {
            ContentBlock::Text { text, cache_control } => {
                assert!(text.contains("Runtime state"), "block 3 should have kv");
                assert!(text.contains("working on tests"), "block 3 should have scratch");
                assert!(cache_control.is_none(), "block 3 should NOT have cache_control");
            }
            _ => panic!("block 3 should be Text"),
        }

        cleanup(&repo);
    }

    #[test]
    fn includes_journal_when_present() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        // Insert a journal entry
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO journal (ts, session_id, text, tags) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        chrono::Utc::now().timestamp_millis(),
                        "s1",
                        "first entry",
                        "[]"
                    ],
                )?;
                Ok(())
            })
            .unwrap();

        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 0).unwrap();
        let text = ctx.system_message.text_concat();
        assert!(text.contains("first entry"), "journal entry should appear");

        cleanup(&repo);
    }

    #[test]
    fn includes_knowledge_index() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        // Insert a knowledge topic
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO knowledge (topic, content, updated_at, summary)
                     VALUES ('rust-tips', 'use iterators', ?1, 'Rust tips and tricks')",
                    rusqlite::params![chrono::Utc::now().timestamp_millis()],
                )?;
                Ok(())
            })
            .unwrap();

        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 0).unwrap();
        let text = ctx.system_message.text_concat();
        assert!(text.contains("rust-tips"), "knowledge topic should appear in index");
        assert!(text.contains("Rust tips and tricks"), "summary should appear");

        cleanup(&repo);
    }

    #[test]
    fn includes_health_invariants() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 0).unwrap();
        let text = ctx.system_message.text_concat();
        assert!(text.contains("Health invariants"), "health section should appear");
        assert!(text.contains("version 0.0.0"), "version check should appear");
        // No charter baseline yet → warning
        assert!(
            text.contains("no charter baseline"),
            "missing charter baseline should be flagged"
        );

        cleanup(&repo);
    }

    #[test]
    fn pruning_removes_sections_in_priority_order() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        // Insert some messages to create the "recent_messages" section
        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO messages (ts, direction, adapter, content)
                     VALUES (?1, 'in', 'cli', 'hello from owner')",
                    rusqlite::params![chrono::Utc::now().timestamp_millis()],
                )?;
                Ok(())
            })
            .unwrap();

        // Insert an event to create the "recent_events" section
        sl_store::events::append_payload(
            &store,
            "s1",
            sl_store::EventKind::SessionStarted,
            None,
            &serde_json::json!({"note": "test"}),
        )
        .unwrap();

        // Build with an absurdly small token cap to force pruning
        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 50).unwrap();

        // recent_events should be pruned first, then recent_messages
        assert!(
            ctx.pruned_sections.contains(&"recent_events".to_string()),
            "recent_events should be pruned first; pruned: {:?}",
            ctx.pruned_sections
        );

        assert!(
            ctx.estimated_tokens_after <= ctx.estimated_tokens_before,
            "pruning should reduce tokens"
        );

        cleanup(&repo);
    }

    #[test]
    fn no_pruning_when_under_cap() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 1_000_000).unwrap();
        assert!(ctx.pruned_sections.is_empty(), "nothing should be pruned with a huge cap");

        cleanup(&repo);
    }

    #[test]
    fn no_pruning_when_cap_is_zero() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        // soft_cap_tokens=0 means "no cap" per the spec
        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 0).unwrap();
        assert!(ctx.pruned_sections.is_empty());

        cleanup(&repo);
    }

    #[test]
    fn budget_shows_in_kv_snapshot() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        // Seed some spending
        sl_store::events::append_payload(
            &store,
            "s1",
            sl_store::EventKind::LlmUsage,
            Some("t0"),
            &serde_json::json!({"cost_usd": 5.0}),
        )
        .unwrap();

        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 0).unwrap();
        let text = ctx.system_message.text_concat();
        assert!(text.contains("5.0000"), "spent should show in kv snapshot");
        assert!(text.contains("95.00"), "remaining should show in kv snapshot");

        cleanup(&repo);
    }

    #[test]
    fn recent_messages_appear_in_dynamic_block() {
        let repo = tmp_repo();
        let store = Store::open_in_memory().unwrap();
        let cfg = cfg_for(&repo);

        store
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO messages (ts, direction, adapter, content)
                     VALUES (?1, 'in', 'cli', 'what is the plan?')",
                    rusqlite::params![chrono::Utc::now().timestamp_millis()],
                )?;
                Ok(())
            })
            .unwrap();

        let ctx = build_context(&cfg, &store, "s1", "t1", TaskKind::User, 0).unwrap();
        let text = ctx.system_message.text_concat();
        assert!(
            text.contains("what is the plan?"),
            "recent message should appear in block 3"
        );

        cleanup(&repo);
    }
}
