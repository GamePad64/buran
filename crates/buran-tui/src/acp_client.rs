//! The ACP client half of the TUI: spawns the agent process, holds the
//! connection open, and exposes the session to the UI loop.
//!
//! Task 9/10 rulings apply mirrored to the client side: the `connect_with`
//! main_fn parks (returning would tear the connection down mid-session);
//! `initialize` MUST precede every other method. All inbound protocol
//! traffic — text chunks and permission questions — converts into one
//! unified event stream (`ClientEvent`), so the UI is a plain `select!`
//! loop with no cross-task chunk plumbing. A prompt runs detached in its
//! own task (`prompt_detached` clones the connection handle): chunks and
//! questions must surface *mid-turn*, which a call-and-return `prompt`
//! cannot do.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PermissionOption, PermissionOptionKind,
    PromptRequest, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, Client, on_receive_notification, on_receive_request};
use tokio::sync::{mpsc, oneshot};

/// How the user answered the permission modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// First allow option (AllowOnce preferred over AllowAlways).
    Allow,
    /// First reject option (RejectOnce preferred over RejectAlways).
    Reject,
    /// Dismiss the question without choosing.
    Cancel,
}

/// What the user's answer resolves to on the handler side: the chosen
/// option id, matched by `kind` at answer time (never by position).
#[derive(Debug)]
pub(crate) enum ResolvedAnswer {
    Selected(String),
    Cancelled,
}

/// One pending permission request, surfaced to the UI. The UI keeps it
/// until the user answers; dropping it unanswered cancels the request
/// agent-side.
#[derive(Debug)]
pub struct PermissionQuestion {
    title: String,
    options: Vec<PermissionOption>,
    answer: Option<oneshot::Sender<ResolvedAnswer>>,
}

impl PermissionQuestion {
    fn new(
        title: String,
        options: Vec<PermissionOption>,
        answer: oneshot::Sender<ResolvedAnswer>,
    ) -> Self {
        Self {
            title,
            options,
            answer: Some(answer),
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Resolves the question. The option id is matched by `kind`, never
    /// by position; a question with no option of the requested kind
    /// cancels (the agent-side policy then denies). Idempotent: a second
    /// call on an already-answered question is a no-op.
    pub fn answer(&mut self, choice: Answer) {
        let Some(answer_tx) = self.answer.take() else {
            return;
        };
        let kinds: &[PermissionOptionKind] = match choice {
            Answer::Allow => &[
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
            ],
            Answer::Reject => &[
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ],
            Answer::Cancel => &[],
        };
        let resolved = kinds
            .iter()
            .find_map(|kind| {
                self.options
                    .iter()
                    .find(|option| option.kind == *kind)
                    .map(|option| ResolvedAnswer::Selected(option.option_id.0.to_string()))
            })
            .unwrap_or(ResolvedAnswer::Cancelled);
        let _ = answer_tx.send(resolved);
    }
}

/// One UI event: assistant text, replayed user text, or a permission
/// question. The UI is a plain `select!` over this stream, key events,
/// and the turn handle.
#[derive(Debug)]
pub enum ClientEvent {
    /// Assistant message text.
    Text(String),
    /// User message text — seen only on the `session/load` replay path,
    /// where the agent re-sends prior turns as `UserMessageChunk` before
    /// the load response. Distinct from [`ClientEvent::Text`] so the
    /// transcript renders resumed user turns as the user's, not the
    /// assistant's.
    UserText(String),
    /// The agent asks which option to take; the UI answers through the
    /// question's `answer` method.
    Question(PermissionQuestion),
}

/// How long the permission handler waits for the UI answer before
/// cancelling (denying). Generous: a human reads the modal.
const PERMISSION_UI_TIMEOUT: Duration = Duration::from_secs(120);

/// Ceiling on the spawn handshake: a TUI failing only at the first prompt
/// looks like a hang, so `spawn` fails loudly instead.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Selection flags appended to the spawned agent command. Pure so the
/// arg shape is testable without spawning the agent. Returns ONLY the
/// flags — the `serve` subcommand itself already sits in the agent's
/// arg list: the user's `--agent` string (default "buran serve") went
/// through `AcpAgent::from_str`, and the SDK's `args()` APPENDS
/// (`self.args.extend`), so returning a "serve" prefix here would spawn
/// `buran serve serve --resume ...`.
pub fn serve_args(resume: Option<String>, fork: Option<String>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(id) = resume {
        args.push("--resume".into());
        args.push(id);
    }
    if let Some(id) = fork {
        args.push("--fork".into());
        args.push(id);
    }
    args
}

/// Store root read from `$HOME` — the same XDG path `Store::default_root`
/// computes server-side. Resolving "latest" TUI-locally duplicates that
/// rule on purpose: the TUI crate has no dependency on the buran store
/// module (the ACP stdio seam is the contract), and the v1.5 model is one
/// process on one machine — TUI and spawned `serve` share `$HOME`, so a
/// max-filename listing sees exactly the store the agent sees. Cross-machine
/// resume (remote agent, separate store) is a spec non-goal.
fn store_root() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    std::path::Path::new(&home).join(".local/state/buran/sessions")
}

/// Max `.jsonl` stem in the sessions store — the `Store::latest` rule
/// (uuidv7 sorts lexicographically == chronologically) mirrored without a
/// crate dependency. `Err` when the store is missing or has no sessions:
/// resuming nothing must fail loudly, not silently start a fresh session.
fn latest_session_id() -> anyhow::Result<String> {
    latest_session_id_in(store_root())
}

/// The max-stem scan itself, rooted at an explicit directory so tests can
/// point it at a tempdir instead of the real HOME.
fn latest_session_id_in(root: std::path::PathBuf) -> anyhow::Result<String> {
    let max = std::fs::read_dir(&root)
        .map_err(|e| anyhow::anyhow!("no stored sessions to resume ({}: {e})", root.display()))?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".jsonl").map(str::to_owned)
        })
        .max();
    match max {
        Some(id) => Ok(id),
        None => anyhow::bail!("no stored sessions to resume ({})", root.display()),
    }
}

