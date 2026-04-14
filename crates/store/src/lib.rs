//! strange-loop store — SQLite-backed durable state.
//!
//! This crate owns the database schema and all read/write paths that
//! touch it. See `docs/SYSTEM_SPEC.md` §4 for the persistence tier
//! design and §4.6 for the schema.
//!
//! The store is the Events tier in the soul/events/ephemeral split.
//! Losing it degrades the agent gracefully — history is gone but
//! identity (soul tier, on disk and git-tracked) is untouched.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use tracing::{debug, warn};

pub mod events;
pub mod kv;

pub use events::{Event, EventKind};

/// The embedded schema. Applied on first boot and on every boot (idempotent).
const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Current schema version. Bumped when a migration is added.
const SCHEMA_VERSION: u32 = 1;

/// A handle to the strange-loop store.
///
/// Wraps a single SQLite connection guarded by a `Mutex`. SQLite WAL mode
/// gives us concurrent readers at the OS level, but rusqlite's own
/// `Connection` is `!Send` across threads without serialization, so we
/// serialize writes behind one mutex per store handle and clone the
/// `Arc` when we need to share across tasks. For v0.1's workload
/// (one process, single-digit concurrent tasks, event writes dominated
/// by LLM-call latency) this is more than fast enough.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Connection>>,
    db_path: PathBuf,
}

impl Store {
    /// Open (or create) the store at `path`. Runs migrations idempotently.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir {:?}", parent))?;
        }

        let conn = Connection::open(&path)
            .with_context(|| format!("opening sqlite db at {:?}", path))?;

        // WAL + sensible defaults. See docs/SYSTEM_SPEC §4.5.
        // NOTE: pragma statements that return rows must use query_row, not
        // execute_batch — otherwise rusqlite errors with ExecuteReturnedResults.
        let _: String = conn
            .query_row("PRAGMA journal_mode = WAL;", [], |row| row.get(0))
            .context("setting WAL journal mode")?;
        conn.execute_batch(
            r#"
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;
            PRAGMA temp_store = MEMORY;
            "#,
        )
        .context("setting sqlite pragmas")?;

        let store = Store {
            inner: Arc::new(Mutex::new(conn)),
            db_path: path,
        };

        store.apply_schema()?;
        store.integrity_check()?;
        debug!(path = %store.db_path.display(), "store opened");
        Ok(store)
    }

    /// Open an in-memory store. Used by tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory sqlite")?;
        let store = Store {
            inner: Arc::new(Mutex::new(conn)),
            db_path: PathBuf::from(":memory:"),
        };
        store.apply_schema()?;
        Ok(store)
    }

    /// Apply `schema.sql`. Idempotent — all statements use IF NOT EXISTS.
    fn apply_schema(&self) -> Result<()> {
        let conn = self.inner.lock().unwrap();
        conn.execute_batch(SCHEMA_SQL).context("applying schema")?;

        // Record schema version in kv for future migrations.
        conn.execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES ('schema_version', ?1);",
            [SCHEMA_VERSION.to_string()],
        )
        .context("writing schema_version kv row")?;
        Ok(())
    }

    /// Run PRAGMA integrity_check. Logs a warning on failure; does not panic
    /// because the correct response to corruption is handled by the caller
    /// (rename the file, boot fresh, preserve the soul tier).
    pub fn integrity_check(&self) -> Result<bool> {
        let conn = self.inner.lock().unwrap();
        let result: String = conn
            .query_row("PRAGMA integrity_check;", [], |row| row.get(0))
            .context("running integrity_check")?;
        if result == "ok" {
            Ok(true)
        } else {
            warn!(result = %result, "integrity_check reported problems");
            Ok(false)
        }
    }

    /// Return the path the store was opened at. Useful for backup subcommands.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Access the inner connection. Prefer the typed helpers (events, kv,
    /// etc.); raw access is here for crates in the workspace that need it.
    pub fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T>,
    {
        let conn = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("store mutex poisoned"))?;
        f(&conn)
    }
}

/// Small helper type used across the store API for structured payloads
/// that will end up as JSON in the events table.
pub fn to_json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).context("serializing to json")
}

/// Version of the schema this build was compiled against.
pub fn schema_version() -> u32 {
    SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_applies_schema() {
        let store = Store::open_in_memory().expect("open");
        let ver: String = store
            .with_conn(|c| {
                Ok(c.query_row(
                    "SELECT value FROM kv WHERE key = 'schema_version'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(ver, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn integrity_check_passes_on_fresh_store() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.integrity_check().unwrap());
    }

    #[test]
    fn journal_is_append_only_update_refused() {
        let store = Store::open_in_memory().unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO journal (ts, session_id, text, tags) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![1_700_000_000_000_i64, "s1", "first entry", "[]"],
                )?;
                Ok(())
            })
            .unwrap();

        let err = store
            .with_conn(|c| {
                c.execute(
                    "UPDATE journal SET text = 'edited' WHERE id = 1",
                    [],
                )?;
                Ok(())
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("journal is append-only"),
            "expected append-only error, got: {}",
            err
        );
    }

    #[test]
    fn journal_is_append_only_delete_refused() {
        let store = Store::open_in_memory().unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO journal (ts, session_id, text, tags) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![1_700_000_000_000_i64, "s1", "first entry", "[]"],
                )?;
                Ok(())
            })
            .unwrap();
        let err = store
            .with_conn(|c| {
                c.execute("DELETE FROM journal WHERE id = 1", [])?;
                Ok(())
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("journal is append-only"),
            "expected append-only error, got: {}",
            err
        );
    }
}

