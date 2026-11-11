use buran::agentsmd::load_system_instructions;
use std::fs;

#[test]
fn reads_nearest_agents_md_first() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("sub");
    fs::create_dir_all(&sub).unwrap();
    fs::write(root.path().join("AGENTS.md"), "root layer\n").unwrap();
    fs::write(sub.join("AGENTS.md"), "leaf layer\n").unwrap();

    let instructions = load_system_instructions(Some(&sub));
    // Root-first layering (ADR 0003): general rules speak before specific ones.
    assert!(
        instructions.starts_with("root layer"),
        "root layer must come first: {instructions:?}"
    );
    assert!(
        instructions.contains("leaf layer"),
        "ancestors layer in: {instructions:?}"
    );
}

#[test]
fn child_inherits_parent_agents_md() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("a/b");
    fs::create_dir_all(&sub).unwrap();
    fs::write(root.path().join("AGENTS.md"), "parent rules\n").unwrap();

    let instructions = load_system_instructions(Some(&sub));
    assert_eq!(instructions.trim(), "parent rules");
}

#[test]
fn claude_md_alone_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("CLAUDE.md"), "claude rules\n").unwrap();
    assert_eq!(load_system_instructions(Some(dir.path())), "");
}

#[test]
fn no_project_dir_returns_empty() {
    assert_eq!(load_system_instructions(None), "");
}

#[test]
fn unreadable_file_is_skipped_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("x");
    fs::create_dir_all(&nested).unwrap();
    // AGENTS.md as a directory makes is_file() false: loader must skip it.
    fs::create_dir_all(dir.path().join("AGENTS.md")).unwrap();
    fs::write(nested.join("AGENTS.md"), "nested ok\n").unwrap();
    let instructions = load_system_instructions(Some(&nested));
    assert_eq!(instructions.trim(), "nested ok");
}

#[test]
fn empty_when_nothing_found_anywhere() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("deep/deeper");
    fs::create_dir_all(&sub).unwrap();
    assert_eq!(load_system_instructions(Some(&sub)), "");
}
