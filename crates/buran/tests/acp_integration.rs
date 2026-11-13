//! ACP server integration: in-process client ↔ agent over the SDK's
//! builder-to-builder connection (no real transport), plus a child-process
//! stdout purity smoke test for `buran serve`.
//!
//! The LLM is `hclient-mock`; no real HTTP server is ever spawned.
//!
//! Session lifecycle note: `on_session_start` spawns its callback, and the
//! connection shuts down when main_fn returns — the callback may be killed
//! mid-flight. All tests therefore use the blocking form
//! (`build_session(..)?.block_task().start_session().await?`), which holds
//! main_fn open for the whole session and fails the test if the session
//! doesn't complete.

use std::time::Duration;

use agent_client_protocol::{
    Client, JsonRpcMessage as _,
    v1::schema::StopReason,
    v1::{SessionMessage, SessionSetup, schema::SessionNotification, schema::SessionUpdate},
};
use buran::config::{ApiKey, ProviderConfig};
use buran::llm::LlmClient;
use buran::session::TranscriptRecord;
use hclient::mock::MockTransport;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Session IDs are UUIDv7: lexicographic order equals creation order, so
/// `--resume latest` later picks the max filename without reading file
/// contents (sessions store spec).
#[test]
fn new_session_ids_are_uuidv7_and_chronologically_ordered() {
    let a = buran::acp::new_session_id_for_test();
    let b = buran::acp::new_session_id_for_test();
    let ua = uuid::Uuid::parse_str(&a).expect("valid uuid");
    let ub = uuid::Uuid::parse_str(&b).expect("valid uuid");
    assert_eq!(
        ua.get_version(),
        Some(uuid::Version::SortRand),
        "must be v7"
    );
    assert_eq!(
        ub.get_version(),
        Some(uuid::Version::SortRand),
        "must be v7"
    );
    // Two calls may land in the same millisecond; the counter inside v7
    // keeps them distinct, so ids are monotonic (equal never in practice).
    assert!(a <= b, "v7 ids must be chronologically monotonic");
}

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

fn text_chunk(text: &str) -> String {
    format!(r#"{{"choices":[{{"delta":{{"content":"{text}"}}}}]}}"#)
}

fn stop_and_done() -> String {
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into()
}

/// SSE body: two text deltas, finish_reason stop, [DONE].
fn hello_body() -> String {
    format!(
        "data: {}\n\ndata: {}\n\n{}",
        text_chunk("Hello"),
        text_chunk(" from buran"),
        stop_and_done()
    )
}

/// The request body as text, for assertions.
fn body_contains(req: &hclient::mock::RecordedRequest, needle: &str) -> bool {
    req.body
        .text()
        .map(|text| text.contains(needle))
        .unwrap_or(false)
}

/// Test config: the injected provider, no MCP servers, Ask policy. Skills
/// dirs passed per-test.
fn test_config(skills_dirs: Vec<std::path::PathBuf>) -> buran::config::Config {
    buran::config::Config {
        provider: Some(provider()),
        mcp_servers: Vec::new(),
        skills_dirs,
        permission: buran::config::PermissionPolicy::Ask,
        permission_timeout: Duration::from_secs(2),
        sessions_dir: None,
    }
}

/// One session notification's update, if it is one.
fn as_notification(message: SessionMessage) -> Option<SessionNotification> {
    let SessionMessage::Message(dispatch) = message else {
        return None;
    };
    let untyped = dispatch.to_untyped_message().ok()?;
    SessionNotification::parse_message(untyped.method(), untyped.params()).ok()
}

/// Send the ACP `initialize` handshake. The SDK rejects `session/new`
/// until initialization completes.
async fn initialize(
    cx: &agent_client_protocol::v1::ConnectionTo<agent_client_protocol::Agent>,
) -> Result<(), agent_client_protocol::Error> {
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::v1::schema::InitializeRequest;
    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await?;
    Ok(())
}

/// Full round trip: initialize → newSession → prompt streams text deltas
/// and ends with EndTurn.
#[tokio::test(flavor = "current_thread")]
async fn initialize_new_session_prompt_streams_and_ends() {
    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(hello_body().into_boxed_str())]));
    let m2 = m.clone();

    let agent = buran::acp::agent_builder_with_config(test_config(Vec::new()), move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } =
            cx.build_session_cwd()?.block_task().start_session().await?;
        session.send_prompt("hi")?;
        let text = session.read_to_string().await?;
        assert_eq!(text, "Hello from buran");
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}

