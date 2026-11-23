//! Headless client tests against a fake agent child: a `bash -c` script
//! speaking canned ACP JSON on stdout and reading requests on stdin.
//!
//! This exercises `AcpClient` end-to-end over stdio without ratatui:
//! initialization, session creation, streamed text chunks surfaced
//! through the unified event stream, a detached turn's stop reason, and
//! session reuse across turns.

use std::time::Duration;

use agent_client_protocol::v1::schema::StopReason;
use buran_tui::acp_client::{AcpClient, ClientEvent};

/// 10 s ceiling per test: a hung handshake fails the test, not CI.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Fake agent: answers `initialize`, `session/new`, and `session/prompt`
/// with one text chunk before the prompt response. The SDK sends UUID
/// string ids, so `id` is extracted as a quoted string and echoed verbatim
/// (JSON-RPC requires exact echo; a numeric-id grep matches nothing).
fn fake_agent_script() -> String {
    r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"fake-session-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake-session-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello from fake"}}}}\n'
      printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
    *'"method":"fs/read_text_file"'*)
      printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32601,"message":"not implemented"}}\n' "$id"
      ;;
    *'"method":"session/request_permission"'*)
      printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32601,"message":"not implemented"}}\n' "$id"
      ;;
  esac
done
"#
    .to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn client_streams_chunks_and_returns_stop_reason() {
    let client = AcpClient::spawn(
        &format!("bash -c {}", shell_quote(&fake_agent_script())),
        None,
        None,
    )
    .await
    .expect("spawn + initialize + newSession against fake agent");

    let mut turn = client.prompt_detached("hi".into());
    let stop = tokio::time::timeout(TIMEOUT, turn.stop_reason())
        .await
        .expect("turn timed out")
        .expect("turn failed");
    assert_eq!(stop, StopReason::EndTurn);

    // The chunk notification races the prompt response, so the event
    // stream must still deliver it after the stop reason resolved.
    let event = tokio::time::timeout(TIMEOUT, client.next_event())
        .await
        .expect("chunk event timed out")
        .expect("agent connection closed before the chunk arrived");
    let ClientEvent::Text(text) = event else {
        panic!("expected a text chunk, got {event:?}");
    };
    assert_eq!(text, "hello from fake");

    client.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn second_prompt_reuses_the_session() {
    let client = AcpClient::spawn(
        &format!("bash -c {}", shell_quote(&fake_agent_script())),
        None,
        None,
    )
    .await
    .expect("spawn");

    let mut first = client.prompt_detached("one".into());
    let first_stop = tokio::time::timeout(TIMEOUT, first.stop_reason())
        .await
        .expect("first turn timed out")
        .expect("first turn failed");
    let mut second = client.prompt_detached("two".into());
    let second_stop = tokio::time::timeout(TIMEOUT, second.stop_reason())
        .await
        .expect("second turn timed out")
        .expect("second turn failed");

    assert_eq!(first_stop, StopReason::EndTurn);
    assert_eq!(second_stop, StopReason::EndTurn);

    client.shutdown();
}

/// Single-quote for shell; embedded quotes are not possible in test scripts.
fn shell_quote(script: &str) -> String {
    format!("'{}'", script.replace('\'', r#"'\''"#))
}

/// Fake that advertises loadSession capability and, on session/load, emits
/// a UserMessageChunk replay before answering the load response. This
/// exercises the TUI client's resume path and the UserText forwarding.
fn fake_agent_script_with_load_replay() -> String {
    r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | sed 's/.*"sessionId":"\([^"]*\)".*/\1/')
      # Replay a prior user turn as the server does on loadSession.
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"prior user turn"}}}}\n' "$sid"
      printf '{"jsonrpc":"2.0","id":"%s","result":{}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#
    .to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn client_loads_session_and_surfaces_replayed_user_text() {
    let client = AcpClient::spawn(
        &format!(
            "bash -c {}",
            shell_quote(&fake_agent_script_with_load_replay())
        ),
        Some("fake-sess-xyz".into()),
        None,
    )
    .await
    .expect("spawn + initialize + loadSession against fake agent");

    // The UserMessageChunk replay should arrive via the unified event stream
    // and be forwarded as ClientEvent::UserText (distinct from assistant Text).
    let event = tokio::time::timeout(TIMEOUT, client.next_event())
        .await
        .expect("replay event timed out")
        .expect("agent connection closed before the replay chunk arrived");
    let ClientEvent::UserText(text) = event else {
        panic!("expected UserText replay chunk, got {event:?}");
    };
    assert_eq!(text, "prior user turn");

    client.shutdown();
}

