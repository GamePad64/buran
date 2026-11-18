//! Session titles: the first user message is a session's human-readable
//! identity (picker + `sessions list`). Derivation normalizes that
//! message — one line, collapsed whitespace, ellipsis cap — and the
//! catalog carries it so the CLI list and the TUI picker read the same
//! rule. Scan tolerates damage by design (ADR 0007): a corrupt or torn
//! line ends the scan and yields whatever was found before it; listing
//! itself never fails over body damage.

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
        subagent_of: None,
    }
}

#[test]
fn list_carries_first_user_message_as_title() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut s = store.create(meta("/w")).unwrap();
    // An assistant greeting is not a title; the first USER line is.
    s.append(&TranscriptRecord::Message(ChatMessage::assistant("hello!")))
        .unwrap();
    s.append(&TranscriptRecord::Message(ChatMessage::user(
        "  fix the\n\tlogin bug  ",
    )))
    .unwrap();
    s.append(&TranscriptRecord::Message(ChatMessage::user("later ask")))
        .unwrap();

    let infos = store.list().unwrap();
    assert_eq!(infos.len(), 1);
    assert_eq!(
        infos[0].title.as_deref(),
        Some("fix the login bug"),
        "first user message, whitespace-collapsed to one line"
    );
}

#[test]
fn empty_session_has_no_title() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    store.create(meta("/w")).unwrap();
    let infos = store.list().unwrap();
    assert_eq!(infos[0].title, None, "no user message, no title");
}

#[test]
fn title_skips_user_message_without_content_and_keeps_scanning() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut s = store.create(meta("/w")).unwrap();
    s.append(&TranscriptRecord::Message(ChatMessage {
        role: buran::llm::types::Role::User,
        content: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
    }))
    .unwrap();
    s.append(&TranscriptRecord::Message(ChatMessage::user("real ask")))
        .unwrap();
    let infos = store.list().unwrap();
    assert_eq!(infos[0].title.as_deref(), Some("real ask"));
}

#[test]
fn title_caps_at_max_chars_with_ellipsis() {
    assert_eq!(
        buran::store::session_title("short", 40),
        "short",
        "under the cap stays verbatim"
    );
    let long = "a".repeat(100);
    let titled = buran::store::session_title(&long, 40);
    assert_eq!(titled.chars().count(), 40, "cap includes the ellipsis");
    assert!(titled.ends_with('…'), "ellipsis marks the cut");
    assert_eq!(&titled[..titled.len() - '…'.len_utf8()], &long[..39]);
}

#[test]
fn title_scan_stops_at_corrupt_body_without_failing_the_listing() {
    // The catalog contract (list_survives_a_corrupt_body_line) extends to
    // titles: body damage ends the scan, head + id still list.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let store = Store::new(root);
    let mut s = store.create(meta("/w")).unwrap();
    s.append(&TranscriptRecord::Message(ChatMessage::user("usable ask")))
        .unwrap();
    let path = root.join(format!("{}.jsonl", s.id()));
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"type\":\"message\",\"role\":\"user\",BROKEN\n");
    std::fs::write(&path, text).unwrap();

    let infos = store.list().unwrap();
    assert_eq!(infos[0].title.as_deref(), Some("usable ask"));
}

#[test]
fn title_falls_back_to_nothing_when_only_corrupt_body() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let store = Store::new(root);
    let s = store.create(meta("/w")).unwrap();
    let path = root.join(format!("{}.jsonl", s.id()));
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("not json at all\n");
    std::fs::write(&path, text).unwrap();
    let infos = store.list().unwrap();
    assert_eq!(infos[0].title, None, "damage yields no title, not an error");
}

#[test]
fn subagent_of_marker_flows_through_head_list_and_old_files() {
    // A child transcript head carries subagent_of; list surfaces it.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut child_meta = meta("/w");
    child_meta.subagent_of = Some("parent-1234".into());
    store.create(child_meta).unwrap();

    let infos = store.list().unwrap();
    assert_eq!(
        infos[0].subagent_of.as_deref(),
        Some("parent-1234"),
        "the marker reaches the catalog"
    );

    // A normal session has no marker; the child row keeps its own.
    store.create(meta("/w")).unwrap();
    let infos = store.list().unwrap();
    let normal = infos
        .iter()
        .find(|i| i.subagent_of.is_none())
        .expect("normal row");
    assert!(normal.title.is_none(), "the normal row is the empty one");
    let child = infos
        .iter()
        .find(|i| i.subagent_of.is_some())
        .expect("child row");
    assert_eq!(child.subagent_of.as_deref(), Some("parent-1234"));

    // A PRE-v2 transcript (no field) loads fine: serde default.
    let legacy = dir
        .path()
        .join("01940000-0000-7000-8000-000000000099.jsonl");
    std::fs::write(
        &legacy,
        r#"{"type":"metadata","model":"m","provider":"p","started_at":"2026-11-18T00:00:00Z","ended_at":null,"cwd":"/w"}"#,
    )
    .unwrap();
    let store2 = Store::new(dir.path());
    let infos = store2.list().unwrap();
    let legacy_row = infos.iter().find(|i| i.id.ends_with("099")).unwrap();
    assert_eq!(legacy_row.subagent_of, None, "old head parses, no marker");
}