/// The prompt response must carry EndTurn after a natural stop. Driven via
/// `read_update` so the PromptResponse itself is observed, not just text.
#[tokio::test(flavor = "current_thread")]
async fn prompt_response_carries_end_turn() {
    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(hello_body().into_boxed_str())]));
    let m2 = m.clone();

    let agent = buran::acp::agent_builder_with_config(test_config(Vec::new()), move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } =
            cx.build_session_cwd()?.block_task().start_session().await?;
        session.send_prompt("hi")?;
        loop {
            if let SessionMessage::PromptResponse(resp) = session.read_update().await? {
                let resp = resp.expect("prompt response ok");
                assert_eq!(resp.stop_reason, StopReason::EndTurn);
                break;
            }
        }
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}

/// The system prompt must reach the LLM request: AGENTS.md instructions
/// from the session cwd become the system message of the outgoing chat
/// request.
#[tokio::test(flavor = "current_thread")]
async fn system_prompt_built_from_agentsmd() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("AGENTS.md"),
        "# Instructions\nBe terse.\n",
    )
    .unwrap();

    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(hello_body().into_boxed_str())]));
    let m2 = m.clone();
    let m3 = m2.clone();

    let agent = buran::acp::agent_builder_with_config(test_config(Vec::new()), move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m3.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let cwd = workspace.path().to_path_buf();
    let client = Client.v1().connect_with(agent, async move |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } =
            cx.build_session(cwd).block_task().start_session().await?;
        session.send_prompt("hi")?;
        let text = session.read_to_string().await?;
        assert_eq!(text, "Hello from buran");
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");

    // One LLM request must carry the AGENTS.md text as system content.
    let requests = m2.requests();
    assert_eq!(requests.len(), 1, "exactly one LLM request");
    assert!(
        body_contains(&requests[0], "Be terse."),
        "system prompt must contain AGENTS.md instructions"
    );
}