/// Fake agent with TWO sessions and a loadSession capability: `session/new`
/// mints "fake-session-1"; `session/load` of "fake-session-2" replays one
/// user turn ("other session turn") then answers; load of anything else
/// errors. Both prompt targets answer with a chunk + end_turn tagged by
/// session, so a test can prove WHICH session a prompt hit.
fn fake_agent_two_sessions() -> String {
    r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"fake-session-1"}}\n' "$id"
      ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | sed 's/.*"sessionId":"\([^"]*\)".*/\1/')
      if [ "$sid" = "fake-session-2" ]; then
        printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"other session turn"}}}}\n' "$sid"
        printf '{"jsonrpc":"2.0","id":"%s","result":{}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32602,"message":"unknown session: %s"}}\n' "$id" "$sid"
      fi
      ;;
    *'"method":"session/prompt"'*)
      sid=$(printf '%s' "$line" | sed 's/.*"sessionId":"\([^"]*\)".*/\1/')
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"reply-to-%s"}}}}\n' "$sid" "$sid"
      printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
    *'session/request_permission'*|*'fs/read_text_file'*)
      printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32601,"message":"not implemented"}}\n' "$id"
      ;;
  esac
done
"#
    .to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn switch_session_replays_and_routes_prompt_to_the_new_session() {
    let client = AcpClient::spawn(
        &format!("bash -c {}", shell_quote(&fake_agent_two_sessions())),
        None,
        None,
    )
    .await
    .expect("spawn");

    // Switch to the second session: replay surfaces as UserText.
    let replay = client
        .switch_session("fake-session-2".into())
        .await
        .expect("switch succeeded");
    assert_eq!(replay, "fake-session-2", "current id updated");
    assert_eq!(
        client.current_session_id().as_deref(),
        Some("fake-session-2"),
        "rollback anchor tracks the switch"
    );

    let event = tokio::time::timeout(TIMEOUT, client.next_event())
        .await
        .expect("replay event timed out")
        .expect("connection closed before replay");
    let ClientEvent::UserText(text) = event else {
        panic!("expected UserText replay, got {event:?}");
    };
    assert_eq!(text, "other session turn");

    // A prompt after the switch must carry the NEW session id.
    let mut turn = client.prompt_detached("ping".into());
    let stop = tokio::time::timeout(TIMEOUT, turn.stop_reason())
        .await
        .expect("turn timed out")
        .expect("turn failed");
    assert_eq!(stop, StopReason::EndTurn);
    let event = tokio::time::timeout(TIMEOUT, client.next_event())
        .await
        .expect("chunk event timed out")
        .expect("connection closed before chunk");
    let ClientEvent::Text(text) = event else {
        panic!("expected text chunk, got {event:?}");
    };
    assert!(
        text.contains("fake-session-2"),
        "prompt must target the switched session: {text}"
    );

    client.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn switch_session_failure_leaves_the_current_session_untouched() {
    let client = AcpClient::spawn(
        &format!("bash -c {}", shell_quote(&fake_agent_two_sessions())),
        None,
        None,
    )
    .await
    .expect("spawn");

    let error = client
        .switch_session("no-such-session".into())
        .await
        .expect_err("switch to unknown session must fail");
    assert!(
        error.to_string().contains("no-such-session"),
        "error names the session: {error}"
    );

    // The old session still works: prompt routes to fake-session-1.
    let mut turn = client.prompt_detached("ping".into());
    let stop = tokio::time::timeout(TIMEOUT, turn.stop_reason())
        .await
        .expect("turn timed out")
        .expect("turn failed");
    assert_eq!(stop, StopReason::EndTurn);
    let event = tokio::time::timeout(TIMEOUT, client.next_event())
        .await
        .expect("chunk event timed out")
        .expect("connection closed before chunk");
    let ClientEvent::Text(text) = event else {
        panic!("expected text chunk, got {event:?}");
    };
    assert!(
        text.contains("fake-session-1"),
        "failed switch must not change the active session: {text}"
    );

    client.shutdown();
}

/// Fake announcing its command registry after session/new: the TUI
/// stores the registry for the slash menu (ADR 0006 announcement).
fn fake_agent_with_commands() -> String {
    r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake-session-1","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"compact","description":"Compact the conversation"}]}}}\n'
      printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"fake-session-1"}}\n' "$id"
      ;;
  esac
