//! Small durable key-value store.
//!
//! Used for singleton runtime state: schema version, session id,
//! charter hash, current branch/sha, bg_enabled flag, etc. Read on
//! boot and on demand. Written on change only.

use anyhow::{Context, Result};
use rusqlite::params;

use crate::Store;

/// Get a value by key. Returns `None` if the key does not exist.
pub fn get(store: &Store, key: &str) -> Result<Option<String>> {
    store.with_conn(|conn| {
        match conn.query_row(
            "SELECT value FROM kv WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        ) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(anyhow::Error::from(e)).context("reading kv"),
        }
    })
}

/// Insert or replace a value.
pub fn set(store: &Store, key: &str, value: &str) -> Result<()> {
    store.with_conn(|conn| {
        conn.execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .context("writing kv")?;
        Ok(())
    })
}

/// Get or return a default; does NOT persist the default.
pub fn get_or(store: &Store, key: &str, default: &str) -> Result<String> {
    Ok(get(store, key)?.unwrap_or_else(|| default.to_string()))
}

/// Delete a key. No-op if missing.
pub fn delete(store: &Store, key: &str) -> Result<()> {
    store.with_conn(|conn| {
        conn.execute("DELETE FROM kv WHERE key = ?1", params![key])
            .context("deleting kv")?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_then_get() {
        let store = Store::open_in_memory().unwrap();
        set(&store, "owner_id", "bills").unwrap();
        assert_eq!(get(&store, "owner_id").unwrap().unwrap(), "bills");
    }

    #[test]
    fn get_missing_returns_none() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(get(&store, "nope").unwrap(), None);
    }

    #[test]
    fn get_or_returns_default_without_persisting() {
        let store = Store::open_in_memory().unwrap();
        let v = get_or(&store, "missing", "fallback").unwrap();
        assert_eq!(v, "fallback");
        // did not persist
        assert_eq!(get(&store, "missing").unwrap(), None);
    }

    #[test]
    fn delete_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        set(&store, "k", "v").unwrap();
        delete(&store, "k").unwrap();
        delete(&store, "k").unwrap(); // no panic
        assert_eq!(get(&store, "k").unwrap(), None);
    }
}
