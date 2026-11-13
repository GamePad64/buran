//! CLI surface: `buran sessions list|rm` drive the real Store through
//! `Store::default_root()`, which reads `$HOME`. The test overrides HOME
//! for the child binary — the same env-isolation pattern as the
//! `serve_child_stdout_purity` test (acp_integration.rs), which already
//! spawns `env!("CARGO_BIN_EXE_buran")`.

use std::process::{Command, Stdio};

fn run_buran_with_home(home: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("run buran");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn seed_session(home: &std::path::Path) -> &'static str {
    let store_root = home.join(".local/state/buran/sessions");
    std::fs::create_dir_all(&store_root).unwrap();
    std::fs::write(
        store_root.join("01940000-0000-7000-8000-000000000001.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "type": "metadata", "model": "seed-model", "provider": "seed-provider",
                "started_at": "2026-11-12T00:00:00Z", "ended_at": null,
                "cwd": "/w"
            })
        ),
    )
    .unwrap();
    "01940000-0000-7000-8000-000000000001"
}

#[test]
fn sessions_list_and_rm_roundtrip_through_the_real_binary() {
    let home = tempfile::tempdir().unwrap();
    let id = seed_session(home.path());

    let out = run_buran_with_home(home.path(), &["sessions", "list"]);
    assert!(out.contains(id), "{out}");
    assert!(
        out.contains("2026-11-12T00:00:00Z"),
        "list carries started_at: {out}"
    );
    assert!(out.contains("m"), "list must carry head model: {out}");

    run_buran_with_home(home.path(), &["sessions", "rm", id]);
    let out = run_buran_with_home(home.path(), &["sessions", "list"]);
    assert!(!out.contains(id), "session gone after rm: {out}");
}

#[test]
fn sessions_list_on_empty_store_says_so() {
    let home = tempfile::tempdir().unwrap();
    let out = run_buran_with_home(home.path(), &["sessions", "list"]);
    assert!(out.to_lowercase().contains("no stored sessions"), "{out}");
}

#[test]
fn sessions_rm_unknown_id_fails_with_hint() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args(["sessions", "rm", "01940000-0000-7000-8000-000000000009"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("buran sessions list"),
        "must hint the list command: {err}"
    );
}

#[test]
fn serve_fork_creates_the_branch_file_then_serves() {
    // --fork copies the transcript to a new uuidv7 file, validates it, and
    // proceeds into serve. With Stdio::null() stdin, serve exits on EOF;
    // the observable contract: a fork file appears while it runs, its head
    // carries the lineage, the original keeps exactly one line.
    let home = tempfile::tempdir().unwrap();
    let id = seed_session(home.path());

    let mut child = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args(["serve", "--fork", id])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let fork_file = loop {
        let mut found = None;
        if let Ok(entries) = std::fs::read_dir(home.path().join(".local/state/buran/sessions")) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".jsonl") && !name.starts_with(id) {
                    found = Some(name);
                }
            }
        }
        if let Some(name) = found {
            break name;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("fork file never appeared");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    // Forked head carries lineage to the original.
    let text = std::fs::read_to_string(
        home.path()
            .join(".local/state/buran/sessions")
            .join(&fork_file),
    )
    .unwrap();
    assert!(
        text.contains(id),
        "fork head must record forked_from={id}: {text}"
    );

    // A fork is "the same conversation, new branch": the fork head carries
    // the SOURCE's model and provider (Finding 2, final review).
    let head_line = text.lines().next().unwrap();
    let head: serde_json::Value = serde_json::from_str(head_line).unwrap();
    assert_eq!(
        head["model"], "seed-model",
        "fork head must copy source model: {head_line}"
    );
    assert_eq!(
        head["provider"], "seed-provider",
        "fork head must copy source provider: {head_line}"
    );
    assert_eq!(
        head["cwd"], "/w",
        "fork head must copy source cwd: {head_line}"
    );

    // Original untouched.
    let original = std::fs::read_to_string(
        home.path()
            .join(".local/state/buran/sessions")
            .join(format!("{id}.jsonl")),
    )
    .unwrap();
    assert_eq!(
        original.lines().count(),
        1,
        "original keeps exactly its head line"
    );

    // serve exits on stdin EOF (Stdio::null); wait for it.
    let _ = child.wait();
}

#[test]
fn serve_resume_unknown_id_fails_fast() {
    // serve --resume with a garbage id must exit with an error naming the
    // list command — before speaking ACP on stdout.
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args(["serve", "--resume", "01940000-0000-7000-8000-000000000009"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("buran sessions list"), "must hint: {err}");
    assert!(
        out.stdout.is_empty(),
        "stdout is the ACP channel; failures happen before it: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
