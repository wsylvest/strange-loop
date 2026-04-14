//! Tracing / logging initialization.
//!
//! Two outputs:
//!   - human-readable on stderr (the interactive CLI surface)
//!   - JSON on disk at `<data_dir>/logs/strange-loop.log` (for replay)
//!
//! Level is controlled by the `RUST_LOG` env var or defaults to `info`.

use std::path::Path;

use anyhow::{Context, Result};
use tracing_subscriber::{
    fmt::{self},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
};

/// Initialize tracing. Returns a guard that must be kept alive for the
/// duration of the program (drop it and the file writer flushes).
pub fn init(data_dir: impl AsRef<Path>) -> Result<LoggingGuard> {
    let data_dir = data_dir.as_ref();
    let logs_dir = data_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)
        .with_context(|| format!("creating logs dir {:?}", logs_dir))?;

    let log_file = logs_dir.join("strange-loop.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .with_context(|| format!("opening log file {:?}", log_file))?;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let stderr_layer = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact();

    let file_layer = fmt::layer()
        .json()
        .with_writer(move || {
            // Re-open on every write. Cheap because append-mode fds on
            // unix are ~free to allocate, and it saves us from holding
            // a global writer mutex across async boundaries.
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_file)
                .unwrap_or_else(|_| {
                    // fall back to a sink that discards — we do not want
                    // logging to crash the runtime under any condition.
                    std::fs::File::create("/dev/null").expect("devnull fallback")
                })
        });

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    Ok(LoggingGuard { _file: file })
}

/// Keep the file descriptor alive for the process lifetime.
pub struct LoggingGuard {
    _file: std::fs::File,
}
