//! `McpManager`: owns one rmcp client per configured MCP server.

use std::collections::BTreeMap;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;

use super::registry::{ToolCatalog, ToolEntry};
use crate::config::McpServerConfig;

/// One rmcp client session with a spawned MCP server process.
pub type McpClient = RunningService<RoleClient, ()>;

/// The source of a `tools/call` failure: an rmcp service error, or the
/// manager's own request timeout.
#[derive(Debug, thiserror::Error)]
pub enum CallErrorSource {
    #[error("{0}")]
    Service(#[from] Box<rmcp::ServiceError>),
    #[error("no response within {CALL_TIMEOUT:?}")]
    TimedOut,
}

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("failed to spawn MCP server {server}: {source}")]
    Spawn {
        server: String,
        source: std::io::Error,
    },
    #[error("MCP handshake with {server} failed: {source}")]
    Handshake {
        server: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("listing tools of {server} failed: {source}")]
    ListTools {
        server: String,
        source: Box<rmcp::ServiceError>,
    },
    #[error("calling {server} failed: {source}")]
    Call {
        server: String,
        source: CallErrorSource,
    },
    #[error("unknown tool: {name}")]
    UnknownTool { name: String },
    #[error("MCP server {server} did not complete handshake in time")]
    Timeout { server: String },
}

/// The model-visible result of a tool call: text content plus the MCP
/// `is_error` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

/// Ceilings on post-handshake MCP requests: a server that handshakes fine
/// and then wedges (slow npx spawn, dead network tool) must degrade to a
/// per-server tool-error, never block the LLM loop forever. The plan's
/// stated invariant. `tools/list` is metadata — healthy servers answer in
/// milliseconds; `tools/call` may run legitimate long work, so it gets a
/// wider bound.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Spawned MCP servers plus a record of the ones that failed.
///
/// Failures do not abort startup: the manager degrades to the servers that
/// work, and doctor (Task 12) reports the rest.
pub struct McpManager {
    clients: BTreeMap<String, McpClient>,
    failures: Vec<(String, String)>,
}

impl McpManager {
    /// Spawns every server and performs the MCP handshake, each bounded by
    /// `timeout`. Broken servers land in `failures()` as `(name, reason)`.
    pub async fn start(configs: &[McpServerConfig], timeout: Duration) -> Result<Self, McpError> {
        let mut clients = BTreeMap::new();
        let mut failures = Vec::new();

        for config in configs {
            let name = config.name.clone();
            match start_one(config, timeout).await {
                Ok(client) => {
                    clients.insert(name, client);
                }
                Err(reason) => {
                    tracing::warn!(server = %name, reason, "MCP server failed to start");
                    failures.push((name, reason));
                }
            }
        }

        Ok(Self { clients, failures })
    }

    /// `(name, reason)` for every server that failed to spawn or handshake.
    pub fn failures(&self) -> &[(String, String)] {
        &self.failures
    }

    /// The catalog of all tools across all healthy servers, qualified by
    /// server name: `"server-name/tool-name"`. Bounded per server: a
    /// silent server costs the catalog its timeout once, then drops out.
    pub async fn catalog(&self) -> ToolCatalog {
        let mut entries = Vec::new();
        for (server, client) in &self.clients {
            match tokio::time::timeout(LIST_TIMEOUT, client.list_all_tools()).await {
                Ok(Ok(tools)) => {
                    for tool in tools {
                        entries.push(ToolEntry {
                            server: server.clone(),
                            tool,
                        });
                    }
                }
                Ok(Err(source)) => {
                    tracing::warn!(server = %server, %source, "tools/list failed");
                }
                Err(_) => {
                    // The handshake succeeded but the server stopped
                    // answering: degrade to zero tools from it rather
                    // than freezing every future prompt.
                    tracing::warn!(server = %server, "tools/list timed out");
                }
            }
        }
        ToolCatalog::new(entries)
    }

    /// Calls a qualified tool (`"server-name/tool-name"`).
    ///
    /// Tool results are text-joined; MCP `is_error` results come back as
    /// `ToolOutput { is_error: true }`, not as `Err` — a failed tool is
    /// model-visible content, the model decides what to do with it.
    /// Bounded: a wedged server surfaces as a model-visible timeout error,
    /// never blocks the turn.
    pub async fn call(
        &self,
        qualified: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolOutput, McpError> {
        let (server, tool_name) =
            split_qualified(qualified).ok_or_else(|| McpError::UnknownTool {
                name: qualified.to_string(),
            })?;
        let client = self
            .clients
            .get(server)
            .ok_or_else(|| McpError::UnknownTool {
                name: qualified.to_string(),
            })?;

        let arguments = match arguments {
            serde_json::Value::Object(map) => map,
            _ => Default::default(),
        };
        let call = client
            .call_tool(CallToolRequestParams::new(tool_name.to_string()).with_arguments(arguments));
        let result = tokio::time::timeout(CALL_TIMEOUT, call)
            .await
            .map_err(|_| McpError::Call {
                server: server.to_string(),
                source: CallErrorSource::TimedOut,
            })?
            .map_err(|source| McpError::Call {
                server: server.to_string(),
                source: CallErrorSource::Service(Box::new(source)),
            })?;

        let text = result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput {
            text,
            is_error: result.is_error == Some(true),
        })
    }

    /// Shuts every server down, bounded per server; transports kill children
    /// on drop regardless.
    pub async fn shutdown(self) {
        for (name, mut client) in self.clients {
            if let Err(error) = client.close_with_timeout(Duration::from_secs(3)).await {
                tracing::warn!(server = %name, %error, "MCP server shutdown error");
            }
        }
    }
}

/// Splits `"server/tool"` into `("server", "tool")`. Tool names may contain
/// `/`, so the first `/` is the separator.
fn split_qualified(qualified: &str) -> Option<(&str, &str)> {
    let (server, tool) = qualified.split_once('/')?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

async fn start_one(config: &McpServerConfig, timeout: Duration) -> Result<McpClient, String> {
    let mut command = tokio::process::Command::new(&config.command);
    command.args(&config.args).envs(&config.env);

    let transport = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|source| {
            McpError::Spawn {
                server: config.name.clone(),
                source,
            }
            .to_string()
        })?
        .0;

    let name = config.name.clone();
    match tokio::time::timeout(timeout, ().serve(transport)).await {
        Err(_) => Err(McpError::Timeout { server: name }.to_string()),
        Ok(Err(source)) => Err(McpError::Handshake {
            server: name,
            source: Box::new(source),
        }
        .to_string()),
        Ok(Ok(client)) => Ok(client),
    }
}
