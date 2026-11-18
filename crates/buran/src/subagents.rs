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
        {
            // Reap finished children: a watch whose value is already set
            // has resolved (done or failed) — its slot is free. Finished
            // ids stay in the map (wait on them returns the cached
            // result) but no longer count against the cap.
            let mut children = self.children.lock().expect("children lock");
            children.retain(|_id, state| state.result.borrow().is_none());
        }
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
        let cwd = params.cwd.clone();
        let timeout = std::time::Duration::from_secs(
            params
                .timeout_secs
                .unwrap_or(self.config.subagents.timeout_secs),
        );

        // The driver task owns EVERYTHING about the child: the AcpAgent
        // spawns the process internally, drives the turn, and drops the
        // child on completion. No process handle in the table — the
        // driver's lifetime IS the child's lifetime.
        //
        // The parent id: the newest transcript in the shared store —
        // newSession minted it right before the model could call spawn
        // (delegation always runs inside a live session). An env channel
        // cannot work: the subagents server starts before newSession, and
        // env writes in a different process never reach this one. Rare
        // race: a concurrently created session mislabels the marker —
        // catalog cosmetics, not execution.
        let parent_id = latest_session_id(&self.config).ok();
        // The `[subagents].provider` override rides as a pipe triple
        // (base_url|model|api_key_env) — the child rewrites its provider
        // from it (ADR 0011: the cheaper child model).
        let provider_spec = self
            .config
            .subagents
            .provider
            .as_ref()
            .map(|p| format!("{}|{}|{}", p.base_url, p.model, p.api_key_env));
        let (result_tx, result_rx) = tokio::sync::watch::channel(None);
        tokio::spawn(run_child_turn(
            prompt,
            cwd,
            parent_id,
            provider_spec,
            timeout,
            result_tx,
        ));

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
        // Wait ON the current runtime via block_in_place: a plain
        // `.await` cannot be used in this sync trait method, and the
        // old thread+private-runtime bridge starved the driver task on
        // single-worker runtimes (the reviewer's starvation finding).
        // block_in_place needs a multithreaded driver — run() builds
        // one; handlers run on it.
        let mut rx = state.result.clone();
        let outcome = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                tokio::select! {
                    _ = rx.changed() => rx.borrow().clone(),
                    _ = tokio::time::sleep(timeout) => None,
                }
            })
        });
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

/// The newest transcript id in the config's sessions root — the parent
/// session (see spawn_child's comment for why this is the channel).
fn latest_session_id(config: &crate::config::Config) -> anyhow::Result<String> {
    let store = crate::store::Store::new(
        config
            .sessions_dir
            .clone()
            .unwrap_or_else(crate::store::Store::default_root),
    );
    store
        .latest()
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("no stored sessions: the parent session has no transcript"))
}

/// The ACP driver: spawn via `AcpAgent` (the SDK owns the process and
/// stdio, mirroring the TUI client), handshake, prompt, collect agent
/// text until the prompt response resolves. Runs inside its own task so
/// `spawn` returns the id immediately.
async fn run_child_turn(
    prompt: String,
    child_cwd: Option<String>,
    parent_id: Option<String>,
    provider_spec: Option<String>,
    timeout: std::time::Duration,
    result_tx: tokio::sync::watch::Sender<Option<Result<String, String>>>,
) {
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::v1::schema::{InitializeRequest, SessionUpdate};
    use agent_client_protocol::v1::{SessionMessage, SessionSetup};
    use agent_client_protocol::{AcpAgent, Agent, Client, on_receive_notification};

    let outcome = tokio::time::timeout(timeout, async {
        // Spawn args resolve BEFORE the async block: no `?` inside.
        // The binary path is absolute — bare `serve` would resolve
        // against PATH and fail.
        let binary = std::env::current_exe()
            .map_err(|e| format!("cannot resolve buran binary: {e}"))?;
        let mut spawn_args = vec![
            binary.to_string_lossy().as_ref().to_string(),
            "serve".to_string(),
            "--subagent".to_string(),
        ];
        if let Some(parent) = parent_id {
            spawn_args.push("--subagent-of".to_string());
            spawn_args.push(parent);
        }
        if let Some(provider_spec) = provider_spec {
            spawn_args.push(format!("--subagent-provider={provider_spec}"));
        }
        let agent_config = AcpAgent::from_args(spawn_args).map_err(|e| e.to_string())?;

        // Text collector: chunks accumulate; the prompt response ends
        // the turn and hands the collector back its full text.
        let (response_tx, response_rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
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
                    // The child's cwd IS the spawn parameter: newSession
                    // records it in the transcript head (and Config::load
                    // reads the project config from it).
                    let cwd = child_cwd
                        .clone()
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    let SessionSetup { mut session, .. } = cx
                        .build_session(cwd)
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
