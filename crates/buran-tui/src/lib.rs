//! Library half of `buran-tui`: the headless ACP client (`acp_client`)
//! the binary's UI layer drives. Kept as a lib so integration tests can
//! exercise the client against a fake agent without a terminal.

pub mod acp_client;
