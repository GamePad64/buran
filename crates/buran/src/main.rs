use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "buran",
    about = "Lightweight ACP agent harness: one binary, zero built-in tools, everything via MCP",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the headless ACP agent server on stdio (default)
    Serve {
        /// Resume a stored session: an id, or "latest". Bare `--resume`
        /// means "latest".
        #[arg(
            long,
            default_missing_value = "latest",
            require_equals = false,
            num_args = 0..=1
        )]
        resume: Option<String>,
        /// Fork a stored session (copy-on-start) and resume the fork.
        /// Conflicts with `--resume` (they are two spellings of "which
        /// conversation to attach to").
        #[arg(long, conflicts_with = "resume")]
        fork: Option<String>,
        /// Serve ACP over HTTP/1.1 + WebSocket on this address instead
        /// of stdio (ADR 0010): `--listen 127.0.0.1:8080`. Bind is the
        /// caller's business: loopback by default, a reverse proxy owns
        /// TLS/auth beyond it. Without the flag, stdio.
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Marks this server as a SUBAGENT (ADR 0011): the `subagents`
        /// MCP entry is filtered from the config (depth ≤ 2 — a child
        /// cannot spawn children), and the transcript is marked
        /// `subagent_of` the parent session. Set by `buran subagents`,
        /// not by humans.
        #[arg(long)]
        subagent: bool,
    },
    /// Probe config and MCP servers, offer ACT on first run
    Doctor,
    /// Introspect configured MCP servers
    Mcp {
        #[command(subcommand)]
        cmd: McpCommand,
    },
    /// Manage stored sessions
    Sessions {
        #[command(subcommand)]
        cmd: SessionsCommand,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// List MCP servers from config and .mcp.json
    List,
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// List stored sessions (oldest first)
    List,
    /// Delete a stored session by id
    Rm { id: String },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve {
        resume: None,
        fork: None,
        listen: None,
        subagent: false,
    }) {
        Command::Serve {
            resume,
            fork,
            listen,
            subagent,
        } => buran::run::serve(resume, fork, listen, subagent),
        Command::Doctor => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(buran::run::doctor())
        }
        Command::Mcp { cmd } => match cmd {
            McpCommand::List => buran::run::mcp_list(),
        },
        Command::Sessions { cmd } => match cmd {
            SessionsCommand::List => buran::run::sessions_list(),
            SessionsCommand::Rm { id } => buran::run::sessions_rm(&id),
        },
    }
}
