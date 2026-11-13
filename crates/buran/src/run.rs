//! CLI entry points. `serve` never touches stdout (it is the ACP channel);
//! `doctor`, `mcp list`, and `sessions` are commands where stdout is fine.

use anyhow::Result;

/// Resume/fork are pre-flight validation plus a stderr hint: ACP clients
/// attach to stored conversations via `session/loadSession` (ADR 0007
/// canon; the store is the contract), so serve only ensures the
/// transcript is reachable before the ACP channel opens.
pub fn serve(resume: Option<String>, fork: Option<String>) -> Result<()> {
    let store = crate::store::Store::new(crate::store::Store::default_root());
    let effective = if let Some(id) = fork {
        let cwd = std::env::current_dir()?;
        let new_file = store
            .fork(
                &id,
                crate::session::TranscriptMeta {
                    model: String::new(),
                    provider: String::new(),
                    started_at: crate::store::rfc3339_now(),
                    ended_at: None,
                    cwd,
                    forked_from: None, // store.fork sets the lineage
                },
            )
            .map_err(|e| anyhow::anyhow!("{e} (see buran sessions list)"))?;
        // stderr: under `serve` stdout is the ACP JSON-RPC channel (AGENTS.md).
        eprintln!("forked {id} -> {}", new_file.id());
        new_file.id().to_string()
    } else if let Some(target) = resume {
        if target == "latest" {
            store
                .latest()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .ok_or_else(|| anyhow::anyhow!("no stored sessions to resume"))?
        } else {
            target
        }
    } else {
        return crate::acp::serve();
    };
    // Fail fast on an unknown id, before stdout becomes the ACP channel.
    store
        .open(&effective)
        .map_err(|e| anyhow::anyhow!("{e} (see buran sessions list)"))?;
    eprintln!("resuming session {effective}: connect a client and load it via session/loadSession");
    crate::acp::serve()
}

/// Pure CLI command — `serve` is not running, so stdout is free for
/// human output (the stdout ban applies under `serve` only).
pub fn sessions_list() -> Result<()> {
    let store = crate::store::Store::new(crate::store::Store::default_root());
    let infos = store.list().map_err(|e| anyhow::anyhow!("{e}"))?;
    if infos.is_empty() {
        println!("no stored sessions");
        return Ok(());
    }
    for i in infos {
        println!(
            "{}\t{}\t{}\t{}",
            i.id,
            i.meta.started_at,
            i.meta.model,
            i.meta.forked_from.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

pub fn sessions_rm(id: &str) -> Result<()> {
    let store = crate::store::Store::new(crate::store::Store::default_root());
    store
        .remove(id)
        .map_err(|e| anyhow::anyhow!("{e} (see buran sessions list)"))
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
