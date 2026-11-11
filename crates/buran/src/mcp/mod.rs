//! MCP client side: spawn configured servers, list their tools, call them.
//!
//! Every server is spawned via rmcp over stdio. A server that fails to spawn
//! or handshake is recorded as a failure and skipped — one broken server must
//! not sink the harness (degradation, ADR 0005's fallback philosophy).

mod manager;
mod registry;

pub use manager::{McpError, McpManager};
pub use registry::{ToolCatalog, ToolEntry};
