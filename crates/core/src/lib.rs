//! strange-loop core — runtime, config, and governance integrity.
//!
//! This crate is the glue between the store, the (future) LLM client,
//! the (future) tool dispatcher, and the (future) adapters. In M0 it
//! exposes only what the binary needs: config loading, logging setup,
//! a `Runtime` that opens the store and records a session, and the
//! charter-hash integrity check.

pub mod budget;
pub mod config;
pub mod context;
pub mod governance;
pub mod logging;
pub mod runtime;
pub mod tool_loop;

pub use config::Config;
pub use context::{build_context, BuiltContext, TaskKind};
pub use runtime::{Runtime, SessionId};
pub use tool_loop::{run_tool_loop, LoopConfig, LoopOutcome, StopReason};
