//! Session transcript types: the record model for persisted conversations.
//!
//! Storage itself is deferred to v1.5 (spec open question 4); these types
//! pin the wire shape now so sessions can later be written as canonical
//! JSONL — one [`TranscriptRecord`] per line — without redesign.

use serde::{Deserialize, Serialize};

use crate::llm::types::ChatMessage;

/// One line of a persisted session transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptRecord {
    /// A conversation message (user, assistant, or tool result).
    Message(ChatMessage),
    /// A marker that everything before `at_message` was compacted into
    /// `summary` (v1.5).
    CompactionPoint {
        at_message: usize,
        summary: Option<String>,
    },
    /// Session bookkeeping, written once at the head of the transcript.
    Metadata(TranscriptMeta),
    /// Token usage observed at the end of one LLM turn (v1.5.1 hardening:
    /// record only — no presentation yet). Unknown to pre-1.5.1 binaries,
    /// which skip it on replay per ADR 0007.
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    },
}

/// Session bookkeeping: what ran, where, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptMeta {
    pub model: String,
    pub provider: String,
    /// RFC 3339 timestamp of session start.
    pub started_at: String,
    /// RFC 3339 timestamp of session end. Never written in v1.5 (would
    /// rewrite the append-only head); kept for wire compatibility.
    pub ended_at: Option<String>,
    /// Working directory the session was created in. `session/loadSession`
    /// requires the request cwd to match (sessions store spec).
    pub cwd: std::path::PathBuf,
    /// Session this one forked from, when `--fork` created it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}
