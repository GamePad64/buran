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
    let client = AcpClient::spawn(&format!("bash -c {}", shell_quote(&fake_agent_script())))
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
    let client = AcpClient::spawn(&format!("bash -c {}", shell_quote(&fake_agent_script())))
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
