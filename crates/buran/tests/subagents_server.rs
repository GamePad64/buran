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
    // The config HOME must outlive the child process: dropping the
    // TempDir deletes the config mid-session (the classic opt-in trap).
    // Never read directly; held for its Drop.
    #[allow(dead_code)]
    home: tempfile::TempDir,
}

impl McpProc {
    fn spawn(args: &[&str]) -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
        std::fs::write(
            home.path().join(".config/buran/config.toml"),
            "[subagents]\nenabled = true\n",
        )
        .unwrap();
        let path = home.path().to_path_buf();
        Self::spawn_home(args, &path, home)
    }

    fn spawn_home(args: &[&str], home: &std::path::Path, held: tempfile::TempDir) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_buran"))
            .args(args)
            .env("HOME", home)
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
            home: held,
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

/// The text-level double: a loopback SSE endpoint the CHILD talks to
/// (the child is a separate process; hclient-mock cannot inject into it).
/// Test-only, labeled — production code never spawns HTTP.
mod stub {
    pub async fn serve_llm(answer: &'static str) -> std::net::SocketAddr {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let answer = Arc::new(answer.to_string());
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let answer = answer.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    // Count LLM rounds so every request gets a fresh answer.
                    // Simplest honest double: always the same completion.
                    let body = format!(
                        r#"{{"choices":[{{"delta":{{"content":"{answer}"}}}},{{"delta":{{}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}}}"#
                    );
                    let sse = format!(
                        "data: {body}\n\ndata: {{\"choices\":[{{\"delta\":{{}}}}]}}\n\ndata: [DONE]\n\n"
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        sse.len(),
                        sse
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        addr
    }
}

#[test]
fn e2e_spawn_wait_returns_the_child_final_text_and_marks_the_transcript() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let addr = rt.block_on(stub::serve_llm("SUBAGENT FINAL TEXT"));

    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
    std::fs::write(
        home.path().join(".config/buran/config.toml"),
        format!(
            "[subagents]\nenabled = true\n\n[provider]\nbase_url = \"http://{addr}/v1\"\nmodel = \"stub-model\"\napi_key = \"x\"\n"
        ),
    )
    .unwrap();

    let home_path = home.path().to_path_buf();
    // A live parent session exists before the model can call spawn: its
    // transcript is the newest uuidv7 in the store — that is the
    // delegation flow's parent-id channel (spawn_child's comment).
    let sessions_root = home_path.join(".local/state/buran/sessions");
    std::fs::create_dir_all(&sessions_root).unwrap();
    std::fs::write(
        sessions_root.join("01940000-0000-7000-8000-000000000042.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "type": "metadata", "model": "stub-model", "provider": "p",
                "started_at": "2026-11-18T00:00:00Z", "ended_at": null,
                "cwd": dir.path()
            })
        ),
    )
    .unwrap();
    let mut proc = McpProc::spawn_home(&["subagents"], &home_path, home);
    init(&mut proc);

    let resp = proc.request(
        2,
        "tools/call",
        serde_json::json!({
            "name": "spawn",
            "arguments": {"prompt": "say the phrase", "cwd": dir.path().to_string_lossy()}
        }),
    );
    let spawn_text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("spawn text");
    let parsed: serde_json::Value = serde_json::from_str(spawn_text).expect("spawn JSON");
    let agent_id = parsed["agent_id"].as_str().expect("agent_id").to_string();

    let resp = proc.request(
        3,
        "tools/call",
        serde_json::json!({"name": "wait", "arguments": {"agent_id": agent_id, "timeout_secs": 20}}),
    );
    let wait_text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("wait text");
    eprintln!("WAIT_TEXT: {wait_text}");
    assert!(
        wait_text.contains("SUBAGENT FINAL TEXT"),
        "wait returns the child's final text: {wait_text}"
    );
    assert!(wait_text.contains("\"done\""), "status done: {wait_text}");

    // The child transcript is marked subagent_of (its parent id is the
    // CHILD's own session id chain — here: the marker exists at all).
    let store_root = home_path.join(".local/state/buran/sessions");
    let entries = std::fs::read_dir(&store_root).expect("store exists");
    let mut marked_with_parent = false;
    for e in entries.flatten() {
        let text = std::fs::read_to_string(e.path()).unwrap_or_default();
        if text.contains("subagent_of\":\"01940000-0000-7000-8000-000000000042") {
            marked_with_parent = true;
        }
    }
    assert!(
        marked_with_parent,
        "the child transcript marks subagent_of = the parent session id"
    );

    drop(proc); // cleanup: kills children
}

