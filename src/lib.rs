//! shell-mcp: scoped, allowlisted shell access exposed over the Model Context Protocol.
//!
//! Public modules are re-exported so integration tests and embedders can use the
//! configuration, allowlist, and execution machinery without going through the
//! MCP server layer.

pub mod allowlist;
pub mod config;
pub mod exec;
pub mod root;
pub mod safety;
pub mod tools;

pub use allowlist::{Allowlist, Rule};
pub use config::{Config, LoadedConfig};
pub use exec::{ExecOptions, ExecOutcome};
pub use root::{resolve_root, ResolvedRoot, RootError, RootSource};
pub use safety::{Rejection, RejectionKind};