/// The ready-signal slot: a take-once oneshot sender shared by the
/// main_fn and the connection task.
type ReadySlot = Arc<
    std::sync::Mutex<Option<oneshot::Sender<anyhow::Result<(ConnectionTo<Agent>, SessionId)>>>>,
>;

/// Sends the one ready-signal the spawn task ever produces: success from
/// the main_fn, failure from either the main_fn or the task wrapper. The
/// `Option` slot makes the first send win and later ones no-ops.
struct ReadyReporter {
    slot: ReadySlot,
}

impl ReadyReporter {
    fn report(&self, error: String) {
        if let Some(tx) = self.slot.lock().unwrap().take() {
            let _ = tx.send(Err(anyhow::anyhow!("{error}")));
        }
    }
}

/// A live connection to one spawned agent process. The process lives as
/// long as the connection: the SDK reaps it when the connection drops.
pub struct AcpClient {
    connection: ConnectionTo<Agent>,
    session_id: SessionId,
    /// All inbound protocol traffic (chunks and questions), in arrival
    /// order.
    events: tokio::sync::Mutex<mpsc::Receiver<ClientEvent>>,
}

impl AcpClient {
    /// Spawns the agent command, initializes, and opens a session — fresh,
    /// or (with `resume`/`fork`) attached to a stored conversation via
    /// `session/load`, whose replayed `session/update` notifications flow
    /// through the same event stream as live turns. `fork` makes the
    /// AGENT copy-on-start the transcript; the TUI then resumes the minted
    /// fork, which it finds as "latest" (a fork file is the newest uuidv7
    /// in the shared-HOME store). Fails loudly when the handshake does not
    /// complete: a TUI failing only at the first prompt looks like a hang.
    pub async fn spawn(
        agent_command: &str,
        resume: Option<String>,
        fork: Option<String>,
    ) -> anyhow::Result<Self> {
        // The fork target for the client's own load: the agent mints the
        // fork id server-side and the TUI cannot read it from the child's
        // stderr, so the client resumes "latest" — resolved HERE against
        // the store directory (shared-HOME contract, see
        // `latest_session_id`), before any process spawns so an empty
        // store fails before a terminal is claimed.
        let load_target = match (&resume, &fork) {
            (Some(id), _) if id != "latest" => Some(id.clone()),
            (_, Some(_)) | (Some(_), None) => Some(latest_session_id()?),
            (None, None) => None,
        };
        // The fluent `.args()` (an APPEND, `self.args.extend`) lives on
        // `AcpAgentConfig`, not on `AcpAgent` itself: round-trip through
        // the config to add the selection flags after whatever `from_str`
        // shell-split out of the user's `--agent` string.
        let agent = AcpAgent::new(
            AcpAgent::from_str(agent_command)?
                .into_config()
                .args(serve_args(resume, fork)),
        );

        let (event_tx, event_rx) = mpsc::channel::<ClientEvent>(256);
        // Each handler registration moves its own clone (AsyncFnMut
        // closures may run repeatedly, so captures cannot be handed
        // between runs).
        let event_tx_chunk = event_tx.clone();
        // The spawned main_fn hands the live connection clone back before
        // parking; `spawn` awaits only this channel, not the connection.
        // Shared behind an Arc'd take-once slot: both the main_fn and the
        // task wrapper report, and the first send wins.
        let (ready_tx, ready_rx) =
            oneshot::channel::<anyhow::Result<(ConnectionTo<Agent>, SessionId)>>();
        let ready_slot: ReadySlot = Arc::new(std::sync::Mutex::new(Some(ready_tx)));
        let wrapper_reporter = ReadyReporter {
            slot: ready_slot.clone(),
        };
        let main_fn_reporter = ReadyReporter {
            slot: ready_slot.clone(),
        };

        // `connect_with` resolves only when main_fn returns, and the
        // main_fn parks for the whole session (returning tears the
        // connection down mid-session — Task 9 ruling). So the whole
        // builder chain runs as a background task and `spawn` learns of
        // success/failure exclusively through the ready slot.
        tokio::spawn(async move {
            let result = Client
                .v1()
                .name("buran-tui")
                .on_receive_notification(
                    async move |notification: SessionNotification, _cx| {
                        // Text chunks, live or replayed (session/load sends
                        // prior turns back as updates before the load
                        // response). Tool-call and other update variants
                        // are ignored — v1.5 replay scope is text.
                        match notification.update {
                            SessionUpdate::AgentMessageChunk(chunk) => {
                                if let ContentBlock::Text(text) = chunk.content {
                                    // A closed sink only means the UI
                                    // stopped reading (e.g. the app
                                    // already exited); dropping the chunk
                                    // is correct there.
                                    let _ = event_tx_chunk.send(ClientEvent::Text(text.text)).await;
                                }
                            }
                            SessionUpdate::UserMessageChunk(chunk) => {
                                if let ContentBlock::Text(text) = chunk.content {
                                    let _ =
                                        event_tx_chunk.send(ClientEvent::UserText(text.text)).await;
                                }
                            }
                            _ => {}
                        }
                        Ok(())
                    },
                    on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: RequestPermissionRequest, responder, _connection| {
                        // Surface the question to the UI, then wait for
                        // its answer through the question's own oneshot.
                        // The wait is bounded and this handler is
                        // per-request, so the dispatch loop never parks
                        // behind an unanswered modal forever.
                        let (answer_tx, answer_rx) = oneshot::channel();
                        let _ = event_tx
                            .send(ClientEvent::Question(PermissionQuestion::new(
                                request
                                    .tool_call
                                    .fields
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| "tool call".into()),
                                request.options.clone(),
                                answer_tx,
                            )))
                            .await;

                        let answered = tokio::time::timeout(PERMISSION_UI_TIMEOUT, answer_rx).await;
                        let response = match answered {
                            Ok(Ok(ResolvedAnswer::Selected(option_id)))
                                if request
                                    .options
                                    .iter()
                                    .any(|o| o.option_id.0.as_ref() == option_id) =>
                            {
                                RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                                    SelectedPermissionOutcome::new(option_id),
                                ))
                            }
                            // Unknown id, timeout, question dropped
                            // unanswered, or channel closed: cancel (the
                            // agent-side policy then denies).
                            _ => {
                                RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                            }
                        };
                        responder.respond(response)
                    },
                    on_receive_request!(),
                )
                .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                    // initialize MUST come first (SDK initialization guard).
                    if let Err(error) = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await
                    {
                        // `connect_with` surfaces main_fn errors only to
                        // its own awaiter (the parked task), so the
                        // handshake failure must also travel to `spawn`
                        // through the ready slot.
                        main_fn_reporter.report(format!("initialize failed: {error}"));
                        return Err(error);
                    }

