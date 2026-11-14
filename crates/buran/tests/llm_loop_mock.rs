//! Agent loop over a scripted mock LLM: text → tool call → tool result →
//! final text, with permission and cancellation paths.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use buran::config::{ApiKey, ProviderConfig};
use buran::llm::LlmClient;
use buran::llm::loop_::{AgentLoop, PermissionDecision, SessionState, ToolCaller, TurnEvent};
use buran::llm::types::{FinishReason, Role};
use buran::mcp::{McpError, ToolOutput};

fn provider() -> ProviderConfig {
    ProviderConfig {
        name: "test-provider".into(),
        base_url: "http://mock.test/v1".into(),
        model: "test-model".into(),
        api_key: ApiKey::Explicit("sk-test".into()),
    }
}

fn sse(frames: Vec<&'static str>) -> http::Response<Vec<&'static str>> {
    http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(frames)
        .unwrap()
}

/// Stub ToolCaller counting real calls; returns "file body".
#[derive(Default)]
struct StubCaller {
    real_calls: AtomicUsize,
}

impl ToolCaller for StubCaller {
    fn call<'a>(
        &'a self,
        _qualified: &str,
        _args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, McpError>> + Send + 'a>> {
        self.real_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(ToolOutput {
            text: "file body".into(),
            is_error: false,
        })))
    }
}

fn text_chunk(text: &str) -> String {
    format!(r#"{{"choices":[{{"delta":{{"content":"{text}"}}}}]}}"#)
}

fn tool_call_chunks() -> &'static str {
    concat!(
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"fs__read","arguments":"{\"pa"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"}}]}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    )
}

fn stop_chunk(text: &str) -> &'static str {
    concat!(
        r#"data: {"choices":[{"delta":{"content":"REPLACED"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    )
    .replace("REPLACED", text)
    .leak() as &'static str
}

#[tokio::test]
async fn full_turn_text_tool_then_final_text() {
    let m = hclient::mock::MockTransport::new();
    // Turn 1: one response = text, tool call (arguments split across
    // chunks), finish_reason tool_calls, [DONE].
    let turn1 = format!(
        "data: {}\n\n{}",
        text_chunk("Let me check."),
        tool_call_chunks()
    );
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    // Turn 2: final text, stop.
    m.push_response_frames(sse(vec![stop_chunk("Done: contents here")]));

    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let tools = Arc::new(StubCaller::default());
    // Catalog entry matching the scripted tool call: server "fs", tool
    // "read" → wire name fs__read.
    let entry = buran::mcp::ToolEntry {
        server: "fs".into(),
        tool: buran::mcp::McpTool::new("read", "read a file", serde_json::Map::new()),
    };
    let loop_ = AgentLoop::new(llm, tools.clone(), "be helpful".to_string())
        .with_catalog(&buran::mcp::ToolCatalog::new(vec![entry]));
    let mut session = SessionState::default();

    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
    let (perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    // Server-side watcher: answers permission queries found in events, and
    // records the full event sequence for assertions.
    let answer = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(ev) = events_rx.recv().await {
            if let TurnEvent::PermissionQuery(q) = &ev {
                assert_eq!(q.server, "fs");
                assert_eq!(q.tool, "read");
                let _ = perm_tx.send(PermissionDecision::Allow).await;
            }
            seen.push(ev);
        }
        seen
    });

    let summary = loop_
        .run_turn(&mut session, "read the file", events_tx, perm_rx, cancel_rx)
        .await
        .unwrap();
    let got = answer.await.unwrap();

    assert!(
        matches!(&got[0], TurnEvent::TextDelta(t) if t == "Let me check."),
        "events: {got:?}"
    );
    assert!(
        matches!(&got[1], TurnEvent::ToolCallStarted { name, .. } if name == "fs__read"),
        "events: {got:?}"
    );
    assert!(
        matches!(&got[2], TurnEvent::PermissionQuery(q) if q.server == "fs" && q.tool == "read"),
        "events: {got:?}"
    );
    assert!(
        matches!(&got[3], TurnEvent::ToolCallFinished { output, .. } if output.text == "file body"),
        "events: {got:?}"
    );
    assert!(
        matches!(&got[4], TurnEvent::TextDelta(t) if t == "Done: contents here"),
        "events: {got:?}"
    );
    assert!(
        matches!(got.last(), Some(TurnEvent::TurnDone(FinishReason::Stop))),
        "events: {got:?}"
    );
    assert_eq!(summary.tool_calls_made, 1);
    assert_eq!(tools.real_calls.load(Ordering::SeqCst), 1);

    // History: user, assistant(tool_calls), tool(result), assistant(final).
    assert_eq!(session.history.len(), 4, "{:?}", session.history);
    assert_eq!(summary.finish, FinishReason::Stop);
}

#[tokio::test]
async fn denied_tool_never_calls_backend() {
    let m = hclient::mock::MockTransport::new();
    // Tool call, then a final answer produced after the denial result.
    m.push_response_frames(sse(vec![tool_call_chunks()]));
    m.push_response_frames(sse(vec![stop_chunk("Cannot read")]));

    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let tools = Arc::new(StubCaller::default());
    let loop_ = AgentLoop::new(llm, tools.clone(), "be helpful".to_string());
    let mut session = SessionState::default();

    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
    let (perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    // Server-side watcher: deny every query seen in the event stream.
    let answer = tokio::spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            if matches!(ev, TurnEvent::PermissionQuery(_)) {
                let _ = perm_tx.send(PermissionDecision::Deny).await;
            }
        }
    });

    let summary = loop_
        .run_turn(&mut session, "read the file", events_tx, perm_rx, cancel_rx)
        .await
        .unwrap();
    answer.abort();

    assert_eq!(
        tools.real_calls.load(Ordering::SeqCst),
        0,
        "denied call must not run"
    );
    assert_eq!(summary.tool_calls_made, 0);
    let tool_msg = session
        .history
        .iter()
        .find(|msg| msg.tool_call_id.as_deref() == Some("c1"))
        .expect("denial tool result in history");
    assert!(
        tool_msg.content.as_deref().unwrap_or("").contains("denied"),
        "{tool_msg:?}"
    );
}

