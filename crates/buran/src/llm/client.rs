//! LLM streaming client: POST to an OpenAI-compatible endpoint, read the SSE
//! stream.
//!
//! hclient's `Client::sse()` is GET-only, so the POST + SSE path is manual:
//! `client.post(..).bearer_auth(..).json(req).send()`, status checked first
//! (an API error body is plain JSON, not SSE), then the response handed to
//! `hclient::sse::SseStream` — which validates status and content-type
//! eagerly, so it must be constructed only after the status check passes.

use std::collections::VecDeque;

use hclient::sse::{DEFAULT_MAX_EVENT_SIZE, SseEvent, SseStream};

use super::types::{ApiErrorBody, ChatChunk, ChatRequest, FinishReason, Usage};
use crate::config::ProviderConfig;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },
    #[error("LLM request failed: {0}")]
    Http(#[from] hclient::Error),
    #[error("failed to parse API response: {0}")]
    Json(#[from] serde_json::Error),
}

/// Events yielded by a [`ChatStream`].
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    /// Assistant text content fragment.
    Delta(String),
    /// One incremental piece of a tool call; fragments accumulate by `index`.
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name_fragment: Option<String>,
        args_fragment: Option<String>,
    },
    /// The stream signalled `[DONE]`; carries the last observed
    /// `finish_reason`, if any chunk provided one.
    Done(Option<FinishReason>),
}

/// A live streaming chat completion.
#[derive(Debug)]
pub struct ChatStream {
    sse: SseStream<hclient::body::ClientBody>,
    last_finish_reason: Option<FinishReason>,
    /// Last observed `usage` chunk (hardening v1.5.1): OpenAI-compatible
    /// endpoints send it on the final chunk; "last wins" across chunks.
    last_usage: Option<Usage>,
    done: bool,
    /// Tool-call deltas parsed from a chunk but not yet yielded: `next()`
    /// returns one event per call, and one chunk may carry several.
    pending: VecDeque<ChatEvent>,
}

impl ChatStream {
    /// The last observed `usage` chunk, if any (hardening v1.5.1). Safe to
    /// call after the stream is drained; `None` when the endpoint sent no
    /// usage (no record gets written).
    pub fn last_usage(&self) -> Option<Usage> {
        self.last_usage
    }

    /// Next event, or `None` once the stream has finished (`[DONE]` seen,
    /// or the connection closed). [`ChatEvent::Done`] is delivered exactly
    /// once; every call after it returns `None`.
    pub async fn next(&mut self) -> Result<Option<ChatEvent>, LlmError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        if self.done {
            return Ok(None);
        }
        loop {
            let event = match self.sse.next().await {
                None => {
                    // Connection closed without [DONE]. The loop still has
                    // whatever fragments it accumulated; surface a Done with
                    // the last finish reason (a truncated turn is surfaced by
                    // the missing [DONE] in logs, and Stop without a final
                    // chunk is the graceful reading).
                    tracing::warn!("SSE stream ended without [DONE]");
                    self.done = true;
                    return Ok(Some(ChatEvent::Done(self.last_finish_reason)));
                }
                Some(Err(source)) => return Err(source.into()),
                Some(Ok(event)) => event,
            };
            match event {
                SseEvent::Message { data, .. } => {
                    if data == "[DONE]" {
                        self.done = true;
                        return Ok(Some(ChatEvent::Done(self.last_finish_reason)));
                    }
                    let chunk: ChatChunk = serde_json::from_str(&data)?;
                    if let Some(usage) = chunk.usage {
                        self.last_usage = Some(usage);
                    }
                    for choice in chunk.choices {
                        if choice.finish_reason.is_some() {
                            self.last_finish_reason = choice.finish_reason;
                        }
                        // A chunk may carry BOTH text and tool-call
                        // fragments (coalescing gateways do this): buffer
                        // the tool fragments FIRST, then queue the text
                        // behind them — an early `return` here would drop
                        // the rest of the chunk (the silent-loss bug the
                        // final review caught).
                        for tc in choice.delta.tool_calls.into_iter().flatten() {
                            let (name_fragment, args_fragment) = match tc.function {
                                Some(f) => (f.name, f.arguments),
                                None => (None, None),
                            };
                            self.pending.push_back(ChatEvent::ToolCallDelta {
                                index: tc.index,
                                id: tc.id,
                                name_fragment,
                                args_fragment,
                            });
                        }
                        if let Some(content) = choice.delta.content.filter(|c| !c.is_empty()) {
                            self.pending.push_back(ChatEvent::Delta(content));
                        }
                        // Yield one buffered event before reading more
                        // wire data; empty when this choice had none.
                        if let Some(event) = self.pending.pop_front() {
                            return Ok(Some(event));
                        }
                    }
                    // Chunk carried nothing user-visible (e.g. usage-only);
                    // keep reading.
                }
                SseEvent::Comment(_) | SseEvent::Retry(_) => {
                    // Keep-alive comments and retry hints are not content.
                }
            }
        }
    }
}

/// Client for one configured provider.
pub struct LlmClient {
    client: hclient::Client,
    provider: ProviderConfig,
}

impl LlmClient {
    /// Builds a client for the provider, resolving its API key.
    pub fn new(provider: &ProviderConfig) -> Result<Self, LlmError> {
        Ok(Self {
            client: hclient::Client::new()?,
            provider: provider.clone(),
        })
    }

    /// Test seam: build around an already-constructed client (mock in tests).
    pub fn with_transport(provider: ProviderConfig, client: hclient::Client) -> Self {
        Self { client, provider }
    }

    fn api_key(&self) -> String {
        // FromEnv stores the variable's NAME; resolve reads the process env.
        // A missing variable yields an empty key: the API answers 401 with
        // its own message, which surfaces through LlmError::Api.
        self.provider.api_key.resolve().unwrap_or_default()
    }

    /// The model name from the provider config; requests carry it.
    pub fn model_name(&self) -> &str {
        &self.provider.model
    }

    /// POSTs the request and returns the live SSE stream.
    ///
    /// Non-2xx responses are read and parsed as an OpenAI error body; the
    /// status is checked *before* `SseStream::new`, which consumes the
    /// response and rejects non-SSE responses itself.
    pub async fn stream_chat<'a>(&'a self, req: &'a ChatRequest) -> Result<ChatStream, LlmError> {
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.api_key())
            .json(req)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.collect().await?.text()?;
            let message = serde_json::from_str::<ApiErrorBody>(&text)
                .map(|body| body.error.message)
                .unwrap_or_else(|_| text.trim().to_string());
            return Err(LlmError::Api {
                status: status.as_u16(),
                message,
            });
        }

        let sse = SseStream::new(resp, DEFAULT_MAX_EVENT_SIZE)?;
        Ok(ChatStream {
            sse,
            last_finish_reason: None,
            last_usage: None,
            done: false,
            pending: VecDeque::new(),
        })
    }
}