                    // Deliberately no fs capability (default capabilities):
                    // without an external fs MCP server the model simply has
                    // no fs tools — honest degradation, no crash.
                    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                    // Resume: `session/load` attaches to the stored
                    // conversation; replay notifications sent before the
                    // load response arrive through the builder-level
                    // notification handler (user handlers run before the
                    // SDK's session route), so the transcript rebuilds in
                    // the UI before the first prompt. The response's
                    // session id IS the loaded id.
                    let session_id = match load_target {
                        Some(id) => match connection
                            .load_session(SessionId::new(id), cwd.clone())
                            .block_task()
                            .start_session()
                            .await
                        {
                            Ok(setup) => setup.session.session_id().clone(),
                            Err(error) => {
                                main_fn_reporter.report(format!("session/load failed: {error}"));
                                return Err(error);
                            }
                        },
                        None => match connection
                            .send_request(NewSessionRequest::new(cwd))
                            .block_task()
                            .await
                        {
                            Ok(session) => session.session_id,
                            Err(error) => {
                                main_fn_reporter.report(format!("session/new failed: {error}"));
                                return Err(error);
                            }
                        },
                    };

                    if let Some(ready_tx) = ready_slot.lock().unwrap().take() {
                        let _ = ready_tx.send(Ok((connection.clone(), session_id)));
                    }

