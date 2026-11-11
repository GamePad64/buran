//! LLM client against hclient's scripted mock transport — no real HTTP.
//!
//! hclient needs an ambient tokio runtime at request time (`tokio::spawn`
//! internally), so these tests run under `#[tokio::test]` even though the
//! transport itself is fully scripted.

use buran::config::{ApiKey, ProviderConfig};
use buran::llm::types::ChatRequest;
use buran::llm::{ChatEvent, LlmClient};

fn provider() -> ProviderConfig {
    ProviderConfig {
        name: "test-provider".into(),
        base_url: "http://mock.test/v1".into(),
        model: "test-model".into(),
        api_key: ApiKey::Explicit("sk-test".into()),
    }
}

/// One SSE frame stream; deltas split across frame boundary (hclient stitches).
fn sse_body() -> Vec<&'static str> {
    vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    ]
}

#[tokio::test]
async fn streams_deltas_then_done_over_mock() {
    let m = hclient::mock::MockTransport::new();
    m.push_response_frames(
        http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body())
            .unwrap(),
    );
    let llm = LlmClient::with_transport(
        provider(),
        hclient::Client::builder(m.clone()).build().unwrap(),
    );
    let req = ChatRequest::new("test-model", vec![]);
    let mut s = llm.stream_chat(&req).await.unwrap();
    let mut text = String::new();
    let mut done = false;
    while let Some(ev) = s.next().await.unwrap() {
        match ev {
            ChatEvent::Delta(t) => text.push_str(&t),
            ChatEvent::Done(_) => done = true,
            ChatEvent::ToolCallDelta { .. } => panic!("no tool calls expected"),
        }
    }
    assert_eq!(text, "Hello");
    assert!(done);
    let reqs = m.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "POST");
    let auth = reqs[0]
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(auth, "Bearer sk-test");
}

#[tokio::test]
async fn http_error_becomes_api_error() {
    let m = hclient::mock::MockTransport::new();
    m.push_response(
        http::Response::builder()
            .status(401)
            .header("content-type", "application/json")
            .body(r#"{"error":{"message":"bad key"}}"#)
            .unwrap(),
    );
    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let req = ChatRequest::new("test-model", vec![]);
    let err = llm.stream_chat(&req).await.unwrap_err();
    assert!(err.to_string().contains("bad key"), "{err}");
}

#[tokio::test]
async fn tool_call_deltas_stream_verbatim() {
    let m = hclient::mock::MockTransport::new();
    m.push_response_frames(
        http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(vec![
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"fs__read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\":\\\"x\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: [DONE]\n\n",
            ])
            .unwrap(),
    );
    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let req = ChatRequest::new("test-model", vec![]);
    let mut s = llm.stream_chat(&req).await.unwrap();
    let mut events = Vec::new();
    while let Some(ev) = s.next().await.unwrap() {
        events.push(ev);
    }
    assert_eq!(
        events,
        vec![
            ChatEvent::ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name_fragment: Some("fs__read".into()),
                args_fragment: Some("{\"pa".into()),
            },
            ChatEvent::ToolCallDelta {
                index: 0,
                id: None,
                name_fragment: None,
                args_fragment: Some("\":\"x\"}".into()),
            },
            ChatEvent::Done(Some(buran::llm::types::FinishReason::ToolCalls)),
        ]
    );
}
