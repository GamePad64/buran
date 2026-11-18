//! CLI entry points. `serve` never touches stdout (it is the ACP channel);
//! `doctor`, `mcp list`, and `sessions` are commands where stdout is fine.

use anyhow::Result;

/// Resume/fork are pre-flight validation plus a stderr hint: ACP clients
/// attach to stored conversations via `session/loadSession` (ADR 0007
/// canon; the store is the contract), so serve only ensures the
/// transcript is reachable before the ACP channel opens. `listen` (ADR
/// 0010) switches the transport to HTTP on `addr`; it is transport-only —
/// resume/fork stay stdio for now and combining them is rejected rather
/// than silently ignored.
pub fn serve(
    resume: Option<String>,
    fork: Option<String>,
    listen: Option<std::net::SocketAddr>,
    subagent: bool,
    subagent_of: Option<String>,
) -> Result<()> {
    if listen.is_some() && (resume.is_some() || fork.is_some()) {
        anyhow::bail!(
            "--listen does not support --resume/--fork yet: HTTP connections are fresh sessions; drop --listen or --resume/--fork"
        );
    }
    if let Some(addr) = listen {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let bound = rt.block_on(crate::acp::serve_listen(addr, crate::llm::LlmClient::new))?;
        eprintln!("buran ACP listening on http://{bound}/acp (HTTP/1.1 + WebSocket)");
        // Serve until Ctrl-C: the listener task dies with the runtime.
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
        });
        return Ok(());
    }
    let store = crate::store::Store::new(crate::store::Store::default_root());
    let effective = if let Some(id) = fork {
        // A CLI fork is "the same conversation, new branch": the fork head
        // carries the source's model/provider/cwd (read-only, no open —
        // `open` would truncate a torn source tail, which fork must not
        // repair). started_at stays now: the branch starts when forked.
        let source_head = store
            .head(&id)
            .map_err(|e| anyhow::anyhow!("{e} (see buran sessions list)"))?;
        let new_file = store
            .fork(
                &id,
                crate::session::TranscriptMeta {
                    model: source_head.model,
                    provider: source_head.provider,
                    started_at: crate::store::rfc3339_now(),
                    ended_at: None,
                    cwd: source_head.cwd,
                    forked_from: None, // store.fork sets the lineage
                    subagent_of: None,
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
        return serve_stdio(subagent, subagent_of);
    };
    // Fail fast on an unknown id, before stdout becomes the ACP channel.
    store
        .open(&effective)
        .map_err(|e| anyhow::anyhow!("{e} (see buran sessions list)"))?;
    eprintln!("resuming session {effective}: connect a client and load it via session/loadSession");
    serve_stdio(subagent, subagent_of)
}

/// The stdio ACP server entry. `subagent` mode filters the `subagents`
/// entry from the MCP registry (depth ≤ 2) and marks the transcript.
fn serve_stdio(subagent: bool, subagent_of: Option<String>) -> Result<()> {
    if subagent {
        let project = std::env::current_dir().ok();
        let mut config = crate::config::Config::load(project.as_deref(), |v| std::env::var(v).ok())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        // The child marks its transcript head with the parent session id
        // (ADR 0011): store the marker where newSession writes the head.
        config.subagent_parent = subagent_of;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        return rt.block_on(crate::acp::serve_with_config(
            crate::config::filter_for_subagent(config),
        ));
    }
    // Parent path: normal serve. newSession exports BURAN_SESSION_ID in
    // THIS process (one process per session), so the subagents spawner —
    // running here too — reads it at spawn time (ADR 0011).
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
    // Newest first: switching is "back to the recent one", and uuidv7
    // order is chronological, so reversing the catalog listing is the
    // whole sort. `(empty)` marks a session with no user message yet —
    // a blank title column would look like a formatting bug.
    for i in infos.iter().rev() {
        // `[subagent]` marks delegation transcripts (ADR 0011): they are
        // catalog-visible for debugging but visually separable.
        let marker = if i.subagent_of.is_some() {
            "[subagent] "
        } else {
            ""
        };
        println!(
            "{}\t{}\t{}{}\t{}\t{}",
            i.id,
            i.meta.started_at,
            marker,
            i.title.as_deref().unwrap_or("(empty)"),
            i.meta.cwd.display(),
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
