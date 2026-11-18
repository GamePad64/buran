//! `buran subagents`: the delegation MCP server (ADR 0011). Speaks
//! MCP-over-stdio like any external tool server; the harness cannot tell
//! it apart from one — no harness branches, just config.
//!
//! Two tools: `spawn` (start a `buran serve --subagent` child, return an
//! id) and `wait` (collect the final text). Depth ≤ 2 is enforced by the
//! child's `--subagent` config filter, not here. The ACP client to the
//! child mirrors the TUI crate's pattern, mirrored — not shared (the tui
//! crate must not depend on buran).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServiceExt};

/// Shared state for one agent: id + the result slot the driver fills.
pub struct AgentState {
    pub id: String,
    /// Set when the turn resolves (final text) or fails (error text).
    pub result: tokio::sync::watch::Receiver<Option<Result<String, String>>>,
}

type ChildTable = Arc<Mutex<HashMap<String, Arc<AgentState>>>>;

pub struct SubagentsServer {
    config: crate::config::Config,
    children: ChildTable,
}

impl SubagentsServer {
    pub fn new(config: crate::config::Config) -> Self {
        Self {
            config,
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn spawn_child(&self, params: SpawnParams) -> Result<CallToolResponse, McpError> {
        let live = self.children.lock().expect("children lock").len();
        if live >= self.config.subagents.max_children {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "spawn refused: {live} live children at the max_children cap ({}); wait for one to finish",
                self.config.subagents.max_children
            ))])
            .into());
        }

        let agent_id = uuid::Uuid::now_v7().to_string();
        let prompt = params.prompt.clone();
        let timeout = std::time::Duration::from_secs(
            params
                .timeout_secs
                .unwrap_or(self.config.subagents.timeout_secs),
        );

        // The driver task owns EVERYTHING about the child: the AcpAgent
        // spawns the process internally, drives the turn, and drops the
        // child on completion. No process handle in the table — the
        // driver's lifetime IS the child's lifetime.
        let (result_tx, result_rx) = tokio::sync::watch::channel(None);
        tokio::spawn(run_child_turn(prompt, timeout, result_tx));

        let state = Arc::new(AgentState {
            id: agent_id.clone(),
            result: result_rx,
        });
        self.children
            .lock()
            .expect("children lock")
            .insert(agent_id.clone(), state);

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            r##"{{"agent_id": "{agent_id}"}}"##
        ))])
        .into())
    }

    fn wait_child(&self, params: WaitParams) -> Result<CallToolResponse, McpError> {
        let state = {
            let children = self.children.lock().expect("children lock");
            children.get(&params.agent_id).cloned()
        };
        let Some(state) = state else {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "unknown agent_id: {}",
                params.agent_id
            ))])
            .into());
        };
        let timeout = std::time::Duration::from_secs(
            params
                .timeout_secs
                .unwrap_or(self.config.subagents.timeout_secs),
        );
        // Block the tool call (this handler is per-request; the server's
        // dispatch loop never parks behind it).
        let mut rx = state.result.clone();
        let waited = tokio::runtime::Handle::try_current().is_ok();
        let _ = waited;
        let outcome = futures_util_lite_wait(&mut rx, timeout);
        match outcome {
            Some(Ok(text)) => {
                let payload = serde_json::json!({"status": "done", "final_text": text});
                Ok(CallToolResult::success(vec![ContentBlock::text(payload.to_string())]).into())
            }
            Some(Err(error)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "subagent failed: {error}"
            ))])
            .into()),
            None => Ok(CallToolResult::success(vec![ContentBlock::text(
                "{\"status\": \"timeout\"}",
            )])
            .into()),
        }
    }
}

/// Synchronous-looking wait usable from a sync (non-async) trait method
/// context: parks the current thread on a oneshot bridged from the watch.
/// The server runs handlers on a multithreaded runtime, so blocking here
/// parks one worker thread per wait — acceptable for v2 delegation
/// (waits are rare relative to turns).
fn futures_util_lite_wait(
    rx: &mut tokio::sync::watch::Receiver<Option<Result<String, String>>>,
    timeout: std::time::Duration,
) -> Option<Result<String, String>> {
    if let Some(value) = rx.borrow().clone() {
        return Some(value);
    }
    let (tx, done) = std::sync::mpsc::channel();
    let mut rx = rx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("wait runtime");
        rt.block_on(async move {
            tokio::select! {
                _ = rx.changed() => {
                    let _ = tx.send(rx.borrow().clone());
                }
                _ = tokio::time::sleep(timeout) => {
                    let _ = tx.send(None);
                }
            }
        });
    });
    done.recv().expect("wait thread")
}

