//! The `Tool` trait and the types tools see.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use sl_llm::ToolSchema;
use sl_store::Store;

/// Isolation class for a tool. See SYSTEM_SPEC §3.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostClass {
    /// Runs in the parent's async runtime. Trusted.
    InProc,
    /// Runs in workerd. JSON-in/JSON-out, declared capabilities only.
    /// Implemented in M8; in M1 we error if a tool requires this class
    /// because the workerd supervisor doesn't exist yet.
    Edge,
    /// Runs in a Docker / Apple Containers / Firecracker microVM.
    /// Implemented in M2 (because the `proc` tool needs it before
    /// `restart` can do its preflight `cargo check`).
    Cell,
}

impl HostClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InProc => "in_proc",
            Self::Edge => "edge",
            Self::Cell => "cell",
        }
    }
}

/// Per-invocation context passed to a tool's `invoke`. Holds the
/// shared services a tool may need: the store handle, the agent's
/// repo root, and the data directory.
#[derive(Clone)]
pub struct ToolCtx {
    pub store: Store,
    pub repo_root: Arc<PathBuf>,
    pub data_dir: Arc<PathBuf>,
    /// The list of paths the agent's write/delete tools must refuse
    /// to touch. Resolved against `repo_root` for relative paths.
    pub protected_paths: Arc<Vec<PathBuf>>,
    /// The session id this tool call belongs to. Used in events.
    pub session_id: Arc<String>,
    /// The task id this tool call belongs to.
    pub task_id: Arc<String>,
}

impl ToolCtx {
    /// Resolve a path under `repo_root` if relative; pass through if absolute.
    /// Used by every tool that takes a `path` argument.
    pub fn resolve_path(&self, path: impl AsRef<std::path::Path>) -> PathBuf {
        let p = path.as_ref();
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.repo_root.join(p)
        }
    }

    /// True if the given (already-resolved) path is protected from
    /// agent writes/deletes. Match is by prefix: a directory in
    /// `protected_paths` protects everything under it.
    pub fn is_protected(&self, resolved: &std::path::Path) -> bool {
        // Canonicalize both sides where possible so symlink games can't
        // bypass the check. If canonicalization fails (file doesn't
        // exist yet), fall back to lexical comparison — which is the
        // safe direction because writes to nonexistent files always
        // pass through resolve_path first.
        let candidate = std::fs::canonicalize(resolved).unwrap_or_else(|_| resolved.to_path_buf());
        for p in self.protected_paths.iter() {
            let full = if p.is_absolute() {
                p.clone()
            } else {
                self.repo_root.join(p)
            };
            let canon = std::fs::canonicalize(&full).unwrap_or(full.clone());
            if candidate == canon {
                return true;
            }
            if candidate.starts_with(&canon) {
                return true;
            }
            // Also check the un-canonicalized form for "doesn't exist yet"
            // cases like an attempted delete of a missing protected file.
            if resolved == full || resolved.starts_with(&full) {
                return true;
            }
        }
        false
    }
}

/// A tool error. Distinct from a generic `anyhow::Error` so the loop
/// can decide whether to retry or surface it. The dispatcher converts
/// `Result<String, ToolError>` into the event log entry shape.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    BadArgs(String),
    #[error("protected path: {path}")]
    ProtectedPath { path: PathBuf },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool runtime error: {0}")]
    Runtime(String),
    #[error("tool not yet implemented in this milestone: {0}")]
    NotImplemented(String),
}

pub type ToolResult = std::result::Result<String, ToolError>;

/// The `Tool` trait. One instance per tool kind, registered once and
/// reused across all invocations.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The canonical name the LLM uses to call this tool.
    fn name(&self) -> &str;

    /// JSON-Schema for the tool's parameters. Used to build the
    /// `ToolSchema` that goes to the LLM.
    fn schema(&self) -> ToolSchema;

    /// The isolation class this tool requires.
    fn host_class(&self) -> HostClass {
        HostClass::InProc
    }

    /// True if this tool is in the always-loaded core set.
    fn is_core(&self) -> bool {
        true
    }

    /// True if this tool is read-only and may run in parallel with
    /// other read-only tools in the same assistant message.
    fn is_read_only(&self) -> bool {
        false
    }

    /// Per-invocation timeout. Default 120s.
    fn timeout(&self) -> Duration {
        Duration::from_secs(120)
    }

    /// Execute the tool. The result string is what the LLM sees as
    /// the tool result message content.
    async fn invoke(&self, ctx: &ToolCtx, args: serde_json::Value) -> ToolResult;
}

#[cfg(test)]
mod tests {
    use super::*;
    use sl_store::Store;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn ctx_with_protected(protected: Vec<PathBuf>) -> ToolCtx {
        let store = Store::open_in_memory().unwrap();
        ToolCtx {
            store,
            repo_root: Arc::new(PathBuf::from("/tmp/repo")),
            data_dir: Arc::new(PathBuf::from("/tmp/data")),
            protected_paths: Arc::new(protected),
            session_id: Arc::new("s".into()),
            task_id: Arc::new("t".into()),
        }
    }

    #[test]
    fn resolve_path_joins_relative() {
        let ctx = ctx_with_protected(vec![]);
        let p = ctx.resolve_path("VERSION");
        assert_eq!(p, PathBuf::from("/tmp/repo/VERSION"));
    }

    #[test]
    fn resolve_path_passes_absolute() {
        let ctx = ctx_with_protected(vec![]);
        let p = ctx.resolve_path("/etc/hosts");
        assert_eq!(p, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn protected_exact_match_lexical() {
        let ctx = ctx_with_protected(vec![PathBuf::from("prompts/CHARTER.md")]);
        let p = PathBuf::from("/tmp/repo/prompts/CHARTER.md");
        assert!(ctx.is_protected(&p));
    }

    #[test]
    fn protected_directory_prefix_lexical() {
        let ctx = ctx_with_protected(vec![PathBuf::from("journal/")]);
        let p = PathBuf::from("/tmp/repo/journal/0001-entry.md");
        assert!(ctx.is_protected(&p));
    }

    #[test]
    fn unprotected_path_passes() {
        let ctx = ctx_with_protected(vec![PathBuf::from("prompts/CHARTER.md")]);
        let p = PathBuf::from("/tmp/repo/src/main.rs");
        assert!(!ctx.is_protected(&p));
    }
}
