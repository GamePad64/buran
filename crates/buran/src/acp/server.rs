//! The ACP agent: initialize → newSession → prompt, streaming
//! `session/update` notifications while the LLM loop runs.
//!
//! Handlers run ON the dispatch loop; only the prompt handler spawns (a
//! turn can take minutes). Permission decisions flow through channels fed
//! by the events stream (Task 10 wires the ACP round-trip; until then the
//! config policy decides inside the pump).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::{
    Agent, Client as AcpClient, Stdio, on_receive_notification, on_receive_request,
    v1::ConnectionTo,
    v1::schema::{
        AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
        InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
        SessionId, SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall,
        ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    },
};
use tokio::sync::{mpsc, watch};

use crate::config::{Config, PermissionPolicy, ProviderConfig};
use crate::llm::loop_::{AgentLoop, PermissionDecision, SessionState, TurnEvent};
use crate::llm::types::FinishReason;
use crate::llm::{LlmClient, LlmError};
use crate::mcp::McpManager;

/// LLM client factory: production builds real clients, tests inject mocks.
pub type LlmFactory = Arc<dyn Fn(&ProviderConfig) -> Result<LlmClient, LlmError> + Send + Sync>;

/// One live ACP session.
struct SessionHandle {
    system_prompt: String,
    skills: Vec<crate::skills::SkillDescription>,
    session: SessionState,
    cancel_tx: watch::Sender<bool>,
}

/// Shared server state: one MCP manager per process, sessions per client.
struct ServerState {
    config: Config,
    mcp: Arc<McpManager>,
    llm_factory: LlmFactory,
    sessions: Mutex<HashMap<SessionId, SessionHandle>>,
}

/// Builds the buran ACP agent from the real filesystem config and an
/// injected LLM factory. Production passes `LlmClient::new` behind the
/// factory; tests inject mock-backed clients. Async because MCP server
/// startup is; safe to call inside a tokio runtime.
pub async fn agent_builder_with_factory(
    llm_factory: impl Fn(&ProviderConfig) -> Result<LlmClient, LlmError> + Send + Sync + 'static,
) -> anyhow::Result<
    agent_client_protocol::Builder<
        Agent,
        impl agent_client_protocol::HandleDispatchFrom<AcpClient> + 'static,
        impl agent_client_protocol::RunWithConnectionTo<AcpClient> + 'static,
        impl agent_client_protocol::HandleConnectionClose<AcpClient> + 'static,
        agent_client_protocol::V1,
    >,
> {
    // Sessions resolve AGENTS.md from the client-provided cwd, so the
    // project dir for *config* is wherever buran was launched.
    let config = Config::load(std::env::current_dir().ok().as_deref(), |var| {
        std::env::var(var).ok()
    })?;

    // Fail fast on a missing API key: a harness that only fails at the
    // first prompt leaves clients showing an empty session.
    if let Some(provider) = &config.provider {
        provider.api_key.resolve().map_err(|_| {
            anyhow::anyhow!(
                "provider api key missing: set api_key in <config file> or BURAN_API_KEY env var"
            )
        })?;
    }

    agent_builder_with_config(config, llm_factory).await
}

/// Same, with an explicit config (tests inject skills dirs and policies).
/// Async: MCP servers spawn and handshake here.
pub async fn agent_builder_with_config(
    config: Config,
    llm_factory: impl Fn(&ProviderConfig) -> Result<LlmClient, LlmError> + Send + Sync + 'static,
) -> anyhow::Result<
    agent_client_protocol::Builder<
        Agent,
        impl agent_client_protocol::HandleDispatchFrom<AcpClient> + 'static,
        impl agent_client_protocol::RunWithConnectionTo<AcpClient> + 'static,
        impl agent_client_protocol::HandleConnectionClose<AcpClient> + 'static,
        agent_client_protocol::V1,
    >,
> {
    let Some(provider) = config.provider.clone() else {
        anyhow::bail!(
            "no provider configured: set [provider] in ~/.config/buran/config.toml \
             or buran.config.toml"
        );
    };
    let provider_for_agent = provider.clone();

    let mcp = McpManager::start(&config.mcp_servers, std::time::Duration::from_secs(10)).await?;

    let state = Arc::new(ServerState {
        config,
        mcp: Arc::new(mcp),
        llm_factory: Arc::new(llm_factory),
        sessions: Mutex::new(HashMap::new()),
    });

    Ok(build_agent(state, provider_for_agent))
}

