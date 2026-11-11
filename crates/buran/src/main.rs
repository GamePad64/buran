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
    Serve,
    /// Probe config and MCP servers, offer ACT on first run
    Doctor,
    /// Introspect configured MCP servers
    Mcp {
        #[command(subcommand)]
        cmd: McpCommand,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// List MCP servers from config and .mcp.json
    List,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => buran::run::serve(),
        Command::Doctor => buran::run::doctor(),
        Command::Mcp { cmd } => match cmd {
            McpCommand::List => buran::run::mcp_list(),
        },
    }
}
