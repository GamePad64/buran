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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_key: ApiKey,
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
}

/// Serialized `[provider]` / top-level shape of a config file.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ConfigFile {
    provider: ProviderToml,
    mcp_servers: Vec<McpServerToml>,
    skills_dirs: Vec<String>,
    permissions: Option<PermissionsToml>,
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
        });

        Ok(Self {
            provider,
            mcp_servers,
            skills_dirs,
            permission,
            permission_timeout: std::time::Duration::from_secs(120),
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
