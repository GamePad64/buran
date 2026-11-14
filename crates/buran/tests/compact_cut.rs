//! Compactor cut-point logic (ADR 0006): the span keeps the most recent
//! `keep_tokens`-budgeted history, cut ONLY at user/assistant message
//! boundaries, never between `assistant_with_tool_calls` and its tool
//! result (a request containing a dangling tool_call or a dangling tool
//! result is invalid on the OpenAI-compatible wire — the invariant is
//! wire correctness, not policy). `cut_span` is pure: same history,
//! same budget, same cut.

use buran::compact::cut_span;
use buran::llm::types::{ChatMessage, FunctionCall, ToolCallReq};

fn u(text: &str) -> ChatMessage {
    ChatMessage::user(text)
}

fn a(text: &str) -> ChatMessage {
    ChatMessage::assistant(text)
}

/// ~400 chars ≈ 100 est. tokens per message, so budgets are readable.
fn big(tag: &str) -> ChatMessage {
    ChatMessage::user(format!("{tag} {}", "x".repeat(400)))
}

fn assistant_with_call(id: &str) -> ChatMessage {
    ChatMessage::assistant_with_tool_calls(vec![ToolCallReq {
        id: id.into(),
        ty: buran::llm::types::FunctionType::Function,
        function: FunctionCall {
            name: "read_file".into(),
            arguments: "{}".into(),
        },
    }])
}

#[test]
fn cut_keeps_recent_budget_and_lands_on_a_user_boundary() {
    // 6 big user messages (~100 tokens each); keep 250 → keeps ~3, cut
    // before message index 3 (a user boundary).
    let history: Vec<ChatMessage> = (0..6).map(|i| big(&format!("m{i}"))).collect();
    let cut = cut_span(&history, 250).expect("span exists");
    assert_eq!(cut, 3, "cut index is the first KEPT message");
    // The kept tail budget: (6-3)*100 = 300 >= 250, and one fewer message
    // would be 200 < 250 — maximal span under the budget that starts clean.
}

#[test]
fn cut_never_dangles_a_tool_call() {
    // Tail: [.., assistant_with_call("t1"), tool result, user "next"].
    // A naive budget cut that lands between the call and its result is
    // forbidden; the cut must walk back to the assistant_with_call (or
    // earlier).
    let history = vec![
        big("m0"),
        assistant_with_call("t1"),
        ChatMessage::tool("t1", "file body"),
        u("next question"),
    ];
    // Budget small enough that a naive token walk would try to start at
    // the tool result: the answer must not be that index.
    let cut = cut_span(&history, 10).expect("span exists");
    assert!(
        cut != 2,
        "cut on the tool result orphans the assistant tool_call"
    );
    // The kept span must start at (or before) the tool-call message.
    assert!(
        cut <= 1,
        "kept span must include the tool_call with its result: cut={cut}"
    );
}

#[test]
fn cut_never_starts_on_a_dangling_tool_result() {
    // History STARTS with a tool result whose call was compacted away in
    // an earlier era: starting at index 0 would be invalid; the only
    // valid cut here is 0 itself (keep everything) or dropping past the
    // orphan — the function must return a span that validates.
    let history = vec![ChatMessage::tool("orphan", "leftover"), u("after")];
    // Budget below the total (6 tokens): the walk reaches the head, where
    // the only reachable cut is 0 — and the orphan-head rule turns that
    // into a cut at 1 rather than "nothing to compact".
    let cut = cut_span(&history, 4).expect("span exists");
    let kept = &history[cut..];
    for msg in kept {
        if msg.role == buran::llm::types::Role::Tool {
            panic!("kept span starts with a dangling tool result: cut={cut}");
        }
        if !msg.tool_calls.is_empty() {
            // A tool_call must be followed (within the span) by its result.
            let has_result = kept
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some(msg.tool_calls[0].id.as_str()));
            assert!(
                has_result,
                "tool_call without its result in span: cut={cut}"
            );
        }
    }
    assert_eq!(cut, 1, "the orphan head is cut off; 'after' is kept");
}

#[test]
fn no_cut_when_history_fits_budget() {
    let history = vec![big("only"), a("ok")];
    assert_eq!(cut_span(&history, 10_000), None, "fits: nothing to compact");
}

#[test]
fn empty_history_yields_no_span() {
    assert_eq!(cut_span(&[], 100), None);
}

#[test]
fn tiny_budget_still_keeps_one_complete_exchange() {
    // Budget too small for anything: keep exactly the last message — but
    // never a lone tool result. Tail here is a user message, so the span
    // is that single message.
    let history = vec![big("m0"), u("last")];
    let cut = cut_span(&history, 1).expect("span exists");
    assert_eq!(cut, 1, "minimal span is the last boundary message");
}