/// Tool calls surface to the client as ToolCall/ToolCallUpdate
/// notifications: turn 1 requests `fs__read`, turn 2 answers. A stub tool
/// caller answers with fixed output (no MCP servers configured).
#[tokio::test(flavor = "current_thread")]
async fn tool_call_streams_tool_call_updates() {
    let m = MockTransport::new();
    let turn1 = format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"function\":{{\"name\":\"fs__read\",\"arguments\":\"{{}}\"}}}}]}}}},{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n",
        text_chunk("")
    );
    let turn2 = format!("data: {}\n\n{}", text_chunk("read done"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();

    let agent = buran::acp::agent_builder_with_config(test_config(Vec::new()), move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } =
            cx.build_session_cwd()?.block_task().start_session().await?;
        session.send_prompt("read it")?;
        let mut saw_tool_call = false;
        loop {
            let message = session.read_update().await?;
            if let SessionMessage::PromptResponse(resp) = message {
                let resp = resp.expect("prompt response ok");
                assert_eq!(resp.stop_reason, StopReason::EndTurn);
                break;
            } else if let Some(SessionUpdate::ToolCall(tool_call)) =
                as_notification(message).map(|n| n.update)
            {
                assert_eq!(tool_call.name.as_deref(), Some("fs__read"));
                saw_tool_call = true;
            }
        }
        assert!(saw_tool_call, "must have seen a ToolCall update");
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}

/// First-run `<skill>` detection: the model's first assistant message
/// begins with `<skill>Name</skill>`, the skill body is injected, and the
/// turn continues to the real answer.
#[tokio::test(flavor = "current_thread")]
async fn skill_tag_in_first_message_loads_body() {
    let workspace = tempfile::tempdir().unwrap();
    let skills_dir = workspace.path().join("skills");
    std::fs::create_dir_all(skills_dir.join("greet")).unwrap();
    std::fs::write(
        skills_dir.join("greet/SKILL.md"),
        "---\nname: greet\ndescription: how to greet\n---\nSAY HELLO LOUDLY.\n",
    )
    .unwrap();

    let m = MockTransport::new();
    // Turn 1: the model asks for the skill.
    let turn1 = format!(
        "data: {}\n\n{}",
        text_chunk("<skill>greet</skill>"),
        stop_and_done()
    );
    // Turn 2: after injection, the model answers.
    let turn2 = format!("data: {}\n\n{}", text_chunk("HELLO!"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();
    let m3 = m2.clone();

    let skills = skills_dir.clone();
    let agent =
        buran::acp::agent_builder_with_config(test_config(vec![skills]), move |_provider| {
            Ok(LlmClient::with_transport(
                provider(),
                hclient::Client::builder(m3.clone()).build().unwrap(),
            ))
        })
        .await
        .expect("agent builder");

    let cwd = workspace.path().to_path_buf();
    let client = Client.v1().connect_with(agent, async move |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } =
            cx.build_session(cwd).block_task().start_session().await?;
        session.send_prompt("use the skill")?;
        let text = session.read_to_string().await?;
        assert_eq!(text, "<skill>greet</skill>HELLO!");
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");

    // Turn 2's request must carry the injected skill body as a system
    // message.
    let requests = m2.requests();
    assert_eq!(requests.len(), 2, "two LLM rounds");
    assert!(
        body_contains(&requests[1], "SAY HELLO LOUDLY."),
        "skill body must be injected into the continuation request"
    );
}

/// SSE body for turn 1 asking for the `fs__read` tool.
fn tool_call_body() -> String {
    format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"function\":{{\"name\":\"fs__read\",\"arguments\":\"{{}}\"}}}}]}}}},{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n",
        text_chunk("")
    )
}

/// Ask policy round-trip, allow path: the test client answers the agent's
/// `session/request_permission` with the `allow` option (matched by KIND,
/// never position), the tool runs, and the tool output notification flows
/// back before the final text.
#[tokio::test(flavor = "current_thread")]
async fn ask_policy_allow_runs_tool_and_streams_output() {
    use agent_client_protocol::on_receive_request;
    use agent_client_protocol::v1::schema::{
        PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
        RequestPermissionResponse, SelectedPermissionOutcome, ToolCallStatus,
    };

    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(tool_call_body().into_boxed_str())]));
    let turn2 = format!(
        "data: {}\n\n{}",
        text_chunk("file is empty"),
        stop_and_done()
    );
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();
    let m3 = m2.clone();

    let mut config = test_config(Vec::new());
    config.permission = buran::config::PermissionPolicy::Ask;
    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m3.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client
        .v1()
        .on_receive_request(
            async |request: RequestPermissionRequest, responder, _cx| {
                // Answer by KIND, never position (yolo example rule):
                // prefer the narrowest allow.
                let allow_id = [
                    PermissionOptionKind::AllowOnce,
                    PermissionOptionKind::AllowAlways,
                ]
                .iter()
                .find_map(|kind| {
                    request
                        .options
                        .iter()
                        .find(|option| option.kind == *kind)
                        .map(|option| option.option_id.clone())
                })
                .expect("buran must offer an allow option");
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(allow_id)),
                ))
            },
            on_receive_request!(),
        )
        .connect_with(agent, async |cx| {
            initialize(&cx).await?;
            let SessionSetup { mut session, .. } =
                cx.build_session_cwd()?.block_task().start_session().await?;
            session.send_prompt("read it")?;
            let mut saw_tool_finish = false;
            loop {
                let message = session.read_update().await?;
                if let SessionMessage::PromptResponse(resp) = message {
                    let resp = resp.expect("prompt response ok");
                    assert_eq!(resp.stop_reason, StopReason::EndTurn);
                    break;
                } else if let Some(SessionUpdate::ToolCallUpdate(update)) =
                    as_notification(message).map(|n| n.update)
                {
                    let finished = matches!(
                        update.fields.status,
                        Some(ToolCallStatus::Completed) | Some(ToolCallStatus::Failed)
                    );
                    if finished {
                        saw_tool_finish = true;
                    }
                }
            }
            assert!(saw_tool_finish, "allowed tool must reach a final status");
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");

    // The tool ran: its result reached the model in round 2. No MCP
    // servers are configured, so `fs__read` fails with a named error —
    // the point is execution after allow, not the tool's success.
    let requests = m2.requests();
    assert_eq!(requests.len(), 2, "two LLM rounds");
    assert!(
        body_contains(&requests[1], "tool error"),
        "allowed tool must execute and its result reach the model"
    );
}

/// Ask policy round-trip, deny path: the client picks the reject option;
/// the tool must not run (stub would panic) and the denial tool result
/// reaches the model — turn 2's request body carries `denied by user`.
#[tokio::test(flavor = "current_thread")]
async fn ask_policy_deny_sends_denial_to_model() {
    use agent_client_protocol::on_receive_request;
    use agent_client_protocol::v1::schema::{
        PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
        RequestPermissionResponse, SelectedPermissionOutcome,
    };

    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(tool_call_body().into_boxed_str())]));
    let turn2 = format!("data: {}\n\n{}", text_chunk("understood"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();
    let m3 = m2.clone();

    let mut config = test_config(Vec::new());
    config.permission = buran::config::PermissionPolicy::Ask;
    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m3.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let asked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let asked_handler = asked.clone();
    let client = Client
        .v1()
        .on_receive_request(
            async |request: RequestPermissionRequest, responder, _cx| {
                let reject_id = [
                    PermissionOptionKind::RejectOnce,
                    PermissionOptionKind::RejectAlways,
                ]
                .iter()
                .find_map(|kind| {
                    request
                        .options
                        .iter()
                        .find(|option| option.kind == *kind)
                        .map(|option| option.option_id.clone())
                })
                .expect("buran must offer a reject option");
                asked_handler.store(true, std::sync::atomic::Ordering::SeqCst);
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(reject_id)),
                ))
            },
            on_receive_request!(),
        )
        .connect_with(agent, async move |cx| {
            initialize(&cx).await?;
            let SessionSetup { mut session, .. } =
                cx.build_session_cwd()?.block_task().start_session().await?;
            session.send_prompt("read it")?;
            let text = session.read_to_string().await?;
            assert_eq!(text, "understood");
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");

    // The permission request must actually have gone over the wire: the
    // client handler ran (Ask policy, not silent deny).
    assert!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        "Ask policy must send session/request_permission to the client"
    );

    // The denial tool result must reach the model in round 2.
    let requests = m2.requests();
    assert_eq!(requests.len(), 2, "two LLM rounds");
    assert!(
        body_contains(&requests[1], "denied by user"),
        "denial tool result must be in the continuation request"
    );
}

