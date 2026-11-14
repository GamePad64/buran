//! The Summarizer seam (ADR 0006): the ONLY LLM dependency of
//! compaction. The builtin implementation calls the session's LLM client
//! with a summary prompt; a future MCP implementation would call
//! `tools/call` and require zero changes to cut-point logic, the JSONL
//! canon, or the projection.
//!
//! The summary call carries: the span verbatim (as chat messages), the
//! output budget (as a system instruction — the summarizer's own limit
//! applies, not the session model's), and optional user focus
//! instructions (`/compact <instructions>`). Thinking is not requested:
//! the summary is plain text.

use crate::llm::LlmClient;
use crate::llm::client::ChatEvent;
use crate::llm::types::{ChatMessage, ChatRequest, Role};

/// Output budget default (estimated tokens) when a caller has no better
/// number: compaction summaries stay compact.
pub const DEFAULT_SUMMARY_BUDGET: usize = 512;

pub trait Summarizer: Send + Sync {
    /// Summarize `span` (already cut by `cut_span`) into text the
    /// projection can stand in for it. `instructions` focus the summary
    /// (`/compact <instructions>`); `budget` caps the OUTPUT in estimated
    /// tokens. Fails honestly: no summary, no CompactionPoint (ADR 0006
    /// all-or-nothing).
    fn summarize(
        &self,
        span: &[ChatMessage],
        instructions: Option<&str>,
        budget: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}

/// The builtin summarizer: one streaming call to the configured LLM
/// client, text collected across deltas.
pub struct BuiltinSummarizer {
    llm: LlmClient,
}

impl BuiltinSummarizer {
    pub fn new(llm: LlmClient) -> Self {
        Self { llm }
    }
}

impl Summarizer for BuiltinSummarizer {
    async fn summarize(
        &self,
        span: &[ChatMessage],
        instructions: Option<&str>,
        budget: usize,
    ) -> anyhow::Result<String> {
        // The budget and the focus instruction travel as a system head;
        // the span rides as the user turn. One message pair keeps the
        // prompt honest about what is the asked-for content and what is
        // the instruction envelope.
        let mut head = format!(
            "Summarize the following conversation excerpt for later reuse. \
             The summary REPLACES the excerpt in future context: keep task \
             state, decisions, file paths, and unresolved questions; drop \
             filler. Output budget: about {budget} tokens."
        );
        if let Some(focus) = instructions {
            head.push_str("\nAdditional focus from the user: ");
            head.push_str(focus);
        }

        let mut messages = vec![ChatMessage {
            role: Role::System,
            content: Some(head),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }];
        messages.push(ChatMessage {
            role: Role::User,
            content: Some(render_span(span)),
            tool_calls: Vec::new(),
            tool_call_id: None,
        });

        let req = ChatRequest::new(self.llm.model_name().to_string(), messages);
        let mut stream = self.llm.stream_chat(&req).await?;
        let mut summary = String::new();
        loop {
            match stream.next().await? {
                Some(ChatEvent::Delta(text)) => summary.push_str(&text),
                Some(_) => {}
                None => break,
            }
        }
        Ok(summary)
    }
}

/// The span as one user-turn text: roles label each turn so the
/// summarizing model can tell speakers apart. Tool traffic renders as its
/// content only — the summary's consumer never re-executes anything.
fn render_span(span: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in span {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool result",
            Role::System => "system",
        };
        if let Some(content) = message.content.as_deref() {
            out.push_str(role);
            out.push_str(": ");
            out.push_str(content);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_render_labels_roles_and_skips_empty_content() {
        let span = vec![
            ChatMessage::user("question"),
            ChatMessage::assistant_with_tool_calls(Vec::new()), // no content
            ChatMessage::tool("t1", "the file body"),
        ];
        let text = render_span(&span);
        assert!(text.contains("user: question"), "{text}");
        assert!(
            !text.contains("assistant"),
            "contentless turn renders nothing: {text}"
        );
        assert!(text.contains("tool result: the file body"), "{text}");
    }
}