#[test]
fn spawn_enforces_max_children() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
    std::fs::write(
        home.path().join(".config/buran/config.toml"),
        "[subagents]\nenabled = true\nmax_children = 2\n\n[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"unused\"\napi_key = \"x\"\n",
    )
    .unwrap();

    let home_path = home.path().to_path_buf();
    let mut proc = McpProc::spawn_home(&["subagents"], &home_path, home);
    init(&mut proc);

    let mut ids = Vec::new();
    for i in 0..2 {
        let resp = proc.request(
            10 + i,
            "tools/call",
            serde_json::json!({
                "name": "spawn",
                "arguments": {"prompt": "w", "cwd": dir.path().to_string_lossy()}
            }),
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        let parsed: serde_json::Value = serde_json::from_str(text).expect("spawn JSON");
        ids.push(parsed["agent_id"].as_str().expect("id").to_string());
    }

    // Third spawn at the cap of 2: refused with the live count.
    let resp = proc.request(
        12,
        "tools/call",
        serde_json::json!({
            "name": "spawn",
            "arguments": {"prompt": "w", "cwd": dir.path().to_string_lossy()}
        }),
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("spawn refused") && text.contains("2"),
        "the cap names itself: {text}"
    );
}

#[test]
fn cap_frees_slots_when_children_finish_and_crash_reports_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
    std::fs::write(
        home.path().join(".config/buran/config.toml"),
        "[subagents]\nenabled = true\nmax_children = 1\n\n[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"unused\"\napi_key = \"x\"\n",
    )
    .unwrap();
    let home_path = home.path().to_path_buf();
    let mut proc = McpProc::spawn_home(&["subagents"], &home_path, home);
    init(&mut proc);

    // One child fills the cap of 1.
    let resp = proc.request(
        20,
        "tools/call",
        serde_json::json!({
            "name": "spawn",
            "arguments": {"prompt": "w", "cwd": dir.path().to_string_lossy()}
        }),
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    let parsed: serde_json::Value = serde_json::from_str(text).expect("spawn JSON");
    let aid = parsed["agent_id"].as_str().expect("id").to_string();

    // Second spawn refused (cap 1, one live).
    let resp = proc.request(
        21,
        "tools/call",
        serde_json::json!({
            "name": "spawn",
            "arguments": {"prompt": "w", "cwd": dir.path().to_string_lossy()}
        }),
    );
    assert!(
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("spawn refused"),
        "cap holds while live"
    );

    // The child cannot reach an LLM → its turn FAILS (driver error), not
    // hangs: the crash path. wait reports the failure promptly.
    let resp = proc.request(
        22,
        "tools/call",
        serde_json::json!({"name": "wait", "arguments": {"agent_id": aid, "timeout_secs": 30}}),
    );
    let wait_text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        wait_text.contains("failed") || wait_text.contains("error"),
        "a dying child surfaces as an error, promptly: {wait_text}"
    );

    // THE FIX UNDER TEST: the finished child freed its slot — spawn works
    // again (the old code bricked the server after max_children spawns).
    let resp = proc.request(
        23,
        "tools/call",
        serde_json::json!({
            "name": "spawn",
            "arguments": {"prompt": "w", "cwd": dir.path().to_string_lossy()}
        }),
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        !text.contains("spawn refused"),
        "a finished child frees its slot: {text}"
    );
}