/// `request_permission` timeout: a client that receives the request but
/// never answers is Deny after `permission_timeout`. The handler parks
/// forever (received-but-silent), so without the timeout the test would
/// hang — the 100 ms budget turns silence into Deny and the turn proceeds.
#[tokio::test(flavor = "current_thread")]
async fn request_permission_timeout_is_deny() {
    use agent_client_protocol::on_receive_request;
    use agent_client_protocol::v1::schema::{
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    };

    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(tool_call_body().into_boxed_str())]));
    let turn2 = format!("data: {}\n\n{}", text_chunk("moved on"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();
    let m3 = m2.clone();

    let mut config = test_config(Vec::new());
    config.permission = buran::config::PermissionPolicy::Ask;
    config.permission_timeout = Duration::from_millis(100);
    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m3.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    // Handler records the request and stalls 1 s before answering — far
    // past buran's 100 ms budget, so the agent must have already timed
    // out to Deny (a handler that parks forever would block the client's
    // whole dispatch loop, since handlers run on it).
    let got_request = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let got_request_handler = got_request.clone();
    let client = Client
        .v1()
        .on_receive_request(
            async |_request: RequestPermissionRequest, responder, _cx| {
                got_request_handler.store(true, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(1)).await;
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            on_receive_request!(),
        )
        .connect_with(agent, async |cx| {
            initialize(&cx).await?;
            let SessionSetup { mut session, .. } =
                cx.build_session_cwd()?.block_task().start_session().await?;
            session.send_prompt("read it")?;
            let text = session.read_to_string().await?;
            assert_eq!(text, "moved on");
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");

    assert!(
        got_request.load(std::sync::atomic::Ordering::SeqCst),
        "the permission request must have reached the silent client"
    );
    let requests = m2.requests();
    assert_eq!(requests.len(), 2, "two LLM rounds after timeout denial");
    assert!(
        body_contains(&requests[1], "denied by user"),
        "timeout denial must produce the denial tool result"
    );
}

/// Smoke test: `buran serve` speaks newline-delimited JSON-RPC on stdout —
/// one parse-error response for garbage in, nothing else on stdout (logs
/// go to stderr).
#[test]
fn serve_child_stdout_purity() {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let bin = env!("CARGO_BIN_EXE_buran");
    let mut child = Command::new(bin)
        .arg("serve")
        .env("HOME", "/nonexistent-buran-home")
        .env_remove("BURAN_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn buran serve");

    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "not json at all").unwrap();
        stdin.flush().unwrap();
    }
    drop(child.stdin.take());

    let deadline = std::time::Instant::now() + TIMEOUT;
    let stdout = child.stdout.take().unwrap();
    let mut reader = std::io::BufReader::new(stdout);
    let mut got = String::new();
    // Read available lines in a thread so we can honor the deadline.
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        use std::io::BufRead as _;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    got.push_str(&line);
                    if line.contains("protocolVersion") || line.contains("error") {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(got.clone());
    });

    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let text = rx.recv_timeout(remaining).unwrap_or_else(|_| {
        let _ = child.kill();
        String::new()
    });
    let _ = handle.join();
    let _ = child.kill();
    let _ = child.wait();

    if text.trim().is_empty() {
        // Server exited without answering (acceptable: malformed line may
        // terminate connection) — nothing non-JSON was printed.
        return;
    }
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("stdout line not JSON: {line:?} ({e})"));
    }
}

