//! Library half of `buran-tui`: the headless ACP client (`acp_client`)
//! and the pure UI layer (`ui`) the binary drives. Kept as a lib so
//! integration tests can exercise the client against a fake agent and the
//! rendering on ratatui's offscreen backend, both without a terminal.

pub mod acp_client;
pub mod ui;
