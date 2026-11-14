//! Store writer task: ordering, acks, failure surfacing, byte-identity
//! with direct `SessionFile::append` output.

use buran::llm::types::ChatMessage;
use buran::session::{TranscriptMeta, TranscriptRecord};
use buran::store::{Store, WriterHandle};

fn meta() -> TranscriptMeta {
    TranscriptMeta {
        model: "test-model".into(),
        provider: "test".into(),
        started_at: "2026-11-12T00:00:00Z".into(),
        ended_at: None,
        cwd: std::path::PathBuf::from("/tmp/proj"),
        forked_from: None,
    }
}

fn msg(n: usize) -> TranscriptRecord {
    TranscriptRecord::Message(ChatMessage::user(format!("m{n}")))
}

/// 10 appends through the writer land as 10 lines, in order.
#[tokio::test]
async fn ten_appends_land_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let id = {
        let file = store.create(meta()).unwrap();
        let id = file.id().to_string();
        let writer = file.spawn_writer();
        for n in 0..10 {
            writer.append(&msg(n)).expect("append queued");
        }
        drop(writer); // close the channel; the task drains and exits
        id
    };
    // Give the task a moment to drain; drop(writer) only closes the
    // channel, the final append may still be in flight.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let (_file, records) = store.open(&id).unwrap();
    assert_eq!(records.len(), 11, "head + 10 messages");
    for (i, record) in records.iter().enumerate().skip(1) {
        let expected = format!("m{}", i - 1);
        let TranscriptRecord::Message(message) = record else {
            panic!("record {i} must be a message: {record:?}");
        };
        assert_eq!(
            message.content.as_deref(),
            Some(expected.as_str()),
            "order must be preserved"
        );
    }
}

/// `append_acked` resolves once the record is on disk.
#[tokio::test]
async fn acked_append_resolves_ok() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let writer = store.create(meta()).unwrap().spawn_writer();

    writer.append_acked(&msg(1)).await.expect("acked ok");
    // The record is readable from another handle right now.
    let content = std::fs::read_to_string(dir.path().join(format!("{}.jsonl", {
        // find the single session id by listing
        let sessions = store.list().unwrap();
        assert_eq!(sessions.len(), 1);
        sessions[0].id.clone()
    })))
    .unwrap();
    assert_eq!(content.lines().count(), 2, "head + 1 acked message");
}

/// Byte-identity: the same record sequence through the writer produces a
/// transcript byte-for-byte equal to direct `SessionFile::append`.
#[tokio::test]
async fn writer_output_is_byte_identical_to_direct_append() {
    let sequence = |n: usize| -> Vec<TranscriptRecord> {
        (0..n)
            .map(msg)
            .chain(std::iter::once(TranscriptRecord::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            }))
            .collect()
    };

    let direct_dir = tempfile::tempdir().unwrap();
    let direct_store = Store::new(direct_dir.path());
    let mut direct = direct_store.create(meta()).unwrap();
    for record in &sequence(5) {
        direct.append(record).unwrap();
    }

    let writer_dir = tempfile::tempdir().unwrap();
    let writer_store = Store::new(writer_dir.path());
    let writer: WriterHandle = writer_store.create(meta()).unwrap().spawn_writer();
    for record in &sequence(5) {
        writer.append_acked(record).await.unwrap();
    }
    drop(writer);

    let direct_path = direct_dir
        .path()
        .join(format!("{}.jsonl", direct_store.list().unwrap()[0].id));
    let writer_path = writer_dir
        .path()
        .join(format!("{}.jsonl", writer_store.list().unwrap()[0].id));
    let direct_bytes = std::fs::read(&direct_path).unwrap();
    let writer_bytes = std::fs::read(&writer_path).unwrap();
    assert_eq!(
        direct_bytes, writer_bytes,
        "writer output must equal direct append byte-for-byte"
    );
}
