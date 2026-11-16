//! BuiltinSummarizer (ADR 0006): the ONE LLM seam of compaction. The
//! summary call carries the span as chat messages, the output budget as
//! a system instruction, and optional user focus instructions. Failures
//! propagate — all-or-nothing, no fake summaries.

use buran::compact::summarizer::{BuiltinSummarizer, Summarizer};
use buran::config::{ApiKey, ProviderConfig};
use buran::llm::LlmClient;
use buran::llm::types::ChatMessage;
use hclient::mock::MockTransport;
use hclient::{Client, Error as HclientError, ErrorKind};

fn provider() -> ProviderConfig {
    ProviderConfig {
        name: "test".into(),
        base_url: "http://mock.test/v1".into(),
        model: "summarizer-model".into(),
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

fn chunk(text: &str) -> String {
    format!(r#"data: {{"choices":[{{"delta":{{"content":"{text}"}}}}]}}"#)
}

fn done() -> &'static str {
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
}

fn body_contains(req: &hclient::mock::RecordedRequest, needle: &str) -> bool {
    req.body
        .text()
        .map(|text| text.contains(needle))
        .unwrap_or(false)
}

#[tokio::test]
async fn summarizer_returns_model_text_and_sends_span_budget_and_focus() {
    let m = MockTransport::new();
    // Frame assembly matches the working ACP tests: JSON + "\n\n" +
    // stop-frame + [DONE] in ONE data string per frame.
    let body = format!("{}\n\n{}", chunk("the span discussed X"), done());
    m.push_response_frames(sse(vec![Box::leak(body.into_boxed_str())]));
    let llm = LlmClient::with_transport(provider(), Client::builder(m.clone()).build().unwrap());
    let summarizer = BuiltinSummarizer::new(llm);

    let span = vec![
        ChatMessage::user("remember CAKE"),
        ChatMessage::assistant("cake noted"),
    ];
    let summary = summarizer
        .summarize(&span, Some("focus on credentials"), 400)
        .await
        .expect("summary");

    assert_eq!(summary, "the span discussed X");

    let requests = m.requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert!(body_contains(req, "remember CAKE"), "span reaches the LLM");
    assert!(body_contains(req, "cake noted"), "assistant span too");
    assert!(body_contains(req, "400"), "output budget on the wire");
    assert!(body_contains(req, "focus on credentials"), "user focus");
    assert!(
        body_contains(req, "summarizer-model"),
        "the summarizer's own model"
    );
}

#[tokio::test]
async fn summarizer_failure_propagates_no_fake_summary() {
    let m = MockTransport::new();
    let partial = chunk("partial");
    let error = HclientError::new(ErrorKind::Other, std::io::Error::other("connection reset"));
    m.push_response_frames_then_error(sse(vec![Box::leak(partial.into_boxed_str())]), error);
    let llm = LlmClient::with_transport(provider(), Client::builder(m).build().unwrap());
    let summarizer = BuiltinSummarizer::new(llm);
    let span = vec![ChatMessage::user("q")];
    let result = summarizer.summarize(&span, None, 100).await;
    assert!(
        result.is_err(),
        "an LLM failure must surface, not summarize"
    );
}

#[test]
fn builtin_summarizer_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BuiltinSummarizer>();
}