#[tokio::test]
async fn cancel_stops_turn_gracefully() {
    let m = hclient::mock::MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(
        format!("data: {}\n\n", text_chunk("Let me check.")).into_boxed_str(),
    )]));

    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let tools = Arc::new(StubCaller::default());
    let loop_ = AgentLoop::new(llm, tools, "be helpful".to_string());
    let mut session = SessionState::default();

    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
    let (_perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    // Fire cancel when the first delta arrives.
    let watcher = tokio::spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            if matches!(ev, TurnEvent::TextDelta(_)) {
                let _ = cancel_tx.send(true);
            }
        }
    });

    let summary = loop_
        .run_turn(&mut session, "read the file", events_tx, perm_rx, cancel_rx)
        .await
        .unwrap_or_else(|e| panic!("cancel must be graceful, got: {e}"));
    watcher.abort();

    assert_eq!(
        summary.finish,
        FinishReason::Stop,
        "cancel maps to a clean Stop"
    );
}

/// Usage arriving on the final chunk must surface in the turn summary
/// (hardening v1.5.1: the server appends `TranscriptRecord::Usage` from it).
#[tokio::test]
async fn usage_on_final_chunk_reaches_turn_summary() {
    let m = hclient::mock::MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(
        concat!(
            r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":4,"total_tokens":15}}"#,
            "\n\n",
            "data: [DONE]\n\n",
        )
        .to_string()
        .into_boxed_str(),
    )]));

    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let loop_ = AgentLoop::new(llm, Arc::new(StubCaller::default()), "be helpful".to_string());
    let mut session = SessionState::default();

    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let (_perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    drop(events_rx);

    let summary = loop_
        .run_turn(&mut session, "hi", events_tx, perm_rx, cancel_rx)
        .await
        .unwrap();

    let usage = summary.usage.expect("usage must reach the summary");
    assert_eq!(usage.prompt_tokens, 11);
    assert_eq!(usage.completion_tokens, 4);
    assert_eq!(usage.total_tokens, 15);
}

/// No usage chunk → summary usage stays None (nothing recorded).
#[tokio::test]
async fn no_usage_chunk_leaves_summary_usage_none() {
    let m = hclient::mock::MockTransport::new();
    m.push_response_frames(sse(vec![stop_chunk("plain")]));

    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let loop_ = AgentLoop::new(llm, Arc::new(StubCaller::default()), "be helpful".to_string());
    let mut session = SessionState::default();

    let (events_tx, events_rx) = tokio::sync::mpsc::channel(64);
    let (_perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    drop(events_rx);

    let summary = loop_
        .run_turn(&mut session, "hi", events_tx, perm_rx, cancel_rx)
        .await
        .unwrap();

    assert!(summary.usage.is_none(), "no usage chunk → no usage");
}

/// The same tool with identical arguments, requested round after round, is
/// a runaway loop: after N identical consecutive rounds the streak guard
/// cuts the turn with `FinishReason::LoopGuard`, leaves a system note the
/// model sees on resume, and does not execute call N+1.
#[tokio::test]
async fn same_tool_streak_guard_cuts_after_n_identical_rounds() {
    let m = hclient::mock::MockTransport::new();
    // Five identical rounds of fs__read {"path":"x"}; the guard must cut
    // after 4 (threshold), so round 5's response is never consumed.
    for _ in 0..5 {
        m.push_response_frames(sse(vec![tool_call_chunks()]));
    }

    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let tools = Arc::new(StubCaller::default());
    let entry = buran::mcp::ToolEntry {
        server: "fs".into(),
        tool: buran::mcp::McpTool::new("read", "read a file", serde_json::Map::new()),
    };
    let loop_ = AgentLoop::new(llm, tools.clone(), "be helpful".to_string())
        .with_catalog(&buran::mcp::ToolCatalog::new(vec![entry]))
        .with_same_tool_streak_guard(4);
    let mut session = SessionState::default();

    let (events_tx, mut events_rx) = tokio::sync::mpsc::channel(64);
    let (perm_tx, perm_rx) = tokio::sync::mpsc::channel(8);
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    // Auto-allow everything so the only thing that can stop the turn is
    // the guard.
    let answer = tokio::spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            if let TurnEvent::PermissionQuery(_) = &ev {
                let _ = perm_tx.send(PermissionDecision::Allow).await;
            }
        }
    });

    let summary = loop_
        .run_turn(&mut session, "read the file", events_tx, perm_rx, cancel_rx)
        .await
        .unwrap();
    answer.abort();

    assert_eq!(
        summary.finish,
        FinishReason::LoopGuard,
        "streak guard must cut the turn"
    );
    assert_eq!(
        tools.real_calls.load(Ordering::SeqCst),
        4,
        "the cut round's calls must not execute"
    );
    let last = session.history.last().expect("history not empty");
    assert_eq!(last.role, Role::System, "cut note is a system message");
    assert!(
        last.content
            .as_deref()
            .unwrap_or("")
            .contains("loop guard"),
        "note must explain the cut: {last:?}"
    );
}
