//! /compact end-to-end (ADR 0006): the prompt handler intercepts
//! `/compact`, cuts the span, summarizes through the ONE LLM seam, and
//! persists a CompactionPoint; the next prompt's LLM request carries the
//! summary + kept tail, never the compacted span. A Summarizer failure
//! leaves the conversation untouched (all-or-nothing).

use std::time::Duration;

use agent_client_protocol::{
    Client, JsonRpcMessage as _,
    v1::schema::{SessionNotification, SessionUpdate},
    v1::{SessionMessage, SessionSetup},
};
use buran::config::{ApiKey, Config, PermissionPolicy, ProviderConfig};
use buran::llm::LlmClient;
use hclient::Client as Http;
use hclient::mock::MockTransport;

const TIMEOUT: Duration = Duration::from_secs(10);

fn provider() -> ProviderConfig {
    ProviderConfig {
        name: "test".into(),
        base_url: "http://mock.test/v1".into(),
        model: "test-model".into(),
        api_key: ApiKey::Explicit("sk-test".into()),
        context_window: None,
    }
}

fn test_config(sessions_dir: Option<std::path::PathBuf>) -> Config {
    Config {
        provider: Some(provider()),
        mcp_servers: Vec::new(),
        skills_dirs: Vec::new(),
        permission: PermissionPolicy::Ask,
        permission_timeout: Duration::from_secs(2),
        sessions_dir,
        compaction: Default::default(),
        subagents: Default::default(),
        subagent_parent: None,
        hooks: Default::default(),
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

fn body_contains(req: &hclient::mock::RecordedRequest, needle: &str) -> bool {
    req.body
        .text()
        .map(|text| text.contains(needle))
        .unwrap_or(false)
}

fn as_notification(message: SessionMessage) -> Option<SessionNotification> {
    let SessionMessage::Message(dispatch) = message else {
        return None;
    };
    let untyped = dispatch.to_untyped_message().ok()?;
    SessionNotification::parse_message(untyped.method(), untyped.params()).ok()
}

/// Text chunks collected until the prompt response resolves.
async fn drain_to_response(
    session: &mut agent_client_protocol::v1::ActiveSession<'_, agent_client_protocol::Agent>,
) -> anyhow::Result<String> {
    let mut text = String::new();
    loop {
        let message = session.read_update().await?;
        if let SessionMessage::PromptResponse(_) = message {
            return Ok(text);
        }
        if let Some(n) = as_notification(message)
            && let SessionUpdate::AgentMessageChunk(c) = n.update
            && let agent_client_protocol::v1::schema::ContentBlock::Text(t) = c.content
        {
            text.push_str(&t.text);
        }
    }
}

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

#[tokio::test(flavor = "current_thread")]
async fn compact_command_summarizes_and_the_next_prompt_uses_the_summary() {
    let dir = tempfile::tempdir().unwrap();

    // One queued response per LLM round, in order: turn 1, the summary
    // call, turn 2.
    let m = MockTransport::new();
    let turn1 = format!(
        "data: {}\n\n{}",
        text_chunk("long answer about the recipe"),
        stop_and_done()
    );
    let summary = format!(
        "data: {}\n\n{}",
        text_chunk("SUMMARY: recipe discussed"),
        stop_and_done()
    );
    let turn2 = format!(
        "data: {}\n\n{}",
        text_chunk("fresh answer"),
        stop_and_done()
    );
    // One queued response PER LLM round: separate pushes (the mock pops
    // one response per request).
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(summary.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();

    let agent = buran::acp::agent_builder_with_config(
        test_config(Some(dir.path().to_path_buf())),
        move |_p| {
            Ok(LlmClient::with_transport(
                provider(),
                Http::builder(m2.clone()).build().unwrap(),
            ))
        },
    )
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } = cx
            .build_session(std::env::current_dir().unwrap())
            .block_task()
            .start_session()
            .await?;

        session.send_prompt("tell me about the recipe")?;
        let _ = drain_to_response(&mut session).await?;

        // /compact: the compaction note echoes the summary.
        session.send_prompt("/compact")?;
        let note = drain_to_response(&mut session).await?;
        assert!(
            note.contains("SUMMARY: recipe discussed"),
            "the compaction note carries the summary: {note}"
        );

        session.send_prompt("what changed?")?;
        let _ = drain_to_response(&mut session).await?;
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("timed out")
        .expect("connection failed");

    let requests = m.requests();
    assert!(
        requests.len() >= 3,
        "turn1 + summary + turn2 on the wire: {}",
        requests.len()
    );
    assert!(
        body_contains(&requests[2], "SUMMARY: recipe discussed"),
        "the follow-up request carries the summary"
    );
    // The user half of the compacted span is replaced by the summary;
    // where the kept tail lands (here: the assistant half stayed inside
    // the keep budget) is cut_span's budget arithmetic, not the command's
    // contract — the user question going is the observable.
    assert!(
        !body_contains(&requests[2], "tell me about the recipe"),
        "the compacted user turn left the window"
    );
    assert!(
        body_contains(&requests[2], "what changed?"),
        "the kept tail survives"
    );

    // The store canon carries the CompactionPoint (ADR 0007).
    let mut found = false;
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".jsonl")
            && let Ok(text) = std::fs::read_to_string(entry.path())
            && text.contains("compaction_point")
            && text.contains("SUMMARY: recipe discussed")
        {
            found = true;
        }
    }
    assert!(found, "transcript records the compaction with its summary");
}