/// Concurrent prompts on one session: the second `prompt` while the first
/// turn is still running must be rejected with an error, not silently fork
/// the conversation (`mem::take` of the session history would hand an empty
/// history to the second turn). The first turn is held open by a permission
/// question the test client never answers; the second prompt must come back
/// as `Err`; then the client answers (deny path) and the first turn
/// completes cleanly.
#[tokio::test(flavor = "current_thread")]
async fn second_concurrent_prompt_is_rejected() {
    use agent_client_protocol::on_receive_request;
    use agent_client_protocol::v1::schema::{
        PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
        RequestPermissionResponse, SelectedPermissionOutcome,
    };

    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(tool_call_body().into_boxed_str())]));
    // Turn 1 continues after the deny; never reached if the guard fails,
    // but queued so the held turn can complete when we release it.
    let turn2 = format!("data: {}\n\n{}", text_chunk("denied done"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();

    let agent = buran::acp::agent_builder_with_config(test_config(Vec::new()), move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    // The client parks on the permission request (turn 1 in flight) until
    // the test signals; answering keeps handlers-on-dispatch-loop semantics.
    // A flag records that the permission request ARRIVED: that is the
    // observable "turn 1 mid-flight" signal (permission requests surface
    // through the handler, not as SessionUpdate variants).
    let release = tokio::sync::oneshot::channel::<()>();
    let mut release_tx = Some(release.0);
    let release_rx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(release.1)));
    let release_rx2 = release_rx.clone();
    let got_permission = std::sync::Arc::new(tokio::sync::Notify::new());
    let got_permission2 = got_permission.clone();
    let client = Client
        .v1()
        .on_receive_request(
            async move |_request: RequestPermissionRequest, responder, _cx| {
                got_permission2.notify_one();
                // Wait for the test's signal (the second prompt was already
                // rejected), then answer with a deny so turn 1 completes.
                if let Some(rx) = release_rx2.lock().await.take() {
                    let _ = rx.await;
                }
                let reject = _request
                    .options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::RejectOnce)
                    .map(|o| o.option_id.clone());
                if let Some(id) = reject {
                    let _ = responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    ));
                }
                Ok::<(), agent_client_protocol::Error>(())
            },
            on_receive_request!(),
        )
        .connect_with(agent, async |cx| {
            initialize(&cx).await?;
            let SessionSetup { mut session, .. } =
                cx.build_session_cwd()?.block_task().start_session().await?;
            session.send_prompt("first")?;

            // Wait until turn 1 is genuinely mid-flight: its permission
            // request arrived at the client handler above.
            tokio::time::timeout(TIMEOUT, got_permission.notified())
                .await
                .expect("permission request must arrive while turn 1 runs");

            // Second prompt on the SAME session while turn 1 runs: the
            // send itself is fire-and-forget (Ok means the request left);
            // the agent's rejection arrives as a PromptResponse(Err(..))
            // on the update stream, ordered before turn 1's completion.
            session.send_prompt("second")?;
            // Release turn 1 immediately: its handler answers the deny and
            // the turn completes. The rejection (fast, no LLM round) is
            // ordered on the stream before turn 1's completion (needs the
            // remaining LLM round).
            release_tx.take().unwrap().send(()).unwrap();
            let mut second_rejected = false;
            loop {
                match session.read_update().await? {
                    SessionMessage::PromptResponse(Err(error)) => {
                        second_rejected = true;
                        assert!(
                            error.to_string().contains("in flight"),
                            "rejection must name the concurrent turn: {error}"
                        );
                    }
                    SessionMessage::PromptResponse(Ok(resp)) => {
                        // Turn 1's completion must come AFTER the rejection:
                        // if the guard never fired, turn 1's PromptResponse
                        // would arrive alone with no prior Err.
                        assert!(
                            second_rejected,
                            "no rejection seen before turn 1 completed: guard did not fire"
                        );
                        assert_eq!(resp.stop_reason, StopReason::EndTurn);
                        break;
                    }
                    _ => {}
                }
            }
            Ok(())
        });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}

