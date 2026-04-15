//! Filesystem tools: fs_read, fs_list, fs_write, fs_delete.
//!
//! All InProc. Protected-path enforcement on writes and deletes is
//! mechanical, not advisory: an attempt to write a protected file
//! returns `ToolError::ProtectedPath` and the LLM sees an error in
//! its tool result. There is no override path through the LLM
//! interface — the only way to modify a protected file is for a
//! human to edit it directly with their own editor, on disk, outside
//! the runtime.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use sl_llm::ToolSchema;

use crate::tool::{Tool, ToolCtx, ToolError, ToolResult};

// ---------------------------------------------------------------------------
// fs_read
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FsReadArgs {
    path: String,
    /// Optional 1-indexed start line for partial reads. Inclusive.
    #[serde(default)]
    start_line: Option<usize>,
    /// Optional inclusive end line. If omitted with `start_line`,
    /// reads from `start_line` to EOF.
    #[serde(default)]
    end_line: Option<usize>,
}

pub struct FsRead;

#[async_trait]
impl Tool for FsRead {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_read",
            "Read a file from the agent's repository or data directory. \
             Relative paths are resolved against the repo root. Optional \
             start_line/end_line for partial reads (1-indexed, inclusive).",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to repo root or absolute." },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line":   { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn invoke(&self, ctx: &ToolCtx, args: serde_json::Value) -> ToolResult {
        let args: FsReadArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        let path = ctx.resolve_path(&args.path);

        let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
            _ => ToolError::Io(e),
        })?;

