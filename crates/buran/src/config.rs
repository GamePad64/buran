//! Configuration loading: global `config.toml`, project `buran.config.toml`,
//! and project `.mcp.json`.
//!
//! Precedence (ADR 0003): global file first, project file overrides provider
//! fields and appends skills dirs, `.mcp.json` appends servers (project wins
//! name collisions). `BURAN_API_KEY` overrides any configured API key.
//!
//! Env access goes through injectable readers so tests never touch process env.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Reads one environment variable. Production reads `std::env`; tests inject.
pub trait Env {
    fn get(&self, name: &str) -> Option<String>;
}

/// Reads the real process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

impl<F> Env for F
where
    F: for<'a> Fn(&'a str) -> Option<String>,
{
    fn get(&self, name: &str) -> Option<String> {
        self(name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiKey {
    Explicit(String),
    FromEnv(String),
}

impl ApiKey {
    /// Resolves against the real process environment.
    pub fn resolve(&self) -> Result<String, ConfigError> {
        self.resolve_with(ProcessEnv)
    }

    /// Resolves against an injected environment (tests).
    pub fn resolve_with(&self, env: impl Env) -> Result<String, ConfigError> {
        match self {
            ApiKey::Explicit(s) => Ok(s.clone()),
            ApiKey::FromEnv(var) => env.get(var).ok_or_else(|| ConfigError::Parse {
                path: PathBuf::from("<env>"),
                source: format!("api key env var {var} is not set").into(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(default)]
pub struct ProviderToml {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    pub context_window: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_key: ApiKey,
    /// The model's context window in tokens; `None` = unknown, callers
    /// apply the 128k hard default (ADR 0006).
    pub context_window: Option<usize>,
}

/// `[compaction]` settings (ADR 0006 trigger policy). Defaults keep
/// auto-compaction ON: real work should not think about context growth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Compact automatically after a turn when the window estimate
    /// crosses the threshold.
    pub auto: bool,
    /// Tokens reserved under the context window for the model's reply
    /// and tool traffic; auto-compact triggers above
    /// `context_window − reserve`.
    pub reserve: usize,
    /// Inline provider for summaries (ADR 0006, the Aider use case:
    /// summarize with a cheaper model). `None` = summaries use the
    /// session provider. Inline, not a provider registry — one use case
    /// does not justify a subsystem.
    pub summarizer: Option<SummarizerProvider>,
}

/// `[compaction.summarizer]`: the minimal OpenAI-compatible provider
/// shape a summary call needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SummarizerProvider {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: true,
            reserve: 8192,
            summarizer: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpServerToml {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl From<McpServerToml> for McpServerConfig {
    fn from(t: McpServerToml) -> Self {
        Self {
            name: t.name,
            command: t.command,
            args: t.args,
            env: t.env,
        }
    }
}

/// `.mcp.json` entry: `command` is required, `args`/`env` optional
/// (Claude-Code-style de-facto standard format).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpJsonServer {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct McpJsonFile {
    #[serde(rename = "mcpServers", default)]
    mcp_servers: BTreeMap<String, McpJsonServer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionPolicy {
    AllowAll,
    DenyAll,
    #[default]
    Ask,
}

impl PermissionPolicy {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "allow_all" => Some(Self::AllowAll),
            "deny_all" => Some(Self::DenyAll),
            "ask" => Some(Self::Ask),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub provider: Option<ProviderConfig>,
    pub mcp_servers: Vec<McpServerConfig>,
    pub skills_dirs: Vec<PathBuf>,
    pub permission: PermissionPolicy,
    /// How long `session/request_permission` waits for the client before
    /// denying (Ask policy). Not user-configurable in v1.
    pub permission_timeout: std::time::Duration,
    /// Sessions-store root override; `None` uses `Store::default_root()`
    /// (`~/.local/state/buran/sessions`). Not a config-file key (YAGNI,
    /// sessions store spec): the field exists for tests and embedding.
    pub sessions_dir: Option<PathBuf>,
    /// `[compaction]` trigger policy (ADR 0006).
    pub compaction: CompactionConfig,
    /// `[subagents]` delegation policy (ADR 0011): opt-in server.
    pub subagents: SubagentsConfig,
    /// `[hooks]` user event hooks (ADR 0012): commands run at harness
    /// events; fail-open.
    pub hooks: HooksConfig,
    /// `[agentsmd]` searched instruction-file names (default
    /// `["AGENTS.md"]`; adding "CLAUDE.md" absorbs Claude Code projects).
    pub agentsmd: AgentsMdConfig,
    /// Parent session id when this server runs AS a subagent
    /// (`--subagent-of`); newSession writes it into the transcript head.
    /// Not a config-file key — a CLI-injected runtime field.
    pub subagent_parent: Option<String>,
}

/// `[agentsmd]`: which instruction-file names the loader searches
/// (default: AGENTS.md only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsMdConfig {
    pub files: Vec<String>,
}

impl Default for AgentsMdConfig {
    fn default() -> Self {
        Self {
            files: buran_default_names()
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// The default searched names, shared with the loader.
pub fn buran_default_names() -> &'static [&'static str] {
    &["AGENTS.md"]
}

/// `[hooks]`: user event-hook commands (ADR 0012). One command per
/// event; the logic lives entirely outside the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HooksConfig {
    /// Run after each tool result; may rewrite it.
    pub post_tool_use: Option<String>,
    /// Run when a turn resolves; observe-only.
    pub on_turn_end: Option<String>,
    /// Per-hook wall-clock cap, seconds.
    pub timeout_secs: u64,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            post_tool_use: None,
            on_turn_end: None,
            timeout_secs: 10,
        }
    }
}

/// `[subagents]`: the delegation server's knobs. Disabled by default —
/// the user opts in by adding the section and registering the server in
/// `.mcp.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentsConfig {
    pub enabled: bool,
    /// Child provider override; `None` = children inherit the parent's.
    pub provider: Option<SummarizerProvider>,
    /// Hard cap of live child agents.
    pub max_children: usize,
    /// Default child wall-clock, seconds.
    pub timeout_secs: u64,
}

impl Default for SubagentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: None,
            max_children: 4,
            timeout_secs: 600,
        }
    }
}

/// Drops the `subagents` MCP entry from a config: the depth-≤-2 rule,
/// mechanically. A child spawned with the filtered config has no spawn
/// tool, so grandchildren are impossible — one filter, no harness branch.
pub fn filter_for_subagent(mut config: Config) -> Config {
    config
        .mcp_servers
        .retain(|server| server.name != "subagents");
    config
}

/// Serialized `[provider]` / top-level shape of a config file.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ConfigFile {
    provider: ProviderToml,
    mcp_servers: Vec<McpServerToml>,
    skills_dirs: Vec<String>,
    permissions: Option<PermissionsToml>,
    compaction: Option<CompactionToml>,
    subagents: Option<SubagentsToml>,
    hooks: Option<HooksToml>,
    agentsmd: Option<AgentsMdToml>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct AgentsMdToml {
    files: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct HooksToml {
    post_tool_use: Option<String>,
    on_turn_end: Option<String>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct SubagentsToml {
    enabled: Option<bool>,
    provider: Option<SummarizerProvider>,
    max_children: Option<usize>,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct CompactionToml {
    auto: Option<bool>,
    reserve: Option<usize>,
    summarizer: Option<SummarizerProvider>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct PermissionsToml {
    policy: Option<String>,
}

impl Config {
    /// Loads config from the real HOME and real process env.
    pub fn load(
        project_dir: Option<&Path>,
        home_reader: impl Fn(&str) -> Option<String> + Clone,
    ) -> Result<Self, ConfigError> {
        Self::load_with_env(project_dir, home_reader, ProcessEnv)
    }

    /// Full loader with an injected `BURAN_API_KEY` reader (tests).
    pub fn load_with_env(
        project_dir: Option<&Path>,
        home_reader: impl Fn(&str) -> Option<String>,
        api_key_env: impl Env,
    ) -> Result<Self, ConfigError> {
        let home = home_reader("HOME");

        let mut provider_toml = ProviderToml::default();
        let mut mcp_servers: Vec<McpServerConfig> = Vec::new();
        let mut skills_dirs: Vec<PathBuf> = Vec::new();
        let mut permission = PermissionPolicy::Ask;
        // Compaction fields merge field-wise like the provider: global
        // sets the baseline, project overrides individual keys.
        let mut compaction = CompactionConfig::default();
        let mut compaction_auto: Option<bool> = None;
        let mut compaction_reserve: Option<usize> = None;
        let mut compaction_summarizer: Option<SummarizerProvider> = None;
        let mut subagents = SubagentsConfig::default();
        let mut hooks = HooksConfig::default();
        let mut agentsmd_files: Option<Vec<String>> = None;
        let mut seen_provider = false;

        if let Some(home) = home.as_deref() {
            let global = Path::new(home).join(".config/buran/config.toml");
            if global.is_file() {
                let file = read_config_file(&global)?;
                provider_toml = file.provider;
                seen_provider = provider_toml.base_url.is_some() || provider_toml.model.is_some();
                mcp_servers.extend(file.mcp_servers.into_iter().map(Into::into));
                skills_dirs.extend(file.skills_dirs.iter().map(PathBuf::from));
                if let Some(p) = file.permissions.and_then(|p| p.policy)
                    && let Some(policy) = PermissionPolicy::parse(&p)
                {
                    permission = policy;
                }
                if let Some(c) = &file.compaction {
                    compaction_auto = c.auto;
                    compaction_reserve = c.reserve;
                    compaction_summarizer = c.summarizer.clone();
                }
                if let Some(a) = &file.agentsmd
                    && a.files.is_some()
                {
                    agentsmd_files = a.files.clone();
                }
                if let Some(h) = &file.hooks {
                    if h.post_tool_use.is_some() {
                        hooks.post_tool_use = h.post_tool_use.clone();
                    }
                    if h.on_turn_end.is_some() {
                        hooks.on_turn_end = h.on_turn_end.clone();
                    }
                    if let Some(secs) = h.timeout_secs {
                        hooks.timeout_secs = secs;
                    }
                }
                if let Some(a) = &file.agentsmd
                    && a.files.is_some()
                {
                    agentsmd_files = a.files.clone();
                }
                if let Some(s) = &file.subagents {
                    if let Some(enabled) = s.enabled {
                        subagents.enabled = enabled;
                    }
                    if s.provider.is_some() {
                        subagents.provider = s.provider.clone();
                    }
                    if let Some(max_children) = s.max_children {
                        subagents.max_children = max_children;
                    }
                    if let Some(timeout_secs) = s.timeout_secs {
                        subagents.timeout_secs = timeout_secs;
                    }
                }
            }
        }

        if let Some(project_dir) = project_dir {
            let project_config = project_dir.join("buran.config.toml");
            if project_config.is_file() {
                let file = read_config_file(&project_config)?;
                // Project provider fields override; unspecified fall back to global.
                merge_provider(&mut provider_toml, file.provider);
                seen_provider = seen_provider
                    || (provider_toml.base_url.is_some() && provider_toml.model.is_some());
                skills_dirs.extend(file.skills_dirs.iter().map(PathBuf::from));
                if let Some(p) = file.permissions.and_then(|p| p.policy)
                    && let Some(policy) = PermissionPolicy::parse(&p)
                {
                    permission = policy;
                }
                if let Some(c) = &file.compaction {
                    if c.auto.is_some() {
                        compaction_auto = c.auto;
                    }
                    if c.reserve.is_some() {
                        compaction_reserve = c.reserve;
                    }
                    if c.summarizer.is_some() {
                        compaction_summarizer = c.summarizer.clone();
                    }
                }
                if let Some(a) = &file.agentsmd
                    && a.files.is_some()
                {
                    agentsmd_files = a.files.clone();
                }
                if let Some(h) = &file.hooks {
                    if h.post_tool_use.is_some() {
                        hooks.post_tool_use = h.post_tool_use.clone();
                    }
                    if h.on_turn_end.is_some() {
                        hooks.on_turn_end = h.on_turn_end.clone();
                    }
                    if let Some(secs) = h.timeout_secs {
                        hooks.timeout_secs = secs;
                    }
                }
                if let Some(a) = &file.agentsmd
                    && a.files.is_some()
                {
                    agentsmd_files = a.files.clone();
                }
                if let Some(s) = &file.subagents {
                    if let Some(enabled) = s.enabled {
                        subagents.enabled = enabled;
                    }
                    if s.provider.is_some() {
                        subagents.provider = s.provider.clone();
                    }
                    if let Some(max_children) = s.max_children {
                        subagents.max_children = max_children;
                    }
                    if let Some(timeout_secs) = s.timeout_secs {
                        subagents.timeout_secs = timeout_secs;
                    }
                }
                // Project [[mcp_servers]] merge like lists do (README:
                // "lists concatenate") — the silent drop here meant a
                // project toml could not add an MCP server at all.
                mcp_servers.extend(file.mcp_servers.into_iter().map(Into::into));
            }

            let mcp_json = project_dir.join(".mcp.json");
            if mcp_json.is_file() {
                let file = read_mcp_json(&mcp_json)?;
                for (name, server) in file.mcp_servers {
                    // Project wins name collisions: replace, keeping first-seen order.
                    if let Some(existing) = mcp_servers.iter_mut().find(|s| s.name == name) {
                        *existing = McpServerConfig {
                            name,
                            command: server.command,
                            args: server.args,
                            env: server.env,
                        };
                    } else {
                        mcp_servers.push(McpServerConfig {
                            name,
                            command: server.command,
                            args: server.args,
                            env: server.env,
                        });
                    }
                }
            }
        }

        let provider = seen_provider.then(|| ProviderConfig {
            name: provider_toml.name.unwrap_or_else(|| "default".into()),
            base_url: provider_toml.base_url.unwrap_or_default(),
            model: provider_toml.model.unwrap_or_default(),
            api_key: if api_key_env.get("BURAN_API_KEY").is_some() {
                ApiKey::FromEnv("BURAN_API_KEY".into())
            } else if let Some(key) = provider_toml.api_key {
                ApiKey::Explicit(key)
            } else if let Some(var) = provider_toml.api_key_env {
                ApiKey::FromEnv(var)
            } else {
                ApiKey::FromEnv("BURAN_API_KEY".into())
            },
            context_window: provider_toml.context_window,
        });

        if let Some(auto) = compaction_auto {
            compaction.auto = auto;
        }
        if let Some(reserve) = compaction_reserve {
            compaction.reserve = reserve;
        }
        compaction.summarizer = compaction_summarizer;

        Ok(Self {
            provider,
            mcp_servers,
            skills_dirs,
            permission,
            permission_timeout: std::time::Duration::from_secs(120),
            sessions_dir: None,
            compaction,
            subagents,
            subagent_parent: None,
            hooks,
            agentsmd: AgentsMdConfig {
                files: agentsmd_files.unwrap_or_else(|| {
                    buran_default_names()
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }),
            },
        })
    }
}

fn merge_provider(base: &mut ProviderToml, over: ProviderToml) {
    if over.name.is_some() {
        base.name = over.name;
    }
    if over.base_url.is_some() {
        base.base_url = over.base_url;
    }
    if over.api_key.is_some() {
        base.api_key = over.api_key;
    }
    if over.api_key_env.is_some() {
        base.api_key_env = over.api_key_env;
    }
    if over.model.is_some() {
        base.model = over.model;
    }
    if over.context_window.is_some() {
        base.context_window = over.context_window;
    }
}

fn read_config_file(path: &Path) -> Result<ConfigFile, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

fn read_mcp_json(path: &Path) -> Result<McpJsonFile, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}
