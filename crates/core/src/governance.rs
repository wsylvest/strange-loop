//! Governance integrity — charter hash pinning.
//!
//! See `docs/SYSTEM_SPEC.md` §2.2 and the treatise §V. The charter is
//! the immutable-at-runtime layer; this module enforces that immutability
//! by hashing the on-disk file and comparing to a hash stored in `kv`.
//!
//! On boot:
//!   1. Hash `prompts/CHARTER.md` (SHA-256).
//!   2. Compare to `kv['charter_hash']`.
//!   3. If missing, record the current hash as the baseline.
//!   4. If mismatched, return `Drift` — the binary halts with an error
//!      message instructing the owner to run `charter approve`.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use sl_store::{kv, Store};

pub const CHARTER_HASH_KEY: &str = "charter_hash";

/// The result of a charter integrity check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CharterCheck {
    /// No baseline recorded yet. The current hash has been stored as the
    /// new baseline; boot may proceed.
    FirstBoot { hash: String },
    /// Baseline matches on-disk hash. Boot proceeds.
    Match { hash: String },
    /// Baseline and on-disk hash disagree. Boot must halt.
    Drift { on_disk: String, on_record: String },
}

/// Compute the SHA-256 of the file at `path`, returning it as lowercase hex.
pub fn hash_file(path: impl AsRef<Path>) -> Result<String> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading file for hashing: {:?}", path))?;
    let digest = Sha256::digest(&bytes);
    Ok(hex::encode(digest))
}

/// Run the charter integrity check. See module docs for behavior.
pub fn check_charter(store: &Store, charter_path: impl AsRef<Path>) -> Result<CharterCheck> {
    let on_disk = hash_file(charter_path.as_ref())?;
    let on_record = kv::get(store, CHARTER_HASH_KEY)?;
    match on_record {
        None => {
            kv::set(store, CHARTER_HASH_KEY, &on_disk)?;
            tracing::info!(hash = %on_disk, "charter baseline recorded (first boot)");
            Ok(CharterCheck::FirstBoot { hash: on_disk })
        }
        Some(recorded) if recorded == on_disk => {
            tracing::debug!(hash = %on_disk, "charter hash matches baseline");
            Ok(CharterCheck::Match { hash: on_disk })
        }
        Some(recorded) => {
            tracing::warn!(
                on_disk = %on_disk,
                on_record = %recorded,
                "CHARTER DRIFT detected",
            );
            Ok(CharterCheck::Drift {
                on_disk,
                on_record: recorded,
            })
        }
    }
}

/// Explicitly approve the current on-disk hash as the new baseline.
/// Invoked by the `strange-loop charter approve` subcommand.
pub fn approve_current_charter(store: &Store, charter_path: impl AsRef<Path>) -> Result<String> {
    let hash = hash_file(charter_path.as_ref())?;
    kv::set(store, CHARTER_HASH_KEY, &hash)?;
    tracing::info!(hash = %hash, "charter approved");
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile_shim::NamedTempFile;

    #[test]
    fn first_boot_records_baseline() {
        let store = Store::open_in_memory().unwrap();
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "hello charter").unwrap();
        let path = f.path().to_path_buf();

        let result = check_charter(&store, &path).unwrap();
        assert!(matches!(result, CharterCheck::FirstBoot { .. }));
        // now stored
        let stored = kv::get(&store, CHARTER_HASH_KEY).unwrap().unwrap();
        assert_eq!(stored.len(), 64); // sha256 hex
    }

    #[test]
    fn second_boot_same_file_matches() {
        let store = Store::open_in_memory().unwrap();
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "immutable content").unwrap();
        let path = f.path().to_path_buf();

        check_charter(&store, &path).unwrap(); // first boot
        let second = check_charter(&store, &path).unwrap();
        assert!(matches!(second, CharterCheck::Match { .. }));
    }

    #[test]
    fn content_change_triggers_drift() {
        let store = Store::open_in_memory().unwrap();
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::write(&path, "original").unwrap();
        let _first = check_charter(&store, &path).unwrap();

        std::fs::write(&path, "tampered").unwrap();
        let second = check_charter(&store, &path).unwrap();
        match second {
            CharterCheck::Drift { on_disk, on_record } => {
                assert_ne!(on_disk, on_record);
            }
            other => panic!("expected Drift, got {:?}", other),
        }
    }

    #[test]
    fn approve_updates_baseline() {
        let store = Store::open_in_memory().unwrap();
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::fs::write(&path, "v1").unwrap();
        check_charter(&store, &path).unwrap();

        std::fs::write(&path, "v2").unwrap();
        // Before approve: drift
        assert!(matches!(
            check_charter(&store, &path).unwrap(),
            CharterCheck::Drift { .. }
        ));
        // Approve
        approve_current_charter(&store, &path).unwrap();
        // After approve: match
        assert!(matches!(
            check_charter(&store, &path).unwrap(),
            CharterCheck::Match { .. }
        ));
    }
}

// Lightweight tempfile shim to avoid a dev-dep. We only need a handle
// that creates a unique path and deletes on drop. Stdlib has no such
// helper, so we wrap `tempfile::NamedTempFile`... except we don't want
// to pull `tempfile` in. Instead, hand-roll the minimum we need.
#[cfg(test)]
mod tempfile_shim {
    use std::fs::File;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};

    pub struct NamedTempFile {
        path: PathBuf,
        // hold the file open so tests that write via path still work
        // since on unix the fd isn't required for path-based access.
        _file: File,
    }

    impl NamedTempFile {
        pub fn new() -> io::Result<Self> {
            let uniq = format!(
                "strange-loop-test-{}-{}",
                std::process::id(),
                uuid_like()
            );
            let path = std::env::temp_dir().join(uniq);
            let file = File::create(&path)?;
            Ok(Self { path, _file: file })
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Write for NamedTempFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?;
            Write::write(&mut f, buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for NamedTempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("{:x}-{:?}-{}", nanos, std::thread::current().id(), n)
    }
}
