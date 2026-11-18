//! `buran subagents` server tests: speak MCP-over-stdio to a REAL child
//! process (`env!("CARGO_BIN_EXE_buran")` + `subagents` subcommand) and
//! exercise the lifecycle. The spawned GRANDchild (`buran serve
//! --subagent`) gets an unreachable provider URL — lifecycle (spawn id,
//! wait status, cap) is testable without an LLM; final-text correctness
//! is Task 5's loopback stub's job.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

/// One line-per-message JSON-RPC exchange over the child's stdio.
struct McpProc {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    // Kept alive for the child's lifetime: the config HOME the server reads.
    _home: tempfile::TempDir,
}

impl McpProc {
    fn spawn(args: &[&str]) -> Self {
        // HOME points at a tempdir whose config enables subagents: the
        // server is opt-in by design, and the test is the opt-in. The
        // TempDir is held in the struct — dropping it mid-session would
        // delete the config under the running server.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
        std::fs::write(
            home.path().join(".config/buran/config.toml"),
            "[subagents]\nenabled = true\n",
        )
        .unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_buran"))
            .args(args)
            .env("HOME", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn buran");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
            _home: home,
        }
    }

    fn request(&mut self, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        let line = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        writeln!(self.stdin, "{line}").expect("write");
        self.stdin.flush().expect("flush");
        let mut buf = String::new();
        self.stdout.read_line(&mut buf).expect("read line");
        serde_json::from_str(&buf).expect("valid json response")
    }
}

impl Drop for McpProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn init(proc: &mut McpProc) {
    let resp = proc.request(
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    assert!(resp.get("result").is_some(), "initialize succeeded: {resp}");
    // notifications/initialized per the MCP lifecycle.
    let line = serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"});
    writeln!(proc.stdin, "{line}").unwrap();
    proc.stdin.flush().unwrap();
}

#[test]
fn subagents_server_lists_spawn_and_wait_tools() {
    let mut proc = McpProc::spawn(&["subagents"]);
    init(&mut proc);
    let resp = proc.request(2, "tools/list", serde_json::json!({}));
    let names: Vec<&str> = resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert!(
        names.contains(&"spawn") && names.contains(&"wait"),
        "exactly the delegation tools: {names:?}"
    );
}

#[test]
fn spawn_returns_an_agent_id_for_a_lifecycle_child() {
    // The child's provider points at an unreachable port: lifecycle only.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"unused\"\napi_key = \"x\"\n",
    )
    .unwrap();

    let mut proc = McpProc::spawn(&["subagents"]);
    init(&mut proc);
    let resp = proc.request(
        2,
        "tools/call",
        serde_json::json!({
            "name": "spawn",
            "arguments": {"prompt": "do a thing", "cwd": dir.path().to_string_lossy()}
        }),
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text result")
        .to_string();
    assert!(
        text.contains("agent_id"),
        "spawn answers with an agent id: {text}"
    );
}

#[test]
fn wait_unknown_id_errors_and_wait_timeout_reports_status() {
    let mut proc = McpProc::spawn(&["subagents"]);
    init(&mut proc);

    // Unknown id: a tool ERROR text (isError content), not a crash.
    let resp = proc.request(
        2,
        "tools/call",
        serde_json::json!({
            "name": "wait",
            "arguments": {"agent_id": "no-such-id", "timeout_secs": 1}
        }),
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("unknown agent_id"),
        "unknown id surfaces as an error text: {text}"
    );

    // A real child with an unreachable provider: wait hits the timeout
    // and reports status=timeout honestly (never hangs the tool past
    // its own cap).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"unused\"\napi_key = \"x\"\n",
    )
    .unwrap();
    let resp = proc.request(
        3,
        "tools/call",
        serde_json::json!({
            "name": "spawn",
            "arguments": {"prompt": "do a thing", "cwd": dir.path().to_string_lossy()}
        }),
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("spawn text");
    let parsed: serde_json::Value = serde_json::from_str(text).expect("spawn JSON");
    let agent_id: String = parsed["agent_id"]
        .as_str()
        .expect("agent_id field")
        .to_string();

    let resp = proc.request(
        4,
        "tools/call",
        serde_json::json!({
            "name": "wait",
            "arguments": {"agent_id": agent_id, "timeout_secs": 3}
        }),
    );
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("wait text");
    assert!(
        text.contains("timeout") || text.contains("failed"),
        "an unreachable-provider child ends in timeout or failure, honestly: {text}"
    );
}
