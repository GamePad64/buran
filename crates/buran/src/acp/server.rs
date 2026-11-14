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
        InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
        NewSessionResponse, PromptRequest, PromptResponse, SessionId, SessionNotification,
        SessionUpdate, StopReason, TextContent, ToolCall, ToolCallId, ToolCallStatus,
        ToolCallUpdate, ToolCallUpdateFields, ToolKind,
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
    /// Append handle for this session's transcript; `None` when the store
    /// refused the file at `newSession` time (persistence degrades, the
    /// session still runs fully in memory — sessions store spec).
    store_file: Option<crate::store::SessionFile>,
    /// In-flight prompt guard: `mem::take(&mut handle.session)` hands the
    /// history to a running turn; a second concurrent prompt would find it
    /// empty and fork the conversation. MVP is one turn per session — the
    /// second prompt is rejected until the first resolves.
    turn_in_flight: bool,
}

/// Why a prompt turn failed: before it started (a request-level rejection
/// the client sees as an error response) or mid-turn (after deltas may have
/// streamed — the failure surfaces as a message chunk, then EndTurn).
enum PromptError {
    Rejected(anyhow::Error),
    MidTurn(anyhow::Error),
}

/// Shared server state: one MCP manager per process, sessions per client,
/// one sessions store per process.
struct ServerState {
    config: Config,
    store: crate::store::Store,
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

    let store = crate::store::Store::new(
        config
            .sessions_dir
            .clone()
            .unwrap_or_else(crate::store::Store::default_root),
    );

