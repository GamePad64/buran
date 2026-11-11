//! LLM access: OpenAI-compatible chat completions over hclient SSE, plus
//! the agent loop that drives tools and permissions.

pub mod client;
pub mod loop_;
pub mod types;

pub use client::{ChatEvent, ChatStream, LlmClient, LlmError};
