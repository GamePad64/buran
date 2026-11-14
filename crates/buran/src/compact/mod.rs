//! Compaction (ADR 0006): cut-point logic is invariant here; the ONLY
//! LLM seam lives in [`summarizer`]. The transcript canon (JSONL,
//! append-only) is never rewritten — a compaction appends a
//! `CompactionPoint`, and the projection (prompt loop) reads
//! summary + kept tail instead of full history.
//!
//! Token estimates are `chars / 4` (ADR 0006: documented limitation, no
//! tokenizer dependency in v1.5).

pub mod summarizer;

use crate::llm::types::{ChatMessage, Role};

/// Characters per estimated token (ADR 0006).
const CHARS_PER_TOKEN: usize = 4;

/// Estimated token weight of one message: content plus tool-call shells
/// (ids and function names ride the wire too).
fn est_tokens(message: &ChatMessage) -> usize {
    let content = message.content.as_deref().unwrap_or("").len();
    let calls: usize = message
        .tool_calls
        .iter()
        .map(|c| c.id.len() + c.function.name.len() + c.function.arguments.len())
        .sum();
    let tool_id = message.tool_call_id.as_deref().unwrap_or("").len();
    (content + calls + tool_id).div_ceil(CHARS_PER_TOKEN)
}

/// Where compaction cuts: index of the first KEPT message. Everything
/// before it is the span the summarizer sees; the kept tail stays
/// verbatim in the projection.
pub type CutPoint = usize;

/// Walks backwards from the tail accumulating `keep_tokens`, and lands
/// the cut ONLY where the kept span is wire-valid:
/// - not between an `assistant_with_tool_calls` and its following tool
///   result (the pair is atomic),
/// - not directly on a tool result whose call falls outside the span.
///
/// Returns `None` when there is nothing to cut (history fits the budget,
/// or history is empty): the caller skips compaction entirely. The
/// maximal valid span under the budget wins — greedy from the tail.
pub fn cut_span(history: &[ChatMessage], keep_tokens: usize) -> Option<CutPoint> {
    if history.is_empty() {
        return None;
    }
    let total: usize = history.iter().map(est_tokens).sum();
    if total <= keep_tokens {
        return None;
    }

    // Accumulate from the tail; `start` is the candidate cut. Walk as far
    // back as the budget allows, then pull the cut forward past any
    // boundary the wire forbids.
    let mut budget = 0usize;
    let mut start = history.len();
    for (index, message) in history.iter().enumerate().rev() {
        if budget >= keep_tokens {
            break;
        }
        budget += est_tokens(message);
        start = index;
    }
    // `start` is the last message that entered the budget. The cut lands
    // BEFORE it; expanding one more message backwards is next walk's job —
    // here we only repair wire validity of `start..`.
    while start > 0 {
        let message = &history[start];
        if message.role == Role::Tool {
            // A tool result inside the span needs its call inside too.
            let call_is_inside = message
                .tool_call_id
                .as_deref()
                .map(|id| {
                    history[start..]
                        .iter()
                        .any(|m| m.tool_calls.iter().any(|c| c.id == id))
                })
                .unwrap_or(false);
            if !call_is_inside {
                start += 1; // drop the orphan result out of the kept span
                continue;
            }
        }
        if !message.tool_calls.is_empty() {
            // A tool_call needs its result inside the span.
            let result_is_inside = history[start..]
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some(message.tool_calls[0].id.as_str()));
            if !result_is_inside {
                start += 1;
                continue;
            }
        }
        break;
    }
    if start >= history.len() {
        // Degenerate: budget swallowed everything (or repair pushed the
        // cut past the tail) — nothing to compact.
        return None;
    }
    if start == 0 {
        // The budget reaches the head. Normally nothing to compact — but
        // a kept tail must still be wire-valid: a head that STARTS with a
        // tool result whose call is gone (compacted away in an earlier
        // era) can never be sent as-is. Cutting at 1 drops the orphan out
        // of the kept span; the summarizer sees only the orphan itself.
        // A valid head (no leading orphan) has nothing to compact.
        let head = &history[0];
        let orphan_head = head.role == Role::Tool
            || (!head.tool_calls.is_empty()
                && !history
                    .iter()
                    .any(|m| m.tool_call_id.as_deref() == Some(head.tool_calls[0].id.as_str())));
        if orphan_head {
            return Some(1);
        }
        return None;
    }
    Some(start)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::types::ToolCallReq;

    #[test]
    fn token_estimate_counts_content_and_call_shells() {
        let m = ChatMessage::user("abcd".repeat(100)); // 400 chars ≈ 100 tokens
        assert_eq!(est_tokens(&m), 100);
        let call = ChatMessage::assistant_with_tool_calls(vec![ToolCallReq {
            id: "12345678".into(),
            ty: crate::llm::types::FunctionType::Function,
            function: crate::llm::types::FunctionCall {
                name: "read_file".to_string(), // 9 chars
                arguments: "{}".into(),
            },
        }]);
        assert_eq!(est_tokens(&call), 5, "(8+9+2+3)/4 = 5");
    }
}