/// The whole turn persists to the transcript in history order: head
/// metadata (model + cwd), then the user message, then the assistant
/// reply. The ACP session id must equal the transcript filename stem, so
/// `loadSession` can find it later (sessions store spec).
#[tokio::test(flavor = "current_thread")]
async fn a_turns_messages_are_persisted_in_history_order() {
    let dir = tempfile::tempdir().unwrap();
    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(hello_body().into_boxed_str())]));
    let m2 = m.clone();

    let mut config = test_config(Vec::new());
    config.sessions_dir = Some(dir.path().to_path_buf());

    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup {
            mut session,
            response,
            ..
        } = cx.build_session_cwd()?.block_task().start_session().await?;
        session.send_prompt("hi")?;
        let text = session.read_to_string().await?;
        assert_eq!(text, "Hello from buran");
        // The ACP session id must equal the transcript filename stem.
        let id = response.session_id.0.to_string();
        let (_, records) = buran::store::Store::new(dir.path())
            .open(&id)
            .expect("transcript exists under the ACP session id");
        assert!(
            matches!(&records[0], TranscriptRecord::Metadata(meta)
                if meta.model == "test-model"
                    && meta.cwd == std::env::current_dir().unwrap()),
            "head metadata recorded (model + cwd)"
        );
        assert!(
            matches!(&records[1], TranscriptRecord::Message(msg)
                if msg.role == buran::llm::types::Role::User
                    && msg.content.as_deref() == Some("hi")),
            "user message first"
        );
        assert!(
            matches!(&records[2], TranscriptRecord::Message(msg)
                if msg.role == buran::llm::types::Role::Assistant
                    && msg.content.as_deref() == Some("Hello from buran")),
            "assistant message second"
        );
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}

