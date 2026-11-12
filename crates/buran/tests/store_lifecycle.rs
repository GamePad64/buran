//! Lifecycle: chronological listing, fork isolation + lineage, latest,
//! remove. Fork MUST never modify the original (spec Review Focus #3).

use buran::llm::types::ChatMessage;
use buran::session::{TranscriptMeta, TranscriptRecord};
use buran::store::Store;

fn meta(cwd: &str) -> TranscriptMeta {
    TranscriptMeta {
        model: "m".into(),
        provider: "p".into(),
        started_at: "2026-11-12T00:00:00Z".into(),
        ended_at: None,
        cwd: cwd.into(),
        forked_from: None,
    }
}

fn bytes_of(dir: &tempfile::TempDir, id: &str) -> Vec<u8> {
    std::fs::read(dir.path().join(format!("{id}.jsonl"))).unwrap()
}

#[test]
fn list_is_chronological_and_carries_head_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let a = store.create(meta("/a")).unwrap();
    let b = store.create(meta("/b")).unwrap();

    let infos = store.list().unwrap();
    assert_eq!(infos.len(), 2);
    assert_eq!(infos[0].id, a.id(), "uuidv7 creation order == sort order");
    assert_eq!(infos[1].id, b.id());
    assert_eq!(infos[0].meta.cwd, std::path::Path::new("/a"));
}

#[test]
fn fork_copies_records_never_touches_the_original_and_records_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut orig = store.create(meta("/proj")).unwrap();
    orig.append(&TranscriptRecord::Message(ChatMessage::user("q")))
        .unwrap();
    orig.append(&TranscriptRecord::Message(ChatMessage::assistant("a")))
        .unwrap();

    let original_bytes = bytes_of(&dir, orig.id());

    let mut fork = store.fork(orig.id(), meta("/proj")).unwrap();
    fork.append(&TranscriptRecord::Message(ChatMessage::user("branch q")))
        .unwrap();

    // Original byte-identical after fork + fork-file appends.
    assert_eq!(
        original_bytes,
        bytes_of(&dir, orig.id()),
        "original untouched"
    );

    let (_, fork_records) = store.open(fork.id()).unwrap();
    assert_eq!(fork_records.len(), 4, "fork head + q + a + branch q");
    assert!(
        matches!(&fork_records[0], TranscriptRecord::Metadata(m)
            if m.forked_from.as_deref() == Some(orig.id())),
        "lineage recorded"
    );

    let (_, orig_records) = store.open(orig.id()).unwrap();
    assert_eq!(orig_records.len(), 3, "original replay unchanged");
}

#[test]
fn latest_is_the_max_uuidv7_filename() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    assert!(store.latest().unwrap().is_none(), "empty store");
    let first = store.create(meta("/")).unwrap();
    let second = store.create(meta("/")).unwrap();
    assert_eq!(store.latest().unwrap().as_deref(), Some(second.id()));
    let _ = first;
}

#[test]
fn remove_deletes_the_file_and_notfound_on_repeat() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let f = store.create(meta("/")).unwrap();
    store.remove(f.id()).unwrap();
    assert!(matches!(store.remove(f.id()),
        Err(buran::store::StoreError::NotFound(id)) if id == f.id()));
}

#[test]
fn fork_of_a_corrupt_source_leaves_no_orphan_session_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut orig = store.create(meta("/")).unwrap();
    orig.append(&TranscriptRecord::Message(ChatMessage::user("keep")))
        .unwrap();
    // Corrupt line followed by a valid one — fatal mid-file corruption
    // for replay (a torn tail alone would be tolerated instead).
    use std::io::Write;
    let path = dir.path().join(format!("{}.jsonl", orig.id()));
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

    assert!(
        store.fork(orig.id(), meta("/")).is_err(),
        "corrupt source refused"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "no orphan fork file — replay must run before create"
    );
}
