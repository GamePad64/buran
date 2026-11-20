//! Configurable instruction-file names + symlink dedup (three phases:
//! collect in tree order → canonicalize → dedup keeping first occurrence,
//! order preserved).

use buran::agentsmd::load_system_instructions_with;
use std::path::Path;

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

#[test]
fn default_names_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "AGENTS.md", "agents layer");
    let out = load_system_instructions_with(Some(dir.path()), &["AGENTS.md".into()]);
    assert_eq!(out, "agents layer");
}

#[test]
fn claude_md_loaded_when_configured() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "CLAUDE.md", "claude rules");
    let out =
        load_system_instructions_with(Some(dir.path()), &["AGENTS.md".into(), "CLAUDE.md".into()]);
    assert_eq!(out, "claude rules");
}

#[test]
fn config_order_within_a_directory_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "BB.md", "second");
    write(dir.path(), "AA.md", "first");
    let out = load_system_instructions_with(Some(dir.path()), &["BB.md".into(), "AA.md".into()]);
    assert_eq!(
        out, "second\n\nfirst",
        "config order wins, not alphabetical"
    );
}

#[test]
fn symlink_twin_is_deduped_first_occurrence_wins() {
    // Sub dir carries CLAUDE.md -> root AGENTS.md (the Claude Code project
    // shape). The symlink resolves to the root file: one load, at the SUB
    // position (nearest layer speaks for its directory).
    let root = tempfile::tempdir().unwrap();
    let sub = tempfile::tempdir_in(root.path()).unwrap();
    write(root.path(), "AGENTS.md", "root layer");
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("AGENTS.md"), sub.path().join("CLAUDE.md"))
        .unwrap();

    let out =
        load_system_instructions_with(Some(sub.path()), &["AGENTS.md".into(), "CLAUDE.md".into()]);
    assert_eq!(
        out, "root layer",
        "deduped: one load, at the symlink's (nearest) position"
    );
    assert_eq!(out.matches("root layer").count(), 1, "no duplicate load");
}

#[test]
fn broken_symlink_is_skipped_fail_open() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.path().join("missing.md"), dir.path().join("CLAUDE.md"))
        .unwrap();
    write(dir.path(), "AGENTS.md", "agents layer");
    let out =
        load_system_instructions_with(Some(dir.path()), &["AGENTS.md".into(), "CLAUDE.md".into()]);
    assert_eq!(out, "agents layer");
}

#[test]
fn distinct_files_with_same_content_in_different_dirs_both_load() {
    let root = tempfile::tempdir().unwrap();
    let sub = tempfile::tempdir_in(root.path()).unwrap();
    write(root.path(), "AGENTS.md", "root layer");
    write(sub.path(), "AGENTS.md", "sub layer");
    let out = load_system_instructions_with(Some(sub.path()), &["AGENTS.md".into()]);
    assert_eq!(out, "root layer\n\nsub layer");
}
