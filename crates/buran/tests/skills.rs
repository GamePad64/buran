use buran::skills::{self, SkillDescription};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills")
}

#[test]
fn scan_finds_valid_skills_and_skips_broken() {
    let found = skills::scan(&[fixtures_dir()]);
    let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
    // broken-no-name lacks `name` in frontmatter: skipped with a warning.
    assert_eq!(names, ["commit-helper", "rust-style"]);
    let commit = &found[0];
    assert_eq!(
        commit.description,
        "Writes conventional commits from staged diffs"
    );
    assert_eq!(
        commit.source_path,
        fixtures_dir().join("commit-helper/SKILL.md")
    );
}

#[test]
fn scan_is_recursive_one_group_level() {
    let found = skills::scan(&[fixtures_dir()]);
    assert!(
        found
            .iter()
            .any(|s| s.source_path.ends_with("rust/style/SKILL.md")),
        "nested group/my-skill/SKILL.md must be found"
    );
}

#[test]
fn first_dir_wins_on_name_collision() {
    let found = skills::scan(&[fixtures_dir(), fixtures_dir()]);
    let commits: Vec<&SkillDescription> =
        found.iter().filter(|s| s.name == "commit-helper").collect();
    assert_eq!(commits.len(), 1, "first dir wins: no duplicate");
}

#[test]
fn load_body_strips_frontmatter() {
    let found = skills::scan(&[fixtures_dir()]);
    let commit = found.iter().find(|s| s.name == "commit-helper").unwrap();
    let body = skills::load_body(commit).unwrap();
    let body = body.body_markdown;
    assert!(body.starts_with("# Commit helper"), "{body:?}");
    assert!(!body.contains("name: commit-helper"));
    assert!(!body.contains("---"));
}

#[test]
fn load_body_missing_file_is_named_error() {
    let ghost = SkillDescription {
        name: "ghost".into(),
        description: "gone".into(),
        source_path: PathBuf::from("/nowhere/SKILL.md"),
    };
    let err = skills::load_body(&ghost).unwrap_err();
    assert!(err.to_string().contains("/nowhere/SKILL.md"), "{err}");
}

#[test]
fn empty_and_missing_dirs_are_fine() {
    assert!(skills::scan(&[]).is_empty());
    let ghost = PathBuf::from("/definitely/not/here");
    assert!(skills::scan(&[ghost]).is_empty());
}