#[tokio::test(flavor = "current_thread")]
async fn compact_failure_leaves_conversation_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let m = MockTransport::new();
    let turn1 = format!("data: {}\n\n{}", text_chunk("answer one"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    // The summary call fails mid-stream: all-or-nothing.
    let partial = format!("data: {}\n\n", text_chunk("partial sum"));
    let error = hclient::Error::new(
        hclient::ErrorKind::Other,
        std::io::Error::other("connection reset"),
    );
    m.push_response_frames_then_error(sse(vec![Box::leak(partial.into_boxed_str())]), error);
    // Turn 2 (after the failed compact) must still answer.
    let turn2 = format!("data: {}\n\n{}", text_chunk("answer two"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();

    let agent = buran::acp::agent_builder_with_config(
        test_config(Some(dir.path().to_path_buf())),
        move |_p| {
            Ok(LlmClient::with_transport(
                provider(),
                Http::builder(m2.clone()).build().unwrap(),
            ))
        },
    )
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } = cx
            .build_session(std::env::current_dir().unwrap())
            .block_task()
            .start_session()
            .await?;
        session.send_prompt("first")?;
        let _ = drain_to_response(&mut session).await?;

        session.send_prompt("/compact")?;
        let note = drain_to_response(&mut session).await?;
        assert!(
            note.contains("compact failed"),
            "the failure surfaces as a note: {note}"
        );

        // The conversation continues on the FULL window: the follow-up
        // request still carries turn 1.
        session.send_prompt("second")?;
        let _ = drain_to_response(&mut session).await?;
        Ok(())
    });
    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("timed out")
        .expect("connection failed");

    let requests = m.requests();
    let last = requests.last().unwrap();
    assert!(
        body_contains(last, "answer one"),
        "history intact after a failed compaction: turn 1's answer stays in the window"
    );
    assert!(
        body_contains(last, "second"),
        "the new turn is in the request too"
    );
}

