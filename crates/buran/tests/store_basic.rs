//! Store contract: create/append/replay roundtrip, torn-tail tolerance,
//! append-after-torn-tail, unknown-record skip. All on tempfile (dev-dep).

use buran::llm::types::ChatMessage;
use buran::session::{TranscriptMeta, TranscriptRecord};
use buran::store::Store;

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

#[test]
fn create_then_append_then_replay_roundtrips_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();

    f.append(&TranscriptRecord::Message(ChatMessage::user("hi")))
        .unwrap();
    f.append(&TranscriptRecord::Message(ChatMessage::assistant("hello")))
        .unwrap();

    let (_, records) = store.open(f.id()).unwrap();
    assert_eq!(records.len(), 3, "head metadata + 2 messages");
    assert!(matches!(&records[0], TranscriptRecord::Metadata(m) if m.model == "test-model"));
    assert!(
        matches!(&records[1], TranscriptRecord::Message(m) if m.content.as_deref() == Some("hi"))
    );
    assert!(
        matches!(&records[2], TranscriptRecord::Message(m) if m.content.as_deref() == Some("hello"))
    );
}

#[test]
fn torn_final_line_is_a_warning_and_replay_stops_at_last_good_line() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();
    f.append(&TranscriptRecord::Message(ChatMessage::user("full")))
        .unwrap();

    // Simulate a crash mid-append: truncate the file mid-line.
    let path = dir.path().join(format!("{}.jsonl", f.id()));
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 4]).unwrap(); // cut into last record

    let (_, records) = store.open(f.id()).unwrap();
    assert_eq!(records.len(), 1, "head only — torn line dropped");
}

#[test]
fn append_after_torn_tail_produces_a_valid_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();
    f.append(&TranscriptRecord::Message(ChatMessage::user("one")))
        .unwrap();

    let path = dir.path().join(format!("{}.jsonl", f.id()));
    std::fs::write(&path, {
        let mut b = std::fs::read(&path).unwrap();
        b.extend_from_slice(b"{\"type\":\"msg"); // torn tail, no newline
        b
    })
    .unwrap();

    // Re-open: replay stops at the torn line; the append handle must
    // position itself at a line boundary so the next append is clean.
    let (mut f2, records) = store.open(f.id()).unwrap();
    assert_eq!(
        records.len(),
        2,
        "head + one survive — only the torn fragment dropped"
    );
    f2.append(&TranscriptRecord::Message(ChatMessage::user("two")))
        .unwrap();

    let (_, records) = store.open(f.id()).unwrap();
    assert_eq!(
        records.len(),
        3,
        "head + one + two — no torn fragment remains"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'), "file must stay newline-terminated");
}

#[test]
fn unknown_record_types_are_skipped_on_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let f = store.create(meta()).unwrap();
    // Hand-write a line with an unknown `type` (forward compat, ADR 0007).
    use std::io::Write;
    let path = dir.path().join(format!("{}.jsonl", f.id()));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, r#"{{"type":"from_the_future","x":1}}"#).unwrap();
    writeln!(
        file,
        r#"{{"type":"message","role":"user","content":"after"}}"#
    )
    .unwrap();
    drop(file);

    let (_, records) = store.open(f.id()).unwrap();
    assert_eq!(records.len(), 2, "unknown line skipped, message kept");
    assert!(
        matches!(&records[1], TranscriptRecord::Message(m) if m.content.as_deref() == Some("after"))
    );
}

#[test]
fn fork_never_touches_the_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();
    f.append(&TranscriptRecord::Message(ChatMessage::user("one")))
        .unwrap();

    // A torn tail on the source must not be "repaired" by a fork —
    // the original file is never opened for writing (brief contract),
    // so its bytes survive byte-for-byte even though `open` would
    // truncate the same tail.
    let path = dir.path().join(format!("{}.jsonl", f.id()));
    std::fs::write(&path, {
        let mut b = std::fs::read(&path).unwrap();
        b.extend_from_slice(b"{\"type\":\"msg"); // torn tail, no newline
        b
    })
    .unwrap();
    let original = std::fs::read(&path).unwrap();

    let forked = store.fork(f.id(), meta()).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "fork must leave the source file byte-for-byte identical"
    );
    // The fork itself got the surviving records (head + "one").
    let (_, records) = store.open(forked.id()).unwrap();
    assert_eq!(
        records.len(),
        2,
        "fork carries head + surviving body records"
    );
    assert!(matches!(
        &records[1],
        TranscriptRecord::Message(m) if m.content.as_deref() == Some("one")
    ));
}

#[test]
fn list_survives_a_corrupt_body_line() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();
    f.append(&TranscriptRecord::Message(ChatMessage::user("keep")))
        .unwrap();
    // Hand-write a corrupt line into the BODY (parseable as nothing,
    // has no `type` key, not last — a hard Corrupt for `open`).
    use std::io::Write;
    let path = dir.path().join(format!("{}.jsonl", f.id()));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, r#"{{"nonsense":true}}"#).unwrap();
    writeln!(
        file,
        r#"{{"type":"message","role":"user","content":"after"}}"#
    )
    .unwrap();
    drop(file);

    // open() correctly refuses the file…
    assert!(
        store.open(f.id()).is_err(),
        "corrupt mid-file line is fatal for replay"
    );
    // …but the catalog still lists the session from its head line.
    let sessions = store.list().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "corrupt body must not drop the session from list()"
    );
    assert_eq!(sessions[0].id, f.id());
    assert_eq!(sessions[0].meta.model, "test-model");
}
