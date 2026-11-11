//! OpenAI-compatible chat completions wire types.
//!
//! Shapes follow the de-facto OpenAI `/chat/completions` streaming format
//! that every OpenAI-compatible endpoint speaks (spec: providers are config,
//! not code). `tool_calls.function.arguments` is a JSON *string* on the wire
//! and is parsed with `serde_json::from_str` by the consumer.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool call requested by the assistant. `arguments` is a JSON string per
/// the wire format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallReq {
    pub id: String,
    #[serde(rename = "type")]
    pub ty: FunctionType,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FunctionType {
    #[serde(rename = "function")]
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolDefFn {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub ty: FunctionType,
    pub function: ToolDefFn,
}

/// One message of the conversation. Assistant messages either carry
/// `content`, or `tool_calls` with `content: None`; tool results are
/// `role: tool` messages with `tool_call_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallReq>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Assistant turn that requested tools instead of speaking.
    pub fn assistant_with_tool_calls(tool_calls: Vec<ToolCallReq>) -> Self {
        Self {
            role: Role::Assistant,
            content: None,
            tool_calls,
            tool_call_id: None,
        }
    }

    /// A tool result fed back to the model.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: BoolTrue,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
}

/// Always serializes as `true`: buran only speaks streaming chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoolTrue;

impl Serialize for BoolTrue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bool(true)
    }
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            stream: BoolTrue,
            tools: None,
            tool_choice: None,
        }
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum FinishReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "tool_calls")]
    ToolCalls,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "content_filter")]
    ContentFilter,
    /// Loop-internal: max_turns exhausted without a natural stop. Never
    /// parsed from the wire — an API never sends this name; run_turn
    /// synthesizes it so callers see a cut loop, not a clean end.
    MaxTurnRequests,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeltaFn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

/// One incremental piece of a tool call the model is emitting.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeltaToolCall {
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<DeltaFn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChunkChoice {
    pub delta: Delta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// One streamed `data:` line of the chat completions stream.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ChatChunk {
    pub choices: Vec<ChunkChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// Non-200 error body: `{"error": {"message": ..., "type": ..., "code": ...}}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApiErrorBody {
    pub error: ApiError,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ApiError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<serde_json::Value>,
}
