//! CLI entry points. `serve` never touches stdout (it is the ACP channel);
//! `doctor` and `mcp list` are interactive commands where stdout is fine.

use anyhow::Result;

pub fn serve() -> Result<()> {
    crate::acp::serve()
}

/// Doctor: run the probe, render the report to stdout.
pub async fn doctor() -> Result<()> {
    let project_dir = std::env::current_dir().ok();
    let home_dir = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let report = crate::doctor::run_doctor(
        home_dir.as_deref(),
        project_dir.as_deref(),
        std::time::Duration::from_secs(5),
    )
    .await;
    print!("{}", crate::doctor::render_report(&report));
    Ok(())
}

/// `mcp list`: the merged server list from config + `.mcp.json`, with
/// command lines. Does not spawn servers — fast and offline.
pub fn mcp_list() -> Result<()> {
    let project_dir = std::env::current_dir().ok();
    let config = buran_config(project_dir.as_deref())?;

    if config.mcp_servers.is_empty() {
        println!("no MCP servers configured");
        return Ok(());
    }
    for server in &config.mcp_servers {
        let mut line = server.command.clone();
        for arg in &server.args {
            line.push(' ');
            line.push_str(arg);
        }
        println!("{}\t{}", server.name, line);
    }
    Ok(())
}

/// Loads config the way the CLI sees it: real HOME, real env.
fn buran_config(
    project_dir: Option<&std::path::Path>,
) -> Result<crate::config::Config, crate::config::ConfigError> {
    crate::config::Config::load(project_dir, |var| std::env::var(var).ok())
}
