//! ACP server: the agent side of the Agent Client Protocol over stdio.

mod permissions;
mod server;

pub use permissions::request_permission;
pub use server::{agent_builder_with_config, agent_builder_with_factory, serve, serve_with};
