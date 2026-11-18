//! ACP-over-HTTP integration: `serve --listen` on a real loopback port,
//! driven by hclient (the crate's own HTTP client). The ACP leg is real
//! HTTP/1.1; the LLM leg stays `hclient-mock` — no external server, no
//! second HTTP stack (AGENTS.md).

use std::time::Duration;

use buran::config::{ApiKey, ProviderConfig};
use buran::llm::LlmClient;
use hclient::mock::MockTransport;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while waiting for the listener to accept.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

fn provider() -> ProviderConfig {
    ProviderConfig {
        name: "test-provider".into(),
        base_url: "http://mock.test/v1".into(),
        model: "test-model".into(),
        api_key: ApiKey::Explicit("sk-test".into()),
        context_window: None,
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
        text_chunk(" over http"),
        stop_and_done()
    )
}

/// Test config: mock provider, no MCP servers, Ask policy — mirrors the
/// stdio integration suite's `test_config`.
fn test_config() -> buran::config::Config {
    buran::config::Config {
        provider: Some(provider()),
        mcp_servers: Vec::new(),
        skills_dirs: Vec::new(),
        permission: buran::config::PermissionPolicy::Ask,
        permission_timeout: Duration::from_secs(2),
        sessions_dir: None,
        compaction: Default::default(),
        subagents: Default::default(),
        subagent_parent: None,
    }
}

/// Spawn `serve --listen` on an ephemeral loopback port with a
/// mock-backed LLM; returns the bound address AND the shared
/// MockTransport (for request introspection). The listener task runs on
/// the test's runtime and is dropped when the test ends.
async fn spawn_listen() -> (std::net::SocketAddr, MockTransport) {
    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(hello_body().into_boxed_str())]));
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let m2 = m.clone();
    let bound = buran::acp::serve_listen_with(addr, test_config(), move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("serve_listen binds and spawns");
    (bound, m)
}

/// Real-HTTP hclient against the loopback listener (LLM stays mocked).
fn http_client() -> hclient::Client {
    hclient::Client::builder(hclient::default_transport().expect("default transport"))
        .build()
        .expect("client builds")
}

/// Wait until POST /acp answers (the listener task races the test).
async fn wait_accepting(client: &hclient::Client, base: &str) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "listener never came up"
        );
        let probe = client
            .post(format!("{base}/acp"))
            .json(&json!({
                "jsonrpc": "2.0", "id": 0,
                "method": "initialize",
                "params": {"protocolVersion": 1, "clientCapabilities": {}}
            }))
            .send()
            .await;
        if probe.is_ok() {
            // Listener is up; this probe also opened connection #1. Done.
            return;
        }
        tokio::time::sleep(ACCEPT_POLL).await;
    }
}

/// Open the connection's SSE stream, returning the concrete stream type
/// (default client body). The stream lives as long as the returned
/// value; dropping it closes the GET.
async fn open_sse(
    client: &hclient::Client,
    base: &str,
    conn_id: &str,
) -> hclient::sse::SseStream<hclient::body::ClientBody> {
    let resp = client
        .get(format!("{base}/acp"))
        .header("accept", "text/event-stream")
        .header("acp-connection-id", conn_id)
        .send()
        .await
        .expect("SSE stream opens");
    assert!(resp.status().is_success(), "SSE status: {}", resp.status());
    hclient::sse::SseStream::new(resp, hclient::sse::DEFAULT_MAX_EVENT_SIZE)
        .expect("SSE stream opens")
}

/// Next JSON-RPC payload off the stream (skipping comments); the async
/// block so `timeout(..)` can wrap it. Panics on decode errors — a test
/// wants loud failures.
macro_rules! next_payload {
    ($stream:expr) => {{
        async {
            loop {
                match $stream.next().await {
                    Some(Ok(hclient::sse::SseEvent::Message { data, .. })) => {
                        break Some(
                            serde_json::from_str::<Value>(&data).expect("SSE data is JSON"),
                        );
                    }
                    Some(Ok(_)) => continue, // comment or other non-message event
                    Some(Err(e)) => panic!("SSE error: {e}"),
                    None => break None,
                }
            }
        }
    }};
}

