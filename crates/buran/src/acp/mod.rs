//! ACP server: the agent side of the Agent Client Protocol over stdio.

mod permissions;
mod server;

pub use permissions::request_permission;
#[doc(hidden)]
pub use server::new_session_id_for_test;
pub use server::{agent_builder_with_config, agent_builder_with_factory, serve, serve_with};