/// Auto-compaction (ADR 0006 trigger policy): after a turn, when the
/// window estimate crosses `context_window − reserve`, the agent
/// compacts on its own — the next LLM request carries the summary, not
/// the span — and announces `[auto-compacted …]` to the client. Reserve
/// is set huge here so the threshold is crossed after one turn.
#[tokio::test(flavor = "current_thread")]
async fn auto_compact_fires_after_a_turn_crossing_the_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let m = MockTransport::new();
    // Round 1: the turn itself. Round 2: the auto-compact summary call.
    // Round 3: the follow-up turn.
    let turn1 = format!(
        "data: {}\n\n{}",
        text_chunk("long answer one"),
        stop_and_done()
    );
    let summary = format!(
        "data: {}\n\n{}",
        text_chunk("AUTO SUMMARY"),
        stop_and_done()
    );
    let turn2 = format!("data: {}\n\n{}", text_chunk("answer two"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(summary.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();

    let mut config = test_config(Some(dir.path().to_path_buf()));
    // Threshold ≈ 0: any non-empty window crosses it.
    config.compaction.auto = true;
    config.compaction.reserve = 128_000;
    config.provider.as_mut().unwrap().context_window = Some(128_000);

    let agent = buran::acp::agent_builder_with_config(config, move |_p| {
        Ok(LlmClient::with_transport(
            provider(),
            Http::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } = cx
            .build_session(std::env::current_dir().unwrap())
            .block_task()
            .start_session()
            .await?;

        // Turn 1: history grows past the (zero) threshold; the
        // auto-compaction note arrives as a chunk before the response.
        session.send_prompt("first prompt")?;
        let note = drain_to_response(&mut session).await?;
        assert!(
            note.contains("auto-compacted"),
            "the client sees the auto-compaction: {note}"
        );

        // Turn 2: the request must carry the summary, not turn 1's text.
        session.send_prompt("second prompt")?;
        let _ = drain_to_response(&mut session).await?;
        Ok(())
    });
    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("timed out")
        .expect("connection failed");

    let requests = m.requests();
    assert!(
        requests.len() >= 3,
        "turn + summary + turn: {}",
        requests.len()
    );
    assert!(
        body_contains(&requests[2], "AUTO SUMMARY"),
        "the next request carries the auto summary"
    );
    assert!(
        !body_contains(&requests[2], "first prompt"),
        "the compacted span (user half) left the window"
    );
}

/// `[compaction.summarizer]` (ADR 0006, the Aider use case): the
/// summary call goes to the CONFIGURED ALTERNATE provider, the turn to
/// the session provider. The test factory distinguishes by base_url.
#[tokio::test(flavor = "current_thread")]
async fn compaction_summarizer_section_routes_summary_to_the_alternate_provider() {
    let dir = tempfile::tempdir().unwrap();
    let m = MockTransport::new();
    // All rounds land in ONE mock; assertions are per-request URI.
    let turn1 = format!("data: {}\n\n{}", text_chunk("answer one"), stop_and_done());
    let summary = format!(
        "data: {}\n\n{}",
        text_chunk("CHEAP SUMMARY"),
        stop_and_done()
    );
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(summary.into_boxed_str())]));
    let m2 = m.clone();

    let mut config = test_config(Some(dir.path().to_path_buf()));
    config.compaction.summarizer = Some(buran::config::SummarizerProvider {
        base_url: "http://cheap.test/v1".into(),
        model: "cheap-model".into(),
        api_key_env: "CHEAP_KEY".into(),
    });

    let agent =
        buran::acp::agent_builder_with_config(config, move |p: &buran::config::ProviderConfig| {
            // The factory honors the provider it is handed: the session
            // provider and the summarizer provider produce clients with
            // DIFFERENT base URLs — that distinction is the test's subject.
            Ok(LlmClient::with_transport(
                p.clone(),
                Http::builder(m2.clone()).build().unwrap(),
            ))
        })
        .await
        .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } = cx
            .build_session(std::env::current_dir().unwrap())
            .block_task()
            .start_session()
            .await?;
        session.send_prompt("first")?;
        let _ = drain_to_response(&mut session).await?;
        session.send_prompt("/compact")?;
        let note = drain_to_response(&mut session).await?;
        assert!(
            note.contains("CHEAP SUMMARY"),
            "the summary came back: {note}"
        );
        Ok(())
    });
    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("timed out")
        .expect("connection failed");

    let requests = m.requests();
    assert!(requests.len() >= 2, "turn + summary");
    // Session-provider URI vs the cheap one.
    let session_uri = requests[0].uri.to_string();
    let summary_uri = requests[1].uri.to_string();
    assert!(
        !summary_uri.contains("mock.test"),
        "summary must NOT hit the session provider: {summary_uri}"
    );
    assert!(
        summary_uri.contains("cheap.test"),
        "summary must hit the configured alternate: {summary_uri}"
    );
    assert!(
        session_uri.contains("mock.test"),
        "the turn stays on the session provider: {session_uri}"
    );
    // And the cheap model's name is on the wire.
    assert!(
        body_contains(&requests[1], "cheap-model"),
        "the summarizer model from the section: {}",
        summary_uri
    );
}

