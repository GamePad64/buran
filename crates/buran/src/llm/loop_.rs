//! The agent loop: one user prompt → streamed LLM turns → tool calls →
//! final answer (ADR 0001).
//!
//! Events flow out through an `mpsc` channel; permission decisions flow in
//! through another; a `watch` channel carries cancellation. Permission
//! *queries* ride the events channel (`TurnEvent::PermissionQuery`) because
//! the loop only holds the decision receiver — the server task watching the
//! events applies the policy (or asks the ACP client) and sends the decision
//! back.
//!
//! The loop owns no I/O beyond the LLM client and the `ToolCaller`.

use std::collections::BTreeMap;
use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use super::client::{ChatEvent, LlmClient};
use super::types::{
    ChatMessage, ChatRequest, FinishReason, FunctionCall, FunctionType, ToolCallReq, ToolDef,
    ToolDefFn,
};
use crate::mcp::{McpError, ToolOutput};

/// How tools reach the loop. `McpManager` implements it in production;
/// tests stub it. Object-safe via boxed futures (`Arc<dyn ToolCaller>`).
pub trait ToolCaller: Send + Sync {
    fn call<'a>(
        &'a self,
        qualified: &str,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, McpError>> + Send + 'a>>;
}

impl ToolCaller for crate::mcp::McpManager {
    fn call<'a>(
        &'a self,
        qualified: &str,
        args: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, McpError>> + Send + 'a>> {
        let qualified = qualified.to_string();
        Box::pin(async move {
            // Explicit path: the inherent method wins name resolution and
            // calling `self.call(..)` here would recurse into the trait.
            crate::mcp::McpManager::call(self, &qualified, args).await
        })
    }
}

/// One turn's event stream, as consumed by the ACP server or a test.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    TextDelta(String),
    ToolCallStarted {
        call_id: String,
        name: String,
        args_pretty: String,
    },
    /// The loop wants to run `server.tool` (`call_id` correlates with the
    /// started tool call); whoever watches the events stream must answer on
    /// the decision channel.
    PermissionQuery(PermissionQuery),
    ToolCallFinished {
        call_id: String,
        output: ToolOutput,
    },
    TurnDone(FinishReason),
}

/// A permission question from the loop: may `server` run `tool`?
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionQuery {
    pub server: String,
    pub tool: String,
    pub call_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
}

/// Conversation history for one session. The system prompt is kept
/// separately (rebuilt per request); `history` is what has been said.
#[derive(Debug, Default, Clone)]
pub struct SessionState {
    pub history: Vec<ChatMessage>,
    /// Message indices that start a compaction window (v1.5 storage hook).
    pub compaction_points: Vec<usize>,
}

/// What a finished turn produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub finish: FinishReason,
    pub tool_calls_made: u32,
}

/// Resolves once cancellation is requested; parks forever if the sender is
/// gone (no further cancel can ever arrive).
async fn wait_for_cancel(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            pending::<()>().await;
        }
    }
}

/// The loop for one session's LLM conversation.
pub struct AgentLoop {
    llm: LlmClient,
    tools: Arc<dyn ToolCaller>,
    system: String,
    tool_defs: Vec<ToolDef>,
    /// Wire name `server__tool` → MCP qualified `server/tool`.
    tool_map: BTreeMap<String, String>,
    max_turns: u32,
    /// Scanned skill descriptions; `<skill>Name</skill>` at the head of
    /// the turn's first assistant message loads the matching body
    /// (ADR 0004 — zero tools, the model just writes the tag).
    skills: Vec<crate::skills::SkillDescription>,
}

impl AgentLoop {
    pub fn new(llm: LlmClient, tools: Arc<dyn ToolCaller>, system: String) -> Self {
        Self {
            llm,
            tools,
            system,
            tool_defs: Vec::new(),
            tool_map: BTreeMap::new(),
            max_turns: 32,
            skills: Vec::new(),
        }
    }

    /// Registers the skills loadable via the `<skill>` tag convention.
    pub fn with_skills(mut self, skills: Vec<crate::skills::SkillDescription>) -> Self {
        self.skills = skills;
        self
    }

    /// Sets the tool catalog offered to the model: every tool becomes
    /// `server__tool` on the wire (double underscore: one string, no clash
    /// with OpenAI-style dotted naming).
    pub fn with_catalog(mut self, catalog: &crate::mcp::ToolCatalog) -> Self {
        self.tool_defs.clear();
        self.tool_map.clear();
        for entry in catalog.entries() {
            let qualified = format!("{}/{}", entry.server, entry.tool.name);
            let wire_name = format!("{}__{}", entry.server, entry.tool.name);
            let parameters =
                serde_json::to_value(&*entry.tool.input_schema).unwrap_or(serde_json::Value::Null);
            self.tool_defs.push(ToolDef {
                ty: FunctionType::Function,
                function: ToolDefFn {
                    name: wire_name.clone(),
                    description: entry.tool.description.as_ref().map(|d| d.to_string()),
                    parameters,
                },
            });
            self.tool_map.insert(wire_name, qualified);
        }
        self
    }

