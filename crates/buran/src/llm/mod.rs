//! LLM access: OpenAI-compatible chat completions over hclient SSE.

pub mod client;
pub mod types;

pub use client::{ChatEvent, ChatStream, LlmClient, LlmError};