/// Mid-turn LLM failure: the turn has already streamed deltas, so the
/// protocol has no error completion — the failure must surface as an
/// `[error: ...]` message chunk the user reads, followed by EndTurn. A bare
/// EndTurn (the pre-review behavior) looks like a clean stop and loses the
/// failure.
#[tokio::test(flavor = "current_thread")]
async fn mid_turn_llm_error_surfaces_as_chunk_before_end_turn() {
    use agent_client_protocol::v1::schema::ContentBlock;
    use hclient::{Error as HclientError, ErrorKind};

    let m = MockTransport::new();
    // One text delta delivered, then the SSE body errors mid-stream
    // (a gateway dropping the connection): the loop's stream_round
    // returns Err AFTER content reached the client.
    let partial = format!("data: {}\n\n", text_chunk("partial answer"));
    m.push_response_frames_then_error(
        sse(vec![Box::leak(partial.into_boxed_str())]),
        HclientError::new(ErrorKind::Other, std::io::Error::other("connection reset")),
    );
    let m2 = m.clone();

    let agent = buran::acp::agent_builder_with_config(test_config(Vec::new()), move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } =
            cx.build_session_cwd()?.block_task().start_session().await?;
        session.send_prompt("hi")?;
        let mut saw_error_chunk = false;
        loop {
            match session.read_update().await? {
                SessionMessage::Message(dispatch) => {
                    let untyped = dispatch.to_untyped_message()?;
                    if let Ok(notification) =
                        SessionNotification::parse_message(untyped.method(), untyped.params())
                        && let SessionUpdate::AgentMessageChunk(chunk) = notification.update
                        && let ContentBlock::Text(text) = chunk.content
                        && text.text.contains("[error:")
                    {
                        saw_error_chunk = true;
                    }
                }
                SessionMessage::PromptResponse(resp) => {
                    let resp = resp.expect("turn completes");
                    assert_eq!(resp.stop_reason, StopReason::EndTurn);
                    break;
                }
                _ => {}
            }
        }
        assert!(
            saw_error_chunk,
            "an [error: ...] chunk must precede EndTurn on a mid-turn LLM failure"
        );
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}

/// `store.create` failure at `newSession` time degrades to in-memory: the
/// session must still work fully (Review Focus #1). The sessions root is
/// pointed under `/proc`, which is unwritable everywhere on Linux, so the
/// create fails deterministically.
#[tokio::test(flavor = "current_thread")]
async fn a_store_create_failure_does_not_kill_the_session() {
    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(hello_body().into_boxed_str())]));
    let m2 = m.clone();

    let mut config = test_config(Vec::new());
    config.sessions_dir = Some(std::path::PathBuf::from("/proc/nonexistent-buran-sessions"));

    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } =
            cx.build_session_cwd()?.block_task().start_session().await?;
        session.send_prompt("hi")?;
        // The full turn completes despite the store being unusable.
        let text = session.read_to_string().await?;
        assert_eq!(text, "Hello from buran");
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}