/// Overflow recovery (ADR 0006): a turn that ends `finish_reason: length`
/// with NO tool calls (the model was cut off mid-prose) triggers ONE
/// aggressive compaction (keep 25%) and ONE retry of the same prompt.
/// A second failure is final — no loops.
#[tokio::test(flavor = "current_thread")]
async fn length_turn_recovers_with_one_compact_and_retry() {
    let dir = tempfile::tempdir().unwrap();
    let m = MockTransport::new();
    // Round 1: turn 1 answers normally (seeds the history).
    let seed = format!("data: {}\n\n{}", text_chunk("seed answer"), stop_and_done());
    // Round 2: cut off mid-prose (length, no tool calls).
    let cut_off = "data: {\"choices\":[{\"delta\":{\"content\":\"partial prose\"}},{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
    // Round 3: the recovery compaction summary.
    let summary = format!(
        "data: {}\n\n{}",
        text_chunk("RECOVERY SUMMARY"),
        stop_and_done()
    );
    // Round 4: the retry answers normally.
    let retry = format!(
        "data: {}\n\n{}",
        text_chunk("retry answer"),
        stop_and_done()
    );
    m.push_response_frames(sse(vec![Box::leak(seed.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(cut_off.into())]));
    m.push_response_frames(sse(vec![Box::leak(summary.into_boxed_str())]));
    m.push_response_frames(sse(vec![Box::leak(retry.into_boxed_str())]));
    let m2 = m.clone();

    let config = test_config(Some(dir.path().to_path_buf()));
    let agent =
        buran::acp::agent_builder_with_config(config, move |p: &buran::config::ProviderConfig| {
            Ok(LlmClient::with_transport(
                p.clone(),
                Http::builder(m2.clone()).build().unwrap(),
            ))
        })
        .await
        .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } = cx
            .build_session(std::env::current_dir().unwrap())
            .block_task()
            .start_session()
            .await?;

        // Turn 1 fills the history (normal stop).
        session.send_prompt("seed the history")?;
        let _ = drain_to_response(&mut session).await?;
        // Turn 2 hits length → recovery → retry; the client sees the
        // retry's answer.
        session.send_prompt("ask that overflows")?;
        let text = drain_to_response(&mut session).await?;
        assert!(
            text.contains("retry answer"),
            "the client sees the RETRY's answer: {text}"
        );
        assert!(
            text.contains("auto-compacted") || text.contains("compact"),
            "and a compaction note: {text}"
        );
        Ok(())
    });
    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("timed out")
        .expect("connection failed");

    let requests = m.requests();
    assert_eq!(
        requests.len(),
        4,
        "seed turn + overflow turn + compaction + retry = 4 LLM rounds"
    );
    // The retry request must carry the recovery summary, not the span.
    assert!(
        body_contains(&requests[3], "RECOVERY SUMMARY"),
        "retry rides the compacted window"
    );
    // The transcript records the recovery compaction.
    let mut found = false;
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".jsonl")
            && let Ok(text) = std::fs::read_to_string(entry.path())
            && text.contains("RECOVERY SUMMARY")
        {
            found = true;
        }
    }
    assert!(found, "the recovery compaction persisted");
}
