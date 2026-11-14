//! Projection (ADR 0006): the LLM request after a compaction carries the
//! last compaction's summary + the KEPT tail, never the compacted span.
//! The window is a method on SessionState so the loop stays I/O-free
//! (ADR 0001) and the projection has exactly one definition — replay
//! (loadSession), the loop, and tests all read the same window.

use buran::llm::loop_::SessionState;
use buran::llm::types::{ChatMessage, Role};

#[test]
fn window_without_compaction_is_the_full_history() {
    let mut s = SessionState::default();
    s.history.push(ChatMessage::user("q1"));
    s.history.push(ChatMessage::assistant("a1"));

    let window = s.window();
    assert_eq!(window.len(), 2, "no summary, no boundary: whole history");
    assert_eq!(window[0].role, Role::User);
}

#[test]
fn window_after_compaction_is_summary_plus_kept_tail() {
    let mut s = SessionState::default();
    // Span (to be compacted): indices 0..2.
    s.history.push(ChatMessage::user("old question"));
    s.history.push(ChatMessage::assistant("old answer"));
    // Kept tail: index 2 onward.
    s.history.push(ChatMessage::user("recent question"));

    // A compaction covering everything before index 2.
    s.set_compaction(2, Some("old stuff summarized".into()));

    let window = s.window();
    assert_eq!(window.len(), 2, "summary stands in for the span + one tail");
    let summary = &window[0];
    assert_eq!(summary.role, Role::System);
    assert!(
        summary
            .content
            .as_deref()
            .unwrap_or("")
            .contains("conversation"),
        "the summary message announces itself: {summary:?}"
    );
    // The tail stays verbatim.
    assert_eq!(window[1], s.history[2]);
}

#[test]
fn window_summary_carries_the_stored_summary_text() {
    let mut s = SessionState::default();
    s.history.push(ChatMessage::user("span start"));
    s.history.push(ChatMessage::user("tail"));
    s.history.push(ChatMessage::user("kept after compaction"));
    // Everything before index 2 collapses into the summary; "kept after
    // compaction" rides verbatim.
    s.set_compaction(2, Some("SPAN SUMMARY TEXT".to_string()));

    let window = s.window();
    assert!(
        window[0]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("SPAN SUMMARY TEXT"),
        "the summary text reaches the request: {:?}",
        window[0]
    );
    assert_eq!(window.len(), 2);
}

#[test]
fn window_before_any_compaction_point_stays_full() {
    // compaction_points carrying an index past the tail (a torn write
    // edge) must not panic or drop history: clamp to the tail.
    let mut s = SessionState::default();
    s.history.push(ChatMessage::user("only"));
    // A torn-write edge: the point lands past the tail.
    s.set_compaction(9, Some("irrelevant".into()));

    let window = s.window();
    assert_eq!(window.len(), 1, "boundary past the tail clamps to tail");
    assert_eq!(window[0], ChatMessage::user("only"));
}

#[test]
fn chained_compactions_use_the_latest_summary() {
    let mut s = SessionState::default();
    s.history.push(ChatMessage::user("m0"));
    s.history.push(ChatMessage::user("m1"));
    s.history.push(ChatMessage::user("m2"));
    s.history.push(ChatMessage::user("m3"));

    // Two compactions: one at 2 (summary A), one at 3 (summary B, newer).
    s.set_compaction(2, Some("SUMMARY A".into()));
    s.set_compaction(3, Some("SUMMARY B".into()));

    let window = s.window();
    assert!(
        window[0]
            .content
            .as_deref()
            .unwrap_or("")
            .contains("SUMMARY B"),
        "the newest compaction wins: {:?}",
        window[0]
    );
    assert_eq!(window.len(), 2, "only the tail past the newest point");
    assert_eq!(window[1], ChatMessage::user("m3"));
}