        match (args.start_line, args.end_line) {
            (None, None) => Ok(text),
            (start, end) => {
                let s = start.unwrap_or(1).max(1);
                let lines: Vec<&str> = text.lines().collect();
                let total = lines.len();
                let start_idx = s - 1;
                let end_idx = end.map(|e| e.min(total)).unwrap_or(total);
                if start_idx >= total {
                    return Ok(String::new());
                }
                let slice = &lines[start_idx..end_idx];
                Ok(slice.join("\n"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// fs_list
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FsListArgs {
    path: String,
}

pub struct FsList;

#[async_trait]
impl Tool for FsList {
    fn name(&self) -> &str {
        "fs_list"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_list",
            "List the contents of a directory in the agent's repository. \
             Returns one entry per line, with a trailing slash for \
             directories. Relative paths resolve against repo root.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn invoke(&self, ctx: &ToolCtx, args: serde_json::Value) -> ToolResult {
        let args: FsListArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        let path = ctx.resolve_path(&args.path);

        let read = std::fs::read_dir(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::NotFound(args.path.clone()),
            _ => ToolError::Io(e),
        })?;

        let mut entries: Vec<String> = Vec::new();
        for entry in read {
            let entry = entry.map_err(ToolError::Io)?;
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry
                .file_type()
                .map(|ft| ft.is_dir())
                .unwrap_or(false);
            entries.push(if is_dir { format!("{name}/") } else { name });
        }
        entries.sort();
        Ok(entries.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// fs_write
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FsWriteArgs {
    path: String,
    content: String,
}

pub struct FsWrite;

#[async_trait]
impl Tool for FsWrite {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_write",
            "Write a file in the agent's repository. Refuses protected paths \
             (CHARTER, journal, .git, etc.). Creates parent directories. \
             Atomic via temp-file-and-rename.",
            json!({
                "type": "object",
                "properties": {
                    "path":    { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
    }
    async fn invoke(&self, ctx: &ToolCtx, args: serde_json::Value) -> ToolResult {
        let args: FsWriteArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        let path = ctx.resolve_path(&args.path);

        if ctx.is_protected(&path) {
            return Err(ToolError::ProtectedPath { path });
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ToolError::Io)?;
        }

        // Atomic write: tmp file in same dir, then rename. This avoids
        // half-written files if the process dies mid-write.
        let tmp = atomic_tmp_path(&path);
        std::fs::write(&tmp, args.content.as_bytes()).map_err(ToolError::Io)?;
        std::fs::rename(&tmp, &path).map_err(ToolError::Io)?;

        Ok(format!(
            "wrote {} bytes to {}",
            args.content.len(),
            path.display()
        ))
    }
}

fn atomic_tmp_path(path: &std::path::Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "out".to_string());
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    tmp.set_file_name(format!(".{}.tmp.{:x}", name, nanos));
    tmp
}

// ---------------------------------------------------------------------------
// fs_delete
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FsDeleteArgs {
    path: String,
}

pub struct FsDelete;

#[async_trait]
impl Tool for FsDelete {
    fn name(&self) -> &str {
        "fs_delete"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_delete",
            "Delete a file. Refuses protected paths and refuses directories \
             (use a future fs_delete_dir for that, intentionally not exposed).",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }
    async fn invoke(&self, ctx: &ToolCtx, args: serde_json::Value) -> ToolResult {
        let args: FsDeleteArgs =
            serde_json::from_value(args).map_err(|e| ToolError::BadArgs(e.to_string()))?;
        let path = ctx.resolve_path(&args.path);

        if ctx.is_protected(&path) {
            return Err(ToolError::ProtectedPath { path });
        }

        match std::fs::metadata(&path) {
            Ok(md) if md.is_dir() => Err(ToolError::Runtime(format!(
                "fs_delete refuses directory {}",
                path.display()
            ))),
            Ok(_) => {
                std::fs::remove_file(&path).map_err(ToolError::Io)?;
                Ok(format!("deleted {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ToolError::NotFound(args.path))
            }
            Err(e) => Err(ToolError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sl_store::Store;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn setup_repo() -> (PathBuf, ToolCtx) {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "sl-fs-test-{}-{:x}-{:?}-{}",
            std::process::id(),
            nanos,
            std::thread::current().id(),
            n
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::create_dir_all(root.join("journal")).unwrap();
        std::fs::write(root.join("VERSION"), "0.0.0\n").unwrap();
        std::fs::write(root.join("prompts/CHARTER.md"), "charter").unwrap();

        let store = Store::open_in_memory().unwrap();
        let ctx = ToolCtx {
            store,
            repo_root: Arc::new(root.clone()),
            data_dir: Arc::new(PathBuf::from("/tmp/data")),
            protected_paths: Arc::new(vec![
                PathBuf::from("prompts/CHARTER.md"),
                PathBuf::from("journal/"),
                PathBuf::from(".git/"),
            ]),
            session_id: Arc::new("s".into()),
            task_id: Arc::new("t".into()),
        };
        (root, ctx)
    }

    fn cleanup(root: &std::path::Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fs_read_full_file() {
        let (root, ctx) = setup_repo();
        let out = FsRead
            .invoke(&ctx, json!({ "path": "VERSION" }))
            .await
            .unwrap();
        assert_eq!(out, "0.0.0\n");
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_read_partial_lines() {
        let (root, ctx) = setup_repo();
        std::fs::write(root.join("multi.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let out = FsRead
            .invoke(
                &ctx,
                json!({ "path": "multi.txt", "start_line": 2, "end_line": 4 }),
            )
            .await
            .unwrap();
        assert_eq!(out, "b\nc\nd");
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_read_missing_file_returns_not_found() {
        let (root, ctx) = setup_repo();
        let err = FsRead
            .invoke(&ctx, json!({ "path": "nope.txt" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_list_returns_sorted_entries() {
        let (root, ctx) = setup_repo();
        let out = FsList.invoke(&ctx, json!({ "path": "." })).await.unwrap();
        // VERSION (file), prompts/ (dir), journal/ (dir) — sorted alphabetically
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.contains(&"VERSION"));
        assert!(lines.contains(&"prompts/"));
        assert!(lines.contains(&"journal/"));
        // sorted
        let mut sorted = lines.clone();
        sorted.sort();
        assert_eq!(lines, sorted);
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_write_creates_file_and_rounds_through() {
        let (root, ctx) = setup_repo();
        let res = FsWrite
            .invoke(
                &ctx,
                json!({ "path": "src/new.rs", "content": "fn main(){}" }),
            )
            .await
            .unwrap();
        assert!(res.starts_with("wrote "));
        let read_back = std::fs::read_to_string(root.join("src/new.rs")).unwrap();
        assert_eq!(read_back, "fn main(){}");
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_write_refuses_protected_charter() {
        let (root, ctx) = setup_repo();
        let err = FsWrite
            .invoke(
                &ctx,
                json!({ "path": "prompts/CHARTER.md", "content": "hacked" }),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::ProtectedPath { path } => {
                assert!(path.ends_with("prompts/CHARTER.md"));
            }
            other => panic!("expected ProtectedPath, got {other:?}"),
        }
        // Charter must still contain its original contents
        let charter = std::fs::read_to_string(root.join("prompts/CHARTER.md")).unwrap();
        assert_eq!(charter, "charter");
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_write_refuses_files_under_protected_dir() {
        let (root, ctx) = setup_repo();
        let err = FsWrite
            .invoke(
                &ctx,
                json!({ "path": "journal/0001-malicious.md", "content": "x" }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ProtectedPath { .. }));
        // The file must not exist
        assert!(!root.join("journal/0001-malicious.md").exists());
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_delete_removes_file() {
        let (root, ctx) = setup_repo();
        std::fs::write(root.join("victim.txt"), "x").unwrap();
        let out = FsDelete
            .invoke(&ctx, json!({ "path": "victim.txt" }))
            .await
            .unwrap();
        assert!(out.starts_with("deleted "));
        assert!(!root.join("victim.txt").exists());
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_delete_refuses_protected_charter() {
        let (root, ctx) = setup_repo();
        let err = FsDelete
            .invoke(&ctx, json!({ "path": "prompts/CHARTER.md" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ProtectedPath { .. }));
        assert!(root.join("prompts/CHARTER.md").exists());
        cleanup(&root);
    }

    #[tokio::test]
    async fn fs_delete_refuses_directories() {
        let (root, ctx) = setup_repo();
        std::fs::create_dir_all(root.join("subdir")).unwrap();
        let err = FsDelete
            .invoke(&ctx, json!({ "path": "subdir" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Runtime(_)));
        cleanup(&root);
    }
}