/// The ACP driver: spawn via `AcpAgent` (the SDK owns the process and
/// stdio, mirroring the TUI client), handshake, prompt, collect agent
/// text until the prompt response resolves. Runs inside its own task so
/// `spawn` returns the id immediately.
async fn run_child_turn(
    prompt: String,
    timeout: std::time::Duration,
    result_tx: tokio::sync::watch::Sender<Option<Result<String, String>>>,
) {
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::v1::schema::{InitializeRequest, SessionUpdate};
    use agent_client_protocol::v1::{SessionMessage, SessionSetup};
    use agent_client_protocol::{AcpAgent, Agent, Client, on_receive_notification};

    let outcome = tokio::time::timeout(timeout, async {
        // Spawn args resolve BEFORE the async block: no `?` inside.
        let agent_config = AcpAgent::from_args(["serve", "--subagent"])
            .map_err(|e| e.to_string())?;
        // Text collector: chunks accumulate; the prompt response ends
        // the turn and hands the collector back its full text.
        let (response_tx, response_rx) =
            tokio::sync::oneshot::channel::<Result<String, String>>();
        let response_tx = Arc::new(std::sync::Mutex::new(Some(response_tx)));
        let text = Arc::new(std::sync::Mutex::new(String::new()));

        let agent = Client
            .v1()
            .name("buran-subagents")
            .on_receive_notification(
                {
                    let text = text.clone();
                    async move |notification: agent_client_protocol::v1::schema::SessionNotification,
                                _cx| {
                        if let SessionUpdate::AgentMessageChunk(chunk) = notification.update
                            && let agent_client_protocol::v1::schema::ContentBlock::Text(t) =
                                chunk.content
                        {
                            text.lock().expect("text lock").push_str(&t.text);
                        }
                        Ok(())
                    }
                },
                on_receive_notification!(),
            )
            .connect_with(
                agent_config,
                move |cx: agent_client_protocol::v1::ConnectionTo<Agent>| async move {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    let SessionSetup { mut session, .. } = cx
                        .build_session(std::env::current_dir().unwrap_or_default())
                        .block_task()
                        .start_session()
                        .await?;
                    session.send_prompt(&prompt)?;
                    // Read until the prompt response; text is complete then.
                    loop {
                        match session.read_update().await? {
                            SessionMessage::PromptResponse(resp) => {
                                let _ = resp;
                                break;
                            }
                            other => {
                                let _ = other; // chunks already captured via notifications
                            }
                        }
                    }
                    let final_text = text.lock().expect("text lock").clone();
                    if let Some(tx) = response_tx.lock().expect("resp lock").take() {
                        let _ = tx.send(Ok(final_text));
                    }
                    // Park: returning would tear the connection down
                    // before the collector reads the result (Task 9 ruling).
                    cx.incoming_closed().await;
                    Ok(())
                },
            );

        // Drive the connection; the collector's oneshot resolves first.
        let result = tokio::select! {
            res = response_rx => match res {
                Ok(outcome) => outcome,
                Err(_) => Err("subagent connection dropped before the result".into()),
            },
            _ = agent => Err("subagent connection closed before the turn completed".into()),
        };
        result
    })
    .await;

    let final_outcome = match outcome {
        Err(_) => Err("subagent turn timed out".to_string()),
        Ok(r) => r,
    };
    let _ = result_tx.send(Some(final_outcome));
}

/// spawn tool arguments (camelCase on the wire, rmcp's serde default).
#[derive(serde::Deserialize)]
pub struct SpawnParams {
    pub prompt: String,
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
}

#[derive(serde::Deserialize)]
pub struct WaitParams {
    pub agent_id: String,
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
            "wait" => {
                let args = request.arguments.clone().unwrap_or_default();
                let params: WaitParams = serde_json::from_value(serde_json::Value::Object(args))
                    .map_err(|e| McpError::invalid_params(format!("wait arguments: {e}"), None))?;
                self.wait_child(params)
            }
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