    let state = Arc::new(ServerState {
        config,
        store,
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
    let state_load = state.clone();
    let state_prompt = state.clone();
    let state_cancel = state.clone();
    let provider_new_session = provider.clone();
    let provider_prompt = provider.clone();

    Agent
        .v1()
        .name("buran")
        .on_receive_request(
            async |req: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new().load_session(true)),
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
                let system_prompt =
                    build_system_prompt(&req.cwd, &state_new_session.config.skills_dirs);
                let skills = crate::skills::scan(&state_new_session.config.skills_dirs);
                let (cancel_tx, _) = watch::channel(false);
                // Transcript head: model/provider/cwd pin the session's
                // identity for `loadSession` later (sessions store spec).
                // Persistence degrades to pre-v1.5 behavior on failure: the
                // session runs in memory, the failure is logged (Review
                // Focus #1) — never fatal for the session itself.
                let store_file =
                    match state_new_session
                        .store
                        .create(crate::session::TranscriptMeta {
                            model: provider_new_session.model.clone(),
                            provider: provider_new_session.name.clone(),
                            started_at: crate::store::rfc3339_now(),
                            ended_at: None,
                            cwd: req.cwd.clone(),
                            forked_from: None,
                        }) {
                        Ok(f) => Some(f),
                        Err(e) => {
                            tracing::error!(
                                %e,
                                "cannot create session transcript — continuing without persistence"
                            );
                            None
                        }
                    };
                // The ACP session id IS the transcript filename stem (the
                // store's uuidv7), so `loadSession` finds the transcript
                // under the id the client already knows. Degraded sessions
                // (no transcript) get a bare uuidv7.
                let session_id = match &store_file {
                    Some(f) => SessionId::new(f.id().to_string()),
                    None => SessionId::new(new_session_id()),
                };
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
                            store_file,
                            turn_in_flight: false,
                        },
                    );
                responder.respond(NewSessionResponse::new(session_id))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, _cx| {
                let load_result = (|| -> anyhow::Result<SessionHandle> {
                    // Store read happens BEFORE the sessions lock: file I/O must
                    // not hold the lock; it guards only the map insert.
                    let (file, records) = state_load
                        .store
                        .open(&req.session_id.0)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    let head = records
                        .first()
                        .and_then(|r| match r {
                            crate::session::TranscriptRecord::Metadata(m) => Some(m.clone()),
                            _ => None,
                        })
                        .ok_or_else(|| anyhow::anyhow!("transcript has no metadata head"))?;
                    if head.cwd != req.cwd {
                        anyhow::bail!(
                            "cwd mismatch: session was created in {}, request is {} \
                             (sessions resume only in their original directory)",
                            head.cwd.display(),
                            req.cwd.display()
                        );
                    }
                    let mut history = Vec::new();
                    let mut compaction_points = Vec::new();
                    for record in &records {
                        match record {
                            crate::session::TranscriptRecord::Message(m) => history.push(m.clone()),
                            crate::session::TranscriptRecord::CompactionPoint {
                                at_message,
                                ..
                            } => compaction_points.push(*at_message),
                            crate::session::TranscriptRecord::Metadata(_) => {}
                            // Usage is bookkeeping, not conversation: it
                            // must not affect the rebuilt SessionState
                            // (Review Focus #3).
                            crate::session::TranscriptRecord::Usage { .. } => {}
                        }
                    }
                    Ok(SessionHandle {
                        system_prompt: build_system_prompt(
                            &req.cwd,
                            &state_load.config.skills_dirs,
                        ),
                        skills: crate::skills::scan(&state_load.config.skills_dirs),
                        session: SessionState {
                            history,
                            compaction_points,
                        },
                        cancel_tx: watch::channel(false).0,
                        turn_in_flight: false,
                        store_file: Some(file),
                    })
                })();
                match load_result {
                    Ok(handle) => {
                        state_load
                            .sessions
                            .lock()
                            .expect("sessions lock")
                            .insert(req.session_id.clone(), handle);
                        let _ = responder.respond(LoadSessionResponse::new());
                    }
                    Err(e) => {
                        let _ = responder.respond_with_error(agent_client_protocol::Error::new(
                            agent_client_protocol::ErrorCode::INVALID_PARAMS,
                            format!("{e}"),
                        ));
                    }
                }
                Ok::<(), agent_client_protocol::Error>(())
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
                    match run_prompt_turn(&state, &provider, &req, &conn).await {
                        Ok(stop) => {
                            let _ = responder.respond(PromptResponse::new(stop));
                        }
                        // A turn that failed before producing anything
                        // (unknown session, concurrent prompt) is a request
                        // error: the client sees it, not a fake EndTurn.
                        Err(PromptError::Rejected(error)) => {
                            tracing::error!(%error, "prompt rejected");
                            let _ =
                                responder.respond_with_error(agent_client_protocol::Error::new(
                                    agent_client_protocol::ErrorCode::INTERNAL_ERROR,
                                    error.to_string(),
                                ));
                        }
                        // A turn that failed mid-flight (LLM error after
                        // deltas may have flown): the protocol has no error
                        // completion for a started turn — surface the
                        // failure as a message chunk the user reads, then
                        // end. A bare EndTurn would look like a clean stop.
                        Err(PromptError::MidTurn(error)) => {
                            tracing::error!(%error, "prompt turn failed mid-flight");
                            let _ = conn.send_notification(SessionNotification::new(
                                req.session_id.clone(),
                                SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                    ContentBlock::Text(TextContent::new(format!(
                                        "\n[error: {error}]"
                                    ))),
                                )),
                            ));
                            let _ = responder.respond(PromptResponse::new(StopReason::EndTurn));
                        }
                    }
                    Ok::<(), agent_client_protocol::Error>(())
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
) -> Result<StopReason, PromptError> {
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

    // History length BEFORE `mem::take`, under the same lock: everything
    // the turn appends past this mark belongs to this turn's transcript
    // slice (persisted in the guard block below).
    let (starting_history_len, session, system_prompt, skills, cancel_rx) = {
        let mut sessions = state.sessions.lock().expect("sessions lock");
        let handle = sessions.get_mut(&req.session_id).ok_or_else(|| {
            PromptError::Rejected(anyhow::anyhow!(
                "prompt for unknown session {}",
                req.session_id.0
            ))
        })?;
        if handle.turn_in_flight {
            return Err(PromptError::Rejected(anyhow::anyhow!(
                "session {} already has a prompt in flight",
                req.session_id.0
            )));
        }
        handle.turn_in_flight = true;
        let starting_history_len = handle.session.history.len();
        let cancel_rx = handle.cancel_tx.subscribe();
        (
            starting_history_len,
            std::mem::take(&mut handle.session),
            handle.system_prompt.clone(),
            handle.skills.clone(),
            cancel_rx,
        )
    };
    // All exits below must hand the session state back and clear the guard;
    // the closure form keeps the unlock and the store on one path.
    let turn = TurnContext {
        text,
        session,
        system_prompt,
        skills,
        cancel_rx,
    };
    let result = run_prompt_turn_inner(state, provider, req, conn, turn).await;
    {
        let mut sessions = state.sessions.lock().expect("sessions lock");
        if let Some(handle) = sessions.get_mut(&req.session_id) {
            handle.turn_in_flight = false;
            // Persist everything this turn added to history, in order. The
            // loop stays I/O-free (ADR 0001); the server layer owns the
            // store. The user prompt is included: the loop pushes it at
            // loop_.rs:192 like every other message. An append failure
            // degrades (Review Focus #1): the conversation continues in
            // memory, later appends still try — do not `?` out, the turn
            // result must not change. The turn may have exited early (LLM
            // error): keep whatever history the turn recorded instead of
            // dropping it.
            if let Some(file) = handle.store_file.as_mut() {
                for msg in handle.session.history[starting_history_len..].iter() {
                    if let Err(e) =
                        file.append(&crate::session::TranscriptRecord::Message(msg.clone()))
                    {
                        tracing::error!(
                            %e,
                            "transcript append failed — continuing without persistence"
                        );
                    }
                }
            }
        }
    }
    result.map_err(PromptError::MidTurn)
}