/// initialize → Acp-Connection-Id → newSession → prompt: the whole
/// roundtrip over real HTTP/1.1. Per the transport spec, only the first
/// initialize POST carries a JSON body; every later POST answers `202
/// Accepted` and its response arrives on the connection's SSE stream.
#[tokio::test(flavor = "multi_thread")]
async fn http_initialize_new_session_prompt_roundtrip() {
    let (addr, mock) = spawn_listen().await;
    let base = format!("http://{addr}");
    let client = http_client();

    // 1. initialize over POST; the response body + connection id header.
    wait_accepting(&client, &base).await;
    let init_resp = client
        .post(format!("{base}/acp"))
        .json(&json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }))
        .send()
        .await
        .expect("initialize POST reaches the agent");
    assert!(init_resp.status().is_success(), "initialize status");
    let conn_id = init_resp
        .headers()
        .get("acp-connection-id")
        .expect("Acp-Connection-Id header")
        .to_str()
        .unwrap()
        .to_string();
    let init_body: Value =
        serde_json::from_str(&init_resp.collect().await.unwrap().text().unwrap())
            .expect("initialize response is JSON");
    assert_eq!(init_body["id"], 1);
    assert!(
        init_body["result"]["protocolVersion"].as_i64().is_some(),
        "initialize result carries protocolVersion"
    );

    // 2. Open the SSE stream for this connection BEFORE posting more
    // requests — everything from here on answers over this stream.
    let mut stream = open_sse(&client, &base, &conn_id).await;

    // 3. newSession: 202 Accepted, response comes over SSE.
    let sess_resp = client
        .post(format!("{base}/acp"))
        .header("acp-connection-id", &conn_id)
        .json(&json!({
            "jsonrpc": "2.0", "id": 2,
            "method": "session/new",
            "params": {"cwd": "/tmp", "mcpServers": []}
        }))
        .send()
        .await
        .expect("session/new POST");
    assert_eq!(sess_resp.status().as_u16(), 202, "session/new accepted");

    // 4. Read the stream until the session/new response (id 2) lands and
    // gives the real sessionId — prompt needs it.
    let mut payloads = Vec::new();
    let session_id = loop {
        let v = tokio::time::timeout(TIMEOUT, next_payload!(stream))
            .await
            .expect("session/new response arrives in time")
            .expect("SSE stream still open");
        let done = v.get("id").and_then(Value::as_i64) == Some(2);
        payloads.push(v);
        if done {
            break payloads.last().unwrap()["result"]["sessionId"]
                .as_str()
                .expect("sessionId in result")
                .to_string();
        }
    };
    assert!(!session_id.is_empty());

    // 5. Session-scoped traffic routes to a per-session SSE stream: open
    // it with the session header before prompting.
    let mut session_stream = {
        let resp = client
            .get(format!("{base}/acp"))
            .header("accept", "text/event-stream")
            .header("acp-connection-id", &conn_id)
            .header("acp-session-id", &session_id)
            .send()
            .await
            .expect("session SSE stream opens");
        assert!(resp.status().is_success(), "session SSE status");
        hclient::sse::SseStream::new(resp, hclient::sse::DEFAULT_MAX_EVENT_SIZE)
            .expect("session SSE stream opens")
    };

    // 6. prompt: 202 Accepted; notifications + the response stream on
    // the session stream.
    let prompt_resp = client
        .post(format!("{base}/acp"))
        .header("acp-connection-id", &conn_id)
        .json(&json!({
            "jsonrpc": "2.0", "id": 3,
            "method": "session/prompt",
            "params": {"sessionId": session_id,
                       "prompt": [{"type": "text", "text": "hi"}]}
        }))
        .send()
        .await
        .expect("session/prompt POST");
    assert_eq!(prompt_resp.status().as_u16(), 202, "prompt accepted");

    let mut updates: Vec<String> = Vec::new();
    let prompt_body = loop {
        let v = match tokio::time::timeout(TIMEOUT, next_payload!(session_stream)).await {
            Ok(v) => v.expect("session SSE stream still open"),
            Err(_) => panic!(
                "prompt response timed out; LLM requests seen: {}",
                mock.requests().len()
            ),
        };
        if v["method"] == "session/update" {
            if let Some(u) = v["params"]["update"]["sessionUpdate"].as_str() {
                updates.push(u.to_string());
            }
            continue;
        }
        if v.get("id").and_then(Value::as_i64) == Some(3) {
            break v;
        }
        payloads.push(v);
    };
    assert_eq!(
        prompt_body["result"]["stopReason"], "end_turn",
        "prompt response payload: {prompt_body}"
    );
    assert!(
        updates.iter().any(|u| u == "agent_message_chunk"),
        "text streamed as agent_message_chunk: {updates:?}"
    );
}

/// `GET /health` answers 200 (crate default, ADR 0010 keeps it).
#[tokio::test(flavor = "multi_thread")]
async fn http_health_endpoint_answers() {
    let (addr, _mock) = spawn_listen().await;
    let client = http_client();
    wait_accepting(&client, &format!("http://{addr}")).await;
    let resp = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .expect("health GET");
    assert!(
        resp.status().is_success(),
        "health status: {}",
        resp.status()
    );
}
