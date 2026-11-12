//! Transcript record types: serde round-trips so v1.5 can serialize
//! sessions to canonical JSONL without redesign (spec open question 4).

use buran::llm::types::ChatMessage;
use buran::session::{TranscriptMeta, TranscriptRecord};

#[test]
fn message_record_round_trips() {
    let record = TranscriptRecord::Message(ChatMessage::user("hello world"));
    let json = serde_json::to_string(&record).unwrap();
    let back: TranscriptRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, record);
}

#[test]
fn compaction_point_record_round_trips() {
    let with_summary = TranscriptRecord::CompactionPoint {
        at_message: 42,
        summary: Some("earlier context compacted".into()),
    };
    let json = serde_json::to_string(&with_summary).unwrap();
    let back: TranscriptRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, with_summary);

    let without_summary = TranscriptRecord::CompactionPoint {
        at_message: 7,
        summary: None,
    };
    let json = serde_json::to_string(&without_summary).unwrap();
    let back: TranscriptRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, without_summary);
}

#[test]
fn metadata_record_round_trips() {
    let record = TranscriptRecord::Metadata(record_meta());
    let json = serde_json::to_string(&record).unwrap();
    let back: TranscriptRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, record);

    let ended = TranscriptRecord::Metadata(TranscriptMeta {
        ended_at: Some("2026-11-11T01:00:00Z".into()),
        ..record_meta()
    });
    let json = serde_json::to_string(&ended).unwrap();
    let back: TranscriptRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ended);
}

fn record_meta() -> TranscriptMeta {
    TranscriptMeta {
        model: "test-model".into(),
        provider: "test-provider".into(),
        started_at: "2026-11-11T00:00:00Z".into(),
        ended_at: None,
        cwd: std::path::PathBuf::from("/tmp/proj"),
        forked_from: None,
    }
}
