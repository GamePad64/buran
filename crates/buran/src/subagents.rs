//! `buran subagents`: the delegation MCP server (ADR 0011). Speaks
//! MCP-over-stdio like any external tool server; the harness cannot tell
//! it apart from one — no harness branches, just config.
//!
//! Two tools: `spawn` (start a `buran serve --subagent` child, return an
//! id) and `wait` (Task 4). Depth ≤ 2 is enforced by the child's
//! `--subagent` config filter (Task 1), not here.
//!
//! The ACP client to the child mirrors the TUI crate's pattern (small,
//! deliberately not shared — the tui crate must not depend on buran).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServiceExt};

/// One delegated child agent.
pub struct ChildAgent {
    _child: std::process::Child,
    pub agent_id: String,
}

/// The delegation server state: live children + finished results.
pub struct SubagentsServer {
    config: crate::config::Config,
    children: Mutex<HashMap<String, Arc<ChildAgent>>>,
}

impl SubagentsServer {
    pub fn new(config: crate::config::Config) -> Self {
        Self {
            config,
            children: Mutex::new(HashMap::new()),
        }
    }

    fn spawn_child(&self, params: SpawnParams) -> Result<CallToolResponse, McpError> {
        // Cap: live children only (finished ones are reaped by their
        // result cache, Task 4).
        let live = self.children.lock().expect("children lock").len();
        if live >= self.config.subagents.max_children {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "spawn refused: {live} live children at the max_children cap ({}); wait for one to finish",
                self.config.subagents.max_children
            ))])
            .into());
        }

        let cwd = params.cwd.clone();
        let child = std::process::Command::new(std::env::current_exe().map_err(|e| {
            McpError::internal_error(format!("cannot resolve buran binary: {e}"), None)
        })?)
        .args(["serve", "--subagent"])
        .current_dir(cwd.as_deref().unwrap_or("."))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| McpError::internal_error(format!("failed to spawn child: {e}"), None))?;

        let agent_id = uuid::Uuid::now_v7().to_string();
        self.children.lock().expect("children lock").insert(
            agent_id.clone(),
            Arc::new(ChildAgent {
                _child: child,
                agent_id,
            }),
        );

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            r#"{{"agent_id": "{}"}}"#,
            self.children
                .lock()
                .expect("children lock")
                .keys()
                .last()
                .cloned()
                .unwrap_or_default()
        ))])
        .into())
    }
}

/// spawn tool arguments (camelCase on the wire, rmcp's serde default).
#[derive(serde::Deserialize)]
pub struct SpawnParams {
    pub prompt: String,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
}

impl ServerHandler for SubagentsServer {
    fn get_info(&self) -> rmcp::model::InitializeResult {
        let mut info =
            rmcp::model::InitializeResult::new(rmcp::model::ServerCapabilities::default());
        info.server_info =
            rmcp::model::Implementation::new("buran-subagents", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(
            "Delegate a subtask to a parallel buran agent: spawn returns an id, wait collects the final text.".into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let spawn_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "The subtask for the child agent"},
                "cwd": {"type": "string", "description": "Working directory for the child (default: inherited)"},
                "timeout_secs": {"type": "integer", "description": "Wall-clock cap for the child turn"}
            },
            "required": ["prompt"]
        });
        let wait_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string"},
                "timeout_secs": {"type": "integer"}
            },
            "required": ["agent_id"]
        });
        let schema = |v: serde_json::Value| -> std::sync::Arc<rmcp::model::JsonObject> {
            serde_json::from_value(v).expect("tool schemas are static and valid")
        };
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                "spawn",
                "Start a subagent on a subtask; returns its agent_id immediately",
                schema(spawn_schema),
            ),
            Tool::new(
                "wait",
                "Wait for a subagent to finish; returns its final text",
                schema(wait_schema),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let _ = &context;
        match request.name.as_ref() {
            "spawn" => {
                let args = request.arguments.clone().unwrap_or_default();
                let params: SpawnParams = serde_json::from_value(serde_json::Value::Object(args))
                    .map_err(|e| {
                    McpError::invalid_params(format!("spawn arguments: {e}"), None)
                })?;
                self.spawn_child(params)
            }
            // Task 4: real wait. Until then the tool exists but reports
            // not-implemented honestly.
            "wait" => Ok(CallToolResult::error(vec![ContentBlock::text(
                "wait is not implemented yet (Task 4)",
            )])
            .into()),
            other => Err(McpError::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

/// Entry: `buran subagents` — load config, require `[subagents].enabled`,
/// serve MCP over stdio.
pub fn run(config: crate::config::Config) -> anyhow::Result<()> {
    if !config.subagents.enabled {
        anyhow::bail!("subagents are disabled: add [subagents] enabled = true to the config");
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let server = SubagentsServer::new(config);
    let stdio = rmcp::transport::async_rw::AsyncRwTransport::new_server(
        tokio::io::stdin(),
        tokio::io::stdout(),
    );
    rt.block_on(async move {
        let service = server.serve(stdio).await?;
        service
            .waiting()
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(())
    })
}