fn build_agent(
    state: Arc<ServerState>,
    provider: ProviderConfig,
) -> agent_client_protocol::Builder<
    Agent,
    impl agent_client_protocol::HandleDispatchFrom<AcpClient> + 'static,
    impl agent_client_protocol::RunWithConnectionTo<AcpClient> + 'static,
    impl agent_client_protocol::HandleConnectionClose<AcpClient> + 'static,
    agent_client_protocol::V1,
> {
    // Each handler captures its own Arc clone: registrations move their
    // captures, and one Arc cannot feed three `async move` closures.
    let state_new_session = state.clone();
    let state_prompt = state.clone();
    let state_cancel = state.clone();
    let provider_prompt = provider.clone();

    Agent
        .v1()
        .name("buran")
        .on_receive_request(
            async |req: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx| {
                if !req.mcp_servers.is_empty() {
                    // MVP: client-supplied MCP servers are not supported
                    // (honest rejection, recorded in ADR 0003).
                    tracing::warn!("client-supplied MCP servers not supported yet");
                }
                let session_id = SessionId::new(new_session_id());
                let system_prompt =
                    build_system_prompt(&req.cwd, &state_new_session.config.skills_dirs);
                let skills = crate::skills::scan(&state_new_session.config.skills_dirs);
                let (cancel_tx, _) = watch::channel(false);
                state_new_session
                    .sessions
                    .lock()
                    .expect("sessions lock")
                    .insert(
                        session_id.clone(),
                        SessionHandle {
                            system_prompt,
                            skills,
                            session: SessionState::default(),
                            cancel_tx,
                        },
                    );
                responder.respond(NewSessionResponse::new(session_id))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx| {
                // Long turns must not block the dispatch loop: move the
                // responder and a connection clone into a spawned task.
                // Per-invocation clones (AsyncFnMut may run repeatedly).
                let state = state_prompt.clone();
                let provider = provider_prompt.clone();
                let conn = cx.clone();
                cx.spawn(async move {
                    let stop = run_prompt_turn(&state, &provider, &req, &conn).await;
                    let stop = stop.unwrap_or_else(|error| {
                        tracing::error!(%error, "prompt turn failed");
                        StopReason::EndTurn
                    });
                    responder.respond(PromptResponse::new(stop))
                })
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: CancelNotification, _cx| {
                if let Some(handle) = state_cancel
                    .sessions
                    .lock()
                    .expect("sessions lock")
                    .get(&notif.session_id)
                {
                    let _ = handle.cancel_tx.send(true);
                }
                Ok(())
            },
            on_receive_notification!(),
        )
}

/// Runs one prompt turn: builds the loop, streams `TurnEvent`s into
/// `session/update` notifications, maps the finish reason to a StopReason.
/// Cancellation maps to `Cancelled` (the protocol-required answer after
/// `session/cancel`).
async fn run_prompt_turn(
    state: &ServerState,
    provider: &ProviderConfig,
    req: &PromptRequest,
    conn: &ConnectionTo<AcpClient>,
) -> anyhow::Result<StopReason> {
    // Flatten the prompt blocks into one user text (text blocks only for
    // MVP; resource blocks arrive in v1.5 with context attachments).
    let mut text = String::new();
    for block in &req.prompt {
        if let ContentBlock::Text(t) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
        }
    }

    let (mut session, system_prompt, skills, mut cancel_rx) = {
        let mut sessions = state.sessions.lock().expect("sessions lock");
        let handle = sessions
            .get_mut(&req.session_id)
            .ok_or_else(|| anyhow::anyhow!("prompt for unknown session {}", req.session_id.0))?;
        let cancel_rx = handle.cancel_tx.subscribe();
        (
            std::mem::take(&mut handle.session),
            handle.system_prompt.clone(),
            handle.skills.clone(),
            cancel_rx,
        )
    };

    let llm = (state.llm_factory)(provider)?;
    let catalog = state.mcp.catalog().await;
    let tools: Arc<McpManager> = state.mcp.clone();
    let loop_ = AgentLoop::new(llm, tools, system_prompt)
        .with_catalog(&catalog)
        .with_skills(skills);

    let (events_tx, mut events_rx) = mpsc::channel::<TurnEvent>(64);
    let (perm_tx, perm_rx) = mpsc::channel::<PermissionDecision>(8);
    let session_id = req.session_id.clone();
    let message_id = MessageIdClock::start();

    // Event pump: TurnEvents → session/update notifications; permission
    // queries are answered by the config policy (Task 10: ACP round-trip).
    let policy = state.config.permission;
    let conn2 = conn.clone();
    let pump = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                TurnEvent::PermissionQuery(_) => {
                    let decision = match policy {
                        PermissionPolicy::AllowAll => PermissionDecision::Allow,
                        PermissionPolicy::DenyAll | PermissionPolicy::Ask => {
                            PermissionDecision::Deny
                        }
                    };
                    let _ = perm_tx.send(decision).await;
                }
                TurnEvent::TextDelta(delta) => {
                    let notification = SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::AgentMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(delta)))
                                .message_id(message_id.current()),
                        ),
                    );
                    if conn2.send_notification(notification).is_err() {
                        break;
                    }
                }
                TurnEvent::ToolCallStarted { call_id, name, .. } => {
                    let notification = SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::ToolCall(
                            ToolCall::new(ToolCallId::new(call_id), name.clone())
                                .name(name)
                                .kind(ToolKind::Other)
                                .status(ToolCallStatus::InProgress),
                        ),
                    );
                    if conn2.send_notification(notification).is_err() {
                        break;
                    }
                }
                TurnEvent::ToolCallFinished { call_id, output } => {
                    let status = if output.is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    };
                    let notification = SessionNotification::new(
                        session_id.clone(),
                        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            ToolCallId::new(call_id),
                            ToolCallUpdateFields::new()
                                .status(status)
                                .content(Vec::from([output.text.into()])),
                        )),
                    );
                    if conn2.send_notification(notification).is_err() {
                        break;
                    }
                }
                TurnEvent::TurnDone(_) => break,
            }
        }
    });

    let summary = loop_
        .run_turn(&mut session, &text, events_tx, perm_rx, cancel_rx.clone())
        .await;
    // run_turn drops the events sender on return (TurnDone already sent),
    // so the pump drains its buffered events and exits. Abort would lose
    // every event still sitting in the channel.
    let _ = pump.await;

    {
        let mut sessions = state.sessions.lock().expect("sessions lock");
        if let Some(handle) = sessions.get_mut(&req.session_id) {
            handle.session = session;
        }
    }

    let summary = summary.map_err(|error| anyhow::anyhow!("{error}"))?;

    // A requested cancel wins over the loop's finish reason: the protocol
    // REQUIRES Cancelled after session/cancel.
    let cancelled = *cancel_rx.borrow_and_update();
    Ok(if cancelled {
        StopReason::Cancelled
    } else {
        match summary.finish {
            FinishReason::Stop | FinishReason::ToolCalls | FinishReason::ContentFilter => {
                StopReason::EndTurn
            }
            FinishReason::Length => StopReason::MaxTokens,
        }
    })
}

