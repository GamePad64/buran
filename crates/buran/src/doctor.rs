//! First-run doctor: probe the config, MCP servers, and skills; offer ACT
//! when no MCP servers are configured.
//!
//! The logic (`run_doctor` -> `DoctorReport`) is separate from the
//! rendering (`render_report`) so tests assert on structured findings.
//! stdout use is fine here — doctor is not `serve`.

use std::path::Path;
use std::time::Duration;

use crate::config::Config;

/// One MCP server's probe outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProbe {
    pub name: String,
    /// The launch line as configured, e.g.
    /// `npx -y @modelcontextprotocol/server-filesystem /tmp`.
    pub command_line: String,
    pub healthy: bool,
    /// Empty when healthy; the spawn/handshake failure reason otherwise.
    pub detail: String,
}

/// Provider presence check; never carries the key itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// Whether an API key resolves — the key value never reaches the report.
    pub api_key_present: bool,
}

/// Structured doctor findings. `doctor()` renders this; tests assert on it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// Human-readable config problems, each naming the offending file.
    pub config_errors: Vec<String>,
    pub provider: Option<ProviderStatus>,
    pub servers: Vec<ServerProbe>,
    /// Skill names found in the configured skills dirs.
    pub skills: Vec<String>,
    /// No MCP servers are configured (and the config is parseable), so the
    /// doctor suggests ACT.
    pub act_offer: bool,
}

/// ACT's `.mcp.json` snippet: a config suggestion printed as text, not a
/// code branch. The brief pins the launch line against ACT docs before the
/// command string is advertised anywhere outside doctor output; until that
/// confirmation the snippet carries the documented-shape placeholder.
const ACT_SNIPPET: &str = r#"{
  "mcpServers": {
    "act-fs": {
      "command": "npx",
      "args": ["-y", "act", "filesystem", "<project dir>"]
    }
  }
}"#;

/// Loads config, probes every MCP server, scans skills. Malformed config
/// files become named errors in the report instead of aborting: the
/// doctor's whole job is reporting what is broken.
///
/// `home_dir` replaces `HOME` for the global config lookup (tests inject a
/// fixture dir; production passes the real home).
pub async fn run_doctor(
    home_dir: Option<&Path>,
    project_dir: Option<&Path>,
    probe_timeout: Duration,
) -> DoctorReport {
    let home_var = home_dir.map(|home| (String::from("HOME"), home.to_path_buf()));
    let config = match Config::load(project_dir, |var| {
        home_var
            .as_ref()
            .and_then(|(name, path)| (name == var).then(|| path.to_string_lossy().into_owned()))
    }) {
        Ok(config) => config,
        Err(error) => {
            return DoctorReport {
                config_errors: vec![error.to_string()],
                ..DoctorReport::default()
            };
        }
    };

    let provider = config.provider.as_ref().map(|p| ProviderStatus {
        name: p.name.clone(),
        base_url: p.base_url.clone(),
        model: p.model.clone(),
        api_key_present: p.api_key.resolve().is_ok(),
    });

    let skills = crate::skills::scan(&config.skills_dirs)
        .into_iter()
        .map(|s| s.name)
        .collect();

    let mut report = DoctorReport {
        config_errors: Vec::new(),
        provider,
        servers: Vec::new(),
        skills,
        act_offer: false,
    };

    if config.mcp_servers.is_empty() {
        // Zero configured servers (neither config.toml nor .mcp.json
        // declared one): offer ACT. A malformed file counts against the
        // offer — a broken config is not an empty config.
        report.act_offer = report.config_errors.is_empty();
        return report;
    }

    let manager = match crate::mcp::McpManager::start(&config.mcp_servers, probe_timeout).await {
        Ok(manager) => manager,
        Err(error) => {
            // Manager construction itself never fails on broken servers
            // (they are recorded), so this is a genuinely unexpected
            // error; report it and stop.
            report.config_errors.push(error.to_string());
            return report;
        }
    };
    let failures: std::collections::BTreeMap<String, String> = manager
        .failures()
        .await
        .iter()
        .map(|(name, reason)| (name.clone(), reason.clone()))
        .collect();

    for server in &config.mcp_servers {
        let probe = match failures.get(&server.name) {
            Some(reason) => ServerProbe {
                name: server.name.clone(),
                command_line: server_command_line(server),
                healthy: false,
                detail: reason.clone(),
            },
            None => ServerProbe {
                name: server.name.clone(),
                command_line: server_command_line(server),
                healthy: true,
                detail: String::new(),
            },
        };
        report.servers.push(probe);
    }
    manager.shutdown().await;

    report
}

/// `command` + args, one line for the report.
fn server_command_line(server: &crate::config::McpServerConfig) -> String {
    let mut line = server.command.clone();
    for arg in &server.args {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// Renders the report for the terminal. Never prints provider API keys.
pub fn render_report(report: &DoctorReport) -> String {
    let mut out = String::from("buran doctor\n============\n\n");

    out.push_str("provider:\n");
    match &report.provider {
        Some(p) => {
            out.push_str(&format!(
                "  name: {}\n  base_url: {}\n  model: {}\n  api key: {}\n",
                p.name,
                p.base_url,
                p.model,
                if p.api_key_present {
                    "present"
                } else {
                    "MISSING"
                }
            ));
        }
        None => out.push_str("  not configured (set [provider] in ~/.config/buran/config.toml)\n"),
    }

    out.push_str("\nskills:\n");
    if report.skills.is_empty() {
        out.push_str("  none found\n");
    } else {
        out.push_str(&format!("  {} found:\n", report.skills.len()));
        for name in &report.skills {
            out.push_str(&format!("  - {name}\n"));
        }
    }

    out.push_str("\nmcp servers:\n");
    if report.servers.is_empty() {
        out.push_str("  none configured\n");
    } else {
        for server in &report.servers {
            if server.healthy {
                out.push_str(&format!(
                    "  [ok]      {} ({})\n",
                    server.name, server.command_line
                ));
            } else {
                out.push_str(&format!(
                    "  [failed]  {} ({}): {}\n",
                    server.name, server.command_line, server.detail
                ));
            }
        }
    }

    if report.act_offer {
        out.push_str(
            "\nNo MCP servers configured.\n\
             Tip: ACT (MCP-over-stdio) is a ready-made tool layer; add this to .mcp.json\n\
             in your project root to enable its filesystem component:\n\n",
        );
        out.push_str(ACT_SNIPPET);
        out.push('\n');
    }

    if !report.config_errors.is_empty() {
        out.push_str("\nconfig errors:\n");
        for error in &report.config_errors {
            out.push_str(&format!("  - {error}\n"));
        }
    }

    out
}