done
"#
    .to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn commands_from_the_announcement_are_queryable() {
    let client = AcpClient::spawn(
        &format!("bash -c {}", shell_quote(&fake_agent_with_commands())),
        None,
        None,
    )
    .await
    .expect("spawn");

    // The announcement arrives as an event the UI applies to its
    // registry; after draining it, the client exposes the list.
    let event = tokio::time::timeout(TIMEOUT, client.next_event())
        .await
        .expect("announcement timed out")
        .expect("connection closed before announcement");
    let ClientEvent::Commands(commands) = event else {
        panic!("expected Commands event, got {event:?}");
    };
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "compact");
    assert_eq!(commands[0].description, "Compact the conversation");

    client.shutdown();
}

#[tokio::test(flavor = "current_thread")]
async fn resume_latest_with_empty_store_fails_loudly() {
    // Empty HOME store: "latest" cannot resolve. The TUI must fail the
    // spawn with the loud error, not silently start a fresh session. The
    // resolution runs inside the parked main_fn task, so the error travels
    // through the ready slot; the process-wide HOME override points it at
    // the empty tempdir store. Safety: set_var is process-global, but no
    // other test in this binary reads HOME — the rest spawn non-interactive
    // `bash -c` agents (no rc files, no tilde expansion), so the window is
    // benign even under the parallel harness.
    let home = tempfile::tempdir().unwrap();
    // SAFETY: see the comment above — single brief window, no concurrent
    // HOME readers in this test binary.
    unsafe { std::env::set_var("HOME", home.path()) };
    let result = AcpClient::spawn(
        &format!("bash -c {}", shell_quote(&fake_agent_script())),
        Some("latest".into()),
        None,
    )
    .await;
    unsafe { std::env::remove_var("HOME") };
    match result {
        Err(error) => assert!(
            error.to_string().contains("no stored sessions to resume"),
            "the resolution error must surface: {error}"
        ),
        Ok(client) => {
            client.shutdown();
            panic!("empty-store resume must not succeed");
        }
    }
}

mod tool_call_visibility {
    //! Tool-call transparency (ADR 0012 sibling): under allow_all no modal
    //! asks — tool_call notifications surface as events so the transcript
    //! shows what the agent did.

    use super::*;
    use agent_client_protocol::v1::schema::SessionUpdate;

    fn fake_agent_with_tool_call() -> String {
        r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"fake-session-1"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake-session-1","update":{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"read config","name":"fs__read"}}}\n'
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake-session-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"tc1","status":"completed"}}}\n'
      printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#
        .to_string()
    }

    #[tokio::test]
    async fn tool_call_notifications_surface_as_events() {
        let client = AcpClient::spawn(
            &format!("bash -c {}", shell_quote(&fake_agent_with_tool_call())),
            None,
            None,
        )
        .await
        .expect("spawn");

        let mut turn = client.prompt_detached("go".into());
        let mut started = None;
        let mut finished = None;
        for _ in 0..6 {
            let event = tokio::time::timeout(TIMEOUT, client.next_event())
                .await
                .expect("event timed out")
                .expect("connection closed");
            match event {
                ClientEvent::ToolCallStarted { title } => started = Some(title),
                ClientEvent::ToolCallFinished { status, .. } => finished = Some(status),
                _ => {}
            }
            if started.is_some() && finished.is_some() {
                break;
            }
        }
        let _ = turn.stop_reason().await;

        assert_eq!(
            started.as_deref(),
            Some("read config"),
            "started title surfaces: {started:?}"
        );
        assert_eq!(
            finished.as_deref(),
            Some("completed"),
            "finish status surfaces: {finished:?}"
        );
        client.shutdown();
    }
}