/// Monotonic message id for one turn's text chunks: all chunks of one
/// assistant message share it; the next turn gets a new one.
struct MessageIdClock {
    turn: u64,
}

impl MessageIdClock {
    fn start() -> Self {
        Self {
            turn: turn_counter(),
        }
    }

    fn current(&self) -> agent_client_protocol::v1::schema::MessageId {
        agent_client_protocol::v1::schema::MessageId::new(format!(
            "buran-msg-{}-{}",
            self.turn,
            std::process::id()
        ))
    }
}

fn turn_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TURN: AtomicU64 = AtomicU64::new(0);
    TURN.fetch_add(1, Ordering::Relaxed)
}

/// System prompt = AGENTS.md instructions + skills description block.
fn build_system_prompt(
    project_dir: &std::path::Path,
    skills_dirs: &[std::path::PathBuf],
) -> String {
    let agentsmd = crate::agentsmd::load_system_instructions(Some(project_dir));
    let skills = crate::skills::scan(skills_dirs);
    let mut prompt = agentsmd;
    if !skills.is_empty() {
        if !prompt.is_empty() {
            prompt.push('\n');
        }
        prompt.push_str("## Available skills\n");
        prompt.push_str(
            "To load a skill, output `<skill>Name</skill>` as the first thing in your reply.\n",
        );
        for skill in skills {
            prompt.push_str(&format!("- {}: {}\n", skill.name, skill.description));
        }
    }
    prompt
}

fn new_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("buran-session-{nanos}")
}

/// Production entry: real serve on stdio. Logs never touch stdout.
pub fn serve() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async { serve_with(LlmClient::new).await })
}

/// Injectable serve (tests drive the builder directly; this is the
/// stdio-connected form).
pub async fn serve_with(
    llm_factory: impl Fn(&ProviderConfig) -> Result<LlmClient, LlmError> + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let builder = agent_builder_with_factory(llm_factory).await?;
    builder
        .connect_to(Stdio::new())
        .await
        .map_err(|error| anyhow::anyhow!("ACP connection failed: {error}"))
}
