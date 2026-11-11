//! MCP manager failure-path tests: real subprocesses that do not speak MCP.
//!
//! `cat` and `bash -c 'echo bad'` spawn fine but never answer the MCP
//! handshake, so `McpManager::start` must record them as failures and still
//! construct — one broken server must not sink the manager (degradation).

use std::collections::BTreeMap;
use std::time::Duration;

use buran::config::McpServerConfig;
use buran::mcp::{McpError, McpManager};

fn server(name: &str, command: &str, args: &[&str]) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: command.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        env: BTreeMap::new(),
    }
}

#[tokio::test]
async fn broken_server_is_recorded_as_failure_and_manager_survives() {
    let manager = McpManager::start(&[server("cat", "cat", &[])], Duration::from_secs(2))
        .await
        .expect("manager must construct even when every server fails");

    assert_eq!(manager.failures().len(), 1, "{:?}", manager.failures());
    assert_eq!(manager.failures()[0].0, "cat");
    assert!(
        !manager.failures()[0].1.is_empty(),
        "failure reason must be non-empty"
    );
    assert!(manager.catalog().await.names().is_empty());
    manager.shutdown().await;
}

#[tokio::test]
async fn mixed_healthy_and_broken_servers_degrade() {
    // `echo bad` exits immediately: spawn succeeds, handshake cannot.
    let configs = [
        server("echo", "bash", &["-c", "echo bad"]),
        server("cat", "cat", &[]),
    ];
    let manager = McpManager::start(&configs, Duration::from_secs(2))
        .await
        .expect("manager must construct");

    let failed: Vec<&str> = manager.failures().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(failed, ["echo", "cat"], "config order: {:?}", failed);
    manager.shutdown().await;
}

#[tokio::test]
async fn missing_binary_is_recorded_not_fatal() {
    let manager = McpManager::start(
        &[server("ghost", "/definitely/not/a/binary", &[])],
        Duration::from_secs(2),
    )
    .await
    .expect("manager must construct");

    assert_eq!(manager.failures()[0].0, "ghost");
    manager.shutdown().await;
}

#[tokio::test]
async fn unknown_tool_is_named_error() {
    let manager = McpManager::start(&[], Duration::from_secs(2))
        .await
        .unwrap();
    let err = manager.call("nope/tool", serde_json::json!({})).await;
    assert!(matches!(err, Err(McpError::UnknownTool { .. })), "{err:?}");
    assert!(err.unwrap_err().to_string().contains("nope/tool"));
    manager.shutdown().await;
}

/// DoD sweep: the real `npx @modelcontextprotocol/server-filesystem` is
/// picked up from `.mcp.json` and its tools land in the catalog. Gated on
/// node being installed (CI and minimal machines skip); `npx -y` needs
/// network on first run, hence the generous timeout.
#[tokio::test]
async fn real_filesystem_server_from_mcp_json_is_picked_up() {
    let Ok(output) = tokio::process::Command::new("node")
        .arg("--version")
        .output()
        .await
    else {
        eprintln!("skipping: node not installed");
        return;
    };
    if !output.status.success() {
        eprintln!("skipping: node --version failed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".mcp.json"),
        r#"{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]}}}"#,
    )
    .unwrap();

    // The .mcp.json parse itself is covered by config tests; here the
    // server config is constructed the way `Config::load` would produce it.
    let configs = [server(
        "fs",
        "npx",
        &["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
    )];
    let manager = McpManager::start(&configs, Duration::from_secs(60))
        .await
        .expect("manager constructs");

    assert!(
        manager.failures().is_empty(),
        "real fs server must handshake: {:?}",
        manager.failures()
    );
    let names = manager.catalog().await.names();
    assert!(
        names.iter().any(|n| n.starts_with("fs/")),
        "fs tools must be qualified in the catalog: {names:?}"
    );
    manager.shutdown().await;
}