    /// Overrides the runaway-loop guard (default 32 LLM round trips).
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// Runs one user prompt to completion, streaming events on `events` and
    /// asking `permissions` for every tool call (decisions arrive on the
    /// paired sender; a closed channel or cancellation denies).
    ///
    /// Cancellation unwinds the turn into a clean `Stop` summary.
    pub async fn run_turn(
        &self,
        session: &mut SessionState,
        user_text: &str,
        events: mpsc::Sender<TurnEvent>,
        mut permissions: mpsc::Receiver<PermissionDecision>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<TurnSummary, super::LlmError> {
        session.history.push(ChatMessage::user(user_text));

        let mut tool_calls_made: u32 = 0;
        let mut finish = FinishReason::Stop;
        // `<skill>` detection applies to the turn's first assistant
        // message only (ADR 0004).
        let mut first_assistant_message = true;

        for _round in 0..self.max_turns {
            if *cancel.borrow_and_update() {
                return Ok(TurnSummary {
                    finish: FinishReason::Stop,
                    tool_calls_made,
                });
            }

            let mut messages = vec![ChatMessage::system(self.system.clone())];
            messages.extend(session.history.iter().cloned());
            let req = ChatRequest::new(self.llm.model_name().to_string(), messages);
            let req = if self.tool_defs.is_empty() {
                req
            } else {
                let mut req = req;
                req.tools = Some(self.tool_defs.clone());
                req
            };

            let (assistant_text, requested, round_finish) =
                self.stream_round(&req, &events, &mut cancel).await?;

            if *cancel.borrow_and_update() {
                // Cancel fired mid-stream: keep any partial text, stop clean.
                if !assistant_text.is_empty() {
                    session.history.push(ChatMessage::assistant(assistant_text));
                }
                return Ok(TurnSummary {
                    finish: FinishReason::Stop,
                    tool_calls_made,
                });
            }

            if requested.is_empty() {
                // Skill-tag convention: `<skill>Name</skill>` at the head
                // of the turn's first assistant message injects the body
                // and continues the turn; the tag text itself stays in the
                // transcript and the stream (ADR 0004).
                if first_assistant_message {
                    first_assistant_message = false;
                    if let Some(skill_message) = self.skill_continuation(&assistant_text) {
                        session.history.push(ChatMessage::assistant(assistant_text));
                        session.history.push(skill_message);
                        continue;
                    }
                }
                session.history.push(ChatMessage::assistant(assistant_text));
                finish = round_finish.unwrap_or(FinishReason::Stop);
                let _ = events.send(TurnEvent::TurnDone(finish)).await;
                return Ok(TurnSummary {
                    finish,
                    tool_calls_made,
                });
            }

            // Assistant asked for tools: record the ask, execute each.
            session
                .history
                .push(ChatMessage::assistant_with_tool_calls(requested.clone()));
            finish = round_finish.unwrap_or(FinishReason::ToolCalls);

            for call in &requested {
                let _ = events
                    .send(TurnEvent::ToolCallStarted {
                        call_id: call.id.clone(),
                        name: call.function.name.clone(),
                        args_pretty: pretty_arguments(&call.function.arguments),
                    })
                    .await;

                let decision = self
                    .ask_permission(call, &events, &mut permissions, &mut cancel)
                    .await;
                let tool_message = match decision {
                    PermissionDecision::Deny => ChatMessage::tool(
                        call.id.clone(),
                        r#"{"error":"denied by user"}"#.to_string(),
                    ),
                    PermissionDecision::Allow => {
                        let args =
                            serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                                .unwrap_or(serde_json::Value::Null);
                        let qualified = self
                            .tool_map
                            .get(call.function.name.as_str())
                            .cloned()
                            .ok_or_else(|| McpError::UnknownTool {
                                name: call.function.name.clone(),
                            });
                        let output = match qualified {
                            Ok(qualified) => self.tools.call(&qualified, args).await,
                            Err(error) => Err(error),
                        };
                        // A failed tool call is model-visible content, not a
                        // loop error: the model reads the text and decides.
                        let output = output.unwrap_or_else(|error| ToolOutput {
                            text: format!("tool error: {error}"),
                            is_error: true,
                        });
                        tool_calls_made += 1;
                        let _ = events
                            .send(TurnEvent::ToolCallFinished {
                                call_id: call.id.clone(),
                                output: output.clone(),
                            })
                            .await;
                        ChatMessage::tool(call.id.clone(), output.text)
                    }
                };
                session.history.push(tool_message);
            }
            // Next round: the model sees the tool results.
        }

        // max_turns exhausted without a natural stop: report the last finish
        // reason so callers see the loop was cut, not a clean end.
        let _ = events.send(TurnEvent::TurnDone(finish)).await;
        Ok(TurnSummary {
            finish,
            tool_calls_made,
        })
    }

    /// If `assistant_text` begins with `<skill>Name</skill>` naming a
    /// registered skill, returns the system-role message carrying the
    /// skill body for the continuation round. Any I/O failure degrades to
    /// no continuation (the turn just ends).
    fn skill_continuation(&self, assistant_text: &str) -> Option<ChatMessage> {
        let rest = assistant_text.strip_prefix("<skill>")?;
        let (name, remainder) = rest.split_once("</skill>")?;
        if name.is_empty() {
            return None;
        }
        let skill = self.skills.iter().find(|s| s.name == name)?;
        let body = crate::skills::load_body(skill).ok()?;
        let mut message = String::from("The user loaded the skill `");
        message.push_str(name);
        message.push_str("`. Follow its instructions:\n\n");
        message.push_str(&body.body_markdown);
        if !remainder.is_empty() {
            message.push_str("\n\nThe rest of your message was: ");
            message.push_str(remainder);
        }
        Some(ChatMessage::system(message))
    }

    /// Streams one LLM round: text deltas out as events, tool-call fragments
    /// accumulated by index. Returns the accumulated text, the completed
    /// tool-call requests, and the round's finish reason if one arrived.
    async fn stream_round(
        &self,
        req: &ChatRequest,
        events: &mpsc::Sender<TurnEvent>,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<(String, Vec<ToolCallReq>, Option<FinishReason>), super::LlmError> {
        let mut stream = self.llm.stream_chat(req).await?;

        let mut text = String::new();
        // index → (id, name, arguments): id+name arrive first, argument
        // fragments dribble in across chunks.
        let mut calls: BTreeMap<u32, (Option<String>, Option<String>, String)> = BTreeMap::new();
        let mut finish_seen: Option<FinishReason> = None;

        loop {
            tokio::select! {
                _ = wait_for_cancel(cancel) => {
                    return Ok((text, Vec::new(), finish_seen));
                }
                event = stream.next() => {
                    match event? {
                        None => break,
                        Some(ChatEvent::Done(reason)) => {
                            finish_seen = reason;
                            break;
                        }
                        Some(ChatEvent::Delta(delta)) => {
                            text.push_str(&delta);
                            let _ = events.send(TurnEvent::TextDelta(delta)).await;
                        }
                        Some(ChatEvent::ToolCallDelta { index, id, name_fragment, args_fragment }) => {
                            let entry = calls.entry(index).or_default();
                            if let Some(id) = id {
                                entry.0 = Some(id);
                            }
                            if let Some(name) = name_fragment {
                                entry.1.get_or_insert_with(String::new).push_str(&name);
                            }
                            if let Some(args) = args_fragment {
                                entry.2.push_str(&args);
                            }
                        }
                    }
                }
            }
        }

        let requested = calls
            .into_values()
            .filter_map(|(id, name, arguments)| {
                let name = name?;
                Some(ToolCallReq {
                    id: id.unwrap_or_else(|| format!("call_{}", nanos())),
                    ty: FunctionType::Function,
                    function: FunctionCall { name, arguments },
                })
            })
            .collect();

        Ok((text, requested, finish_seen))
    }

    /// Asks for permission to run `call`. The query rides the events
    /// channel; the decision arrives on `permissions` (closed channel →
    /// Deny; cancellation → Deny without waiting).
    async fn ask_permission(
        &self,
        call: &ToolCallReq,
        events: &mpsc::Sender<TurnEvent>,
        permissions: &mut mpsc::Receiver<PermissionDecision>,
        cancel: &mut watch::Receiver<bool>,
    ) -> PermissionDecision {
        // Wire name `server__tool` → (server, tool) for the query.
        let (server, tool) = match call.function.name.split_once("__") {
            Some((server, tool)) => (server.to_string(), tool.to_string()),
            None => (call.function.name.clone(), String::new()),
        };
        let _ = events
            .send(TurnEvent::PermissionQuery(PermissionQuery {
                server,
                tool,
                call_id: call.id.clone(),
            }))
            .await;

        tokio::select! {
            _ = wait_for_cancel(cancel) => PermissionDecision::Deny,
            decision = permissions.recv() => {
                decision.unwrap_or(PermissionDecision::Deny)
            }
        }
    }
}

fn pretty_arguments(arguments: &str) -> String {
    serde_json::from_str::<serde_json::Value>(arguments)
        .map(|value| serde_json::to_string_pretty(&value).unwrap_or_default())
        .unwrap_or_else(|_| arguments.to_string())
}

fn nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
