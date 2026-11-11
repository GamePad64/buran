//! ACP server: the agent side of the Agent Client Protocol over stdio.

mod server;

pub use server::{agent_builder_with_config, agent_builder_with_factory, serve, serve_with};
