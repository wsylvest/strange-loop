//! Runtime — the top-level wiring for a running strange-loop instance.
//!
//! In M0 this is minimal: it opens the store, generates a session id,
//! records a `session_started` event, and exposes methods for the binary
//! to drive the `self-test` subcommand. It grows in M1 to own the LLM
//! client, the tool dispatcher, and the scheduler.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

use sl_store::{events, EventKind, Store};

use crate::config::{CellBackend, Config};
use crate::governance;

/// A session id is a UUID generated at every process boot. It groups
/// events across one run and is the scope for "current budget" queries.
#[derive(Debug, Clone)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// The runtime handle.
pub struct Runtime {
    pub config: Config,
    pub store: Store,
    pub session_id: SessionId,
    pub cell_backend: CellBackend,
}

#[derive(Debug, Serialize)]
struct SessionStartedPayload<'a> {
    version: &'a str,
    cell_backend: &'a str,
    max_concurrent_tasks: u32,
    owner_id: &'a str,
}

impl Runtime {
    /// Open the runtime: load store, generate session, log session_started,
    /// run charter integrity check if the charter file is present.
    pub fn open(config: Config) -> Result<Self> {
        let data_dir = config.agent.data_dir.clone();
        let db_path = data_dir.join("strange-loop.db");
        let store = Store::open(&db_path)
            .with_context(|| format!("opening store at {:?}", db_path))?;

        let session_id = SessionId::new();
        let cell_backend = config.resolved_cell_backend();

        // session_started event
        let payload = SessionStartedPayload {
            version: env!("CARGO_PKG_VERSION"),
            cell_backend: cell_backend.as_str(),
            max_concurrent_tasks: config.tool_loop.max_concurrent_tasks,
            owner_id: &config.agent.owner_id,
        };
        events::append_payload(
            &store,
            session_id.as_str(),
            EventKind::SessionStarted,
            None,
            &payload,
        )
        .context("recording session_started event")?;

        // Charter integrity — non-fatal in M0 if the charter file is
        // missing, because tests and self-test use temp dirs that may
        // not have prompts/. A future boot path will refuse to run
        // without a present charter.
        let charter_path = config.governance.charter.clone();
        let full_charter_path = resolve_under_repo(&config.agent.repo_root, &charter_path);
        if full_charter_path.exists() {
            match governance::check_charter(&store, &full_charter_path)? {
                governance::CharterCheck::FirstBoot { hash } => {
                    tracing::info!(hash = %hash, "charter baseline recorded");
                }
                governance::CharterCheck::Match { .. } => {
                    tracing::debug!("charter hash verified");
                }
                governance::CharterCheck::Drift { on_disk, on_record } => {
                    anyhow::bail!(
                        "CHARTER drift detected.\n  on-disk:   {}\n  on-record: {}\n\n\
                         Run `strange-loop charter approve --hash {}` to acknowledge \
                         the new charter, or restore the file from git.",
                        on_disk,
                        on_record,
                        on_disk
                    );
                }
            }
        } else {
            tracing::warn!(
                path = %full_charter_path.display(),
                "charter file not found; skipping integrity check (M0 tolerance)",
            );
        }

        Ok(Self {
            config,
            store,
            session_id,
            cell_backend,
        })
    }

    /// Run the self-test pipeline: record one health_invariant event of
    /// level "info" and verify the count increments. Returns Ok(()) on
    /// success. Invoked by the `strange-loop self-test` subcommand.
    pub fn self_test(&self) -> Result<SelfTestReport> {
        tracing::info!(session = %self.session_id.as_str(), "running self-test");

        #[derive(Serialize)]
        struct Payload<'a> {
            level: &'a str,
            name: &'a str,
            detail: &'a str,
        }

        let before = events::count_by_kind(
            &self.store,
            self.session_id.as_str(),
            EventKind::HealthInvariant,
        )?;

        events::append_payload(
            &self.store,
            self.session_id.as_str(),
            EventKind::HealthInvariant,
            None,
            &Payload {
                level: "info",
                name: "self_test",
                detail: "runtime open, store writable, events append ok",
            },
        )?;

        let after = events::count_by_kind(
            &self.store,
            self.session_id.as_str(),
            EventKind::HealthInvariant,
        )?;

        if after != before + 1 {
            anyhow::bail!(
                "self_test: event count did not increment as expected ({} -> {})",
                before,
                after
            );
        }

        // And verify the session_started event is there.
        let session_started = events::count_by_kind(
            &self.store,
            self.session_id.as_str(),
            EventKind::SessionStarted,
        )?;
        if session_started < 1 {
            anyhow::bail!("self_test: no session_started event found");
        }

        let db_path = self.store.db_path().to_path_buf();
        Ok(SelfTestReport {
            ok: true,
            session_id: self.session_id.as_str().to_string(),
            cell_backend: self.cell_backend.as_str().to_string(),
            db_path,
            events_written: after + 1, // +1 for session_started
        })
    }
}

/// Structured self-test output. Printed as JSON by the binary so it can
/// be consumed by CI and by future test harnesses.
#[derive(Debug, Serialize)]
pub struct SelfTestReport {
    pub ok: bool,
    pub session_id: String,
    pub cell_backend: String,
    pub db_path: PathBuf,
    pub events_written: i64,
}

/// Resolve a path under repo_root if it's relative; pass through if absolute.
fn resolve_under_repo(repo_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir_shim::TempDir;

    #[test]
    fn runtime_opens_and_runs_self_test() {
        let tmp = TempDir::new();
        let mut config = Config::default();
        config.agent.data_dir = tmp.path().join("data");
        // point charter at a nonexistent file — M0 tolerates this
        config.governance.charter = PathBuf::from("/nonexistent/CHARTER.md");

        let runtime = Runtime::open(config).expect("open runtime");
        let report = runtime.self_test().expect("self test");
        assert!(report.ok);
        assert!(report.events_written >= 2);
    }

    #[test]
    fn session_id_is_unique() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a.as_str(), b.as_str());
    }

    mod tempdir_shim {
        use std::path::{Path, PathBuf};

        pub struct TempDir {
            path: PathBuf,
        }

        impl TempDir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicU64, Ordering};
                use std::time::{SystemTime, UNIX_EPOCH};
                static COUNTER: AtomicU64 = AtomicU64::new(0);
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let n = COUNTER.fetch_add(1, Ordering::SeqCst);
                let path = std::env::temp_dir().join(format!(
                    "strange-loop-test-{}-{:x}-{:?}-{}",
                    std::process::id(),
                    nanos,
                    std::thread::current().id(),
                    n
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self { path }
            }

            pub fn path(&self) -> &Path {
                &self.path
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }
}