/// Everything a turn body needs from its session, taken under the sessions
/// lock and handed back after the turn resolves.
struct TurnContext {
    text: String,
    session: SessionState,
    system_prompt: String,
    skills: Vec<crate::skills::SkillDescription>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
}

/// The turn body, split out so `run_prompt_turn` can own the guard's
/// set/clear around it.
async fn run_prompt_turn_inner(
    state: &ServerState,
    provider: &ProviderConfig,
    req: &PromptRequest,
    conn: &ConnectionTo<AcpClient>,
    turn: TurnContext,
) -> anyhow::Result<StopReason> {
    let TurnContext {
        text,
        mut session,
        system_prompt,
        skills,
        mut cancel_rx,
    } = turn;
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
    // queries follow the config policy — Ask round-trips the client (in
    // this spawned task, never on the dispatch loop), AllowAll/DenyAll
    // decide locally.
    let policy = state.config.permission;
    let permission_timeout = state.config.permission_timeout;
    let conn2 = conn.clone();
    let pump = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                TurnEvent::PermissionQuery(query) => {
                    let decision = match policy {
                        PermissionPolicy::AllowAll => PermissionDecision::Allow,
                        PermissionPolicy::DenyAll => PermissionDecision::Deny,
                        PermissionPolicy::Ask => {
                            super::request_permission(
                                &conn2,
                                &session_id,
                                ToolCallId::new(query.call_id),
                                &query.tool,
                                &format!("{}/{}", query.server, query.tool),
                                permission_timeout,
                            )
                            .await
                        }
                    };
                    if perm_tx.send(decision).await.is_err() {
                        break;
                    }
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
            FinishReason::MaxTurnRequests => StopReason::MaxTurnRequests,
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
    // UUIDv7: time-ordered, so lexicographic filename order equals
    // chronological order — `--resume latest` resolves by filename
    // without file contents (sessions store spec).
    uuid::Uuid::now_v7().to_string()
}

/// Test seam: integration suite asserts the ID format contract.
#[doc(hidden)]
pub fn new_session_id_for_test() -> String {
    new_session_id()
}

/// Production entry: real serve on stdio. Logs go to stderr only — stdout
/// is the ACP JSON-RPC channel and must stay clean.
pub fn serve() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

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
