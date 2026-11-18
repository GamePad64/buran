//! CLI surface of the subagent child flags: clap accepts the spawner's
//! argument shapes without error (the flags are machine-set, ADR 0011).

use std::process::{Command, Stdio};

/// Minimal HOME: no sessions store, config load still succeeds (no
/// provider is fine — clap parsing fails before any provider use).
fn home_with_empty_store() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
    home
}

#[test]
fn subagent_provider_flag_is_accepted() {
    // The triple form from the spawner parses (no clap error).
    let home = home_with_empty_store();
    let out = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args([
            "serve",
            "--subagent",
            "--subagent-of",
            "p1",
            "--subagent-provider=http://cheap/v1|cheap-model|CHEAP_KEY",
        ])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run buran");
    // output() already waits; silence no-wait lint by asserting status exists.
    let _ = out.status;
    // It proceeds into serve (stdin EOF ends it); clap errors would exit
    // non-zero with usage text mentioning "unexpected argument".
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("unexpected argument"),
        "the provider flag must parse: {err}"
    );
}

#[test]
fn provider_override_reaches_the_child_provider() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
    std::fs::write(
        home.path().join(".config/buran/config.toml"),
        "[provider]\nbase_url = \"http://parent.test/v1\"\nmodel = \"parent-model\"\napi_key = \"x\"\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args([
            "serve",
            "--subagent",
            "--subagent-of",
            "PARENT-X",
            "--subagent-provider=http://cheap.test/v1|cheap-model|CHEAP_KEY",
        ])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn");

    // Drive the handshake READING BETWEEN WRITES: the SDK's init guard
    // needs the initialize response observed before session/new arrives —
    // a write-all-at-once pipe drive races it (manual repro).
    let stdout = child.stdout.take().expect("stdout");
    let mut stdout = std::io::BufReader::new(stdout);
    use std::io::BufRead as _;
    use std::io::Write as _;
    let mut stdin = child.stdin.take().expect("stdin");
    let send = |stdin: &mut std::process::ChildStdin, line: &str| {
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    };
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"processInfo":{"name":"t","version":"0"}}}"#,
    );
    let mut line = String::new();
    stdout.read_line(&mut line).expect("init response");
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}"#,
    );
    line.clear();
    stdout.read_line(&mut line).expect("new response");
    // Head written by newSession; stop the child and reap it.
    child.kill().ok();
    child.wait().ok();

    let store = home.path().join(".local/state/buran/sessions");
    let mut found = false;
    for e in std::fs::read_dir(&store).into_iter().flatten().flatten() {
        let text = std::fs::read_to_string(e.path()).unwrap_or_default();
        if text.contains("cheap-model") {
            found = true;
        }
    }
    assert!(
        found,
        "the override model lands in the child transcript head"
    );
}
