//! The Summarizer seam (ADR 0006): the ONLY LLM dependency of compaction.
//! The builtin implementation calls the session model (or a configured
//! `[compaction] model` alternative); a future MCP implementation would
//! call `tools/call` and need zero changes elsewhere.
//!
//! Wired in task #60 — this file lands first so the module tree compiles;
//! the contract below is the implementation target.

pub trait Summarizer: Send + Sync {
    /// Summarize `span` (already cut by `cut_span`) into a text the
    /// projection can stand in for the span. `instructions` focus the
    /// summary (the `/compact` argument); `budget` caps the OUTPUT in
    /// estimated tokens — the summarizer's own limit, not the session
    /// model's (ADR 0006).
    fn summarize(
        &self,
        span: &[crate::llm::types::ChatMessage],
        instructions: Option<&str>,
        budget: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}
