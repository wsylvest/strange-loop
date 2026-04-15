//! strange-loop tools — registry, dispatcher, and core implementations.
//!
//! The `Tool` trait is the contract every tool implements. The
//! `Registry` owns the catalog. The `Dispatcher` runs them with
//! timeouts and isolation-class routing. Individual tool modules
//! (fs, etc.) provide implementations.
//!
//! See `docs/SYSTEM_SPEC.md` §7 and §3 for design.

pub mod dispatcher;
pub mod fs;
pub mod registry;
pub mod tool;

pub use dispatcher::Dispatcher;
pub use registry::Registry;
pub use tool::{HostClass, Tool, ToolCtx, ToolError, ToolResult};