                    // Park until the process exits: returning tears the
                    // connection down mid-session (Task 9 ruling). EOF on
                    // the child's stdout also resolves this future.
                    connection.incoming_closed().await;
                    Ok(())
                })
                .await
                .map_err(|error| anyhow::anyhow!("ACP connection failed: {error}"));

            // If the connection itself died before ready (spawn failure,
            // child crash), the error must reach `spawn` — it cannot stay
            // in this parked task. A no-op when the main_fn already
            // reported (success or failure): the first send wins.
            if let Err(error) = result {
                wrapper_reporter.report(error.to_string());
            }
        });

        let (connection, session_id) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            ready_rx
                .await
                .map_err(|error| anyhow::anyhow!("connection dropped before ready: {error}"))?
        })
        .await
        .map_err(|_| anyhow::anyhow!("agent handshake timed out"))??;

        Ok(AcpClient {
            connection,
            session_id,
            events: tokio::sync::Mutex::new(event_rx),
        })
    }

    /// One protocol event (a text chunk or a permission question);
    /// `None` when the agent connection closed. The UI event loop awaits
    /// this in its `select!` — no polling: every arrival is a real wake.
    pub async fn next_event(&self) -> Option<ClientEvent> {
        let mut events = self.events.lock().await;
        events.recv().await
    }

    /// Sends one user line in a detached task and returns the handle the
    /// UI awaits for the stop reason. Text chunks and permission
    /// questions do NOT flow through here — they arrive via `next_event`
    /// on their own `select!` arm while the turn runs, which is the whole
    /// point of detaching.
    pub fn prompt_detached(&self, text: String) -> TurnHandle {
        let connection = self.connection.clone();
        let session_id = self.session_id.clone();
        let (stop_tx, stop_rx) = oneshot::channel();
        tokio::spawn(async move {
            let request =
                PromptRequest::new(session_id, vec![ContentBlock::Text(TextContent::new(text))]);
            let result = connection
                .send_request(request)
                .block_task()
                .await
                .map(|response| response.stop_reason)
                .map_err(|error| anyhow::anyhow!("prompt failed: {error}"));
            let _ = stop_tx.send(result);
        });
        TurnHandle { stop_rx }
    }

    /// Ends the TUI session: dropping the connection closes the child's
    /// stdin, and the SDK reaps the process on drop.
    pub fn shutdown(self) {
        drop(self);
    }
}

/// The awaitable result of one detached prompt turn.
pub struct TurnHandle {
    stop_rx: oneshot::Receiver<anyhow::Result<StopReason>>,
}

impl TurnHandle {
    /// Resolves with the turn's stop reason.
    pub async fn stop_reason(&mut self) -> anyhow::Result<StopReason> {
        (&mut self.stop_rx)
            .await
            .map_err(|error| anyhow::anyhow!("turn channel closed: {error}"))?
    }
}

#[cfg(test)]
mod latest_resolution_tests {
    use super::latest_session_id_in;

    #[test]
    fn max_jsonl_stem_wins_and_missing_or_empty_store_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Missing directory: loud error, never a silent fresh session.
        assert!(latest_session_id_in(root.join("nonexistent")).is_err());

        // Empty directory: same.
        assert!(latest_session_id_in(root.clone()).is_err());

        // Non-.jsonl files don't count; max stem wins.
        std::fs::write(root.join("01940000-0000-7000-8000-000000000001.jsonl"), "").unwrap();
        std::fs::write(root.join("01940000-0000-7000-8000-000000000002.jsonl"), "").unwrap();
        std::fs::write(root.join("notes.txt"), "").unwrap();
        assert_eq!(
            latest_session_id_in(root).unwrap(),
            "01940000-0000-7000-8000-000000000002"
        );
    }
}
