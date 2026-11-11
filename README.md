# buran

_Lightweight agent harness in Rust, in the style of Pi: an ACP agent with MCP tools. One binary, zero built-in tools._

**buran** is a headless ACP agent server with a separate TUI client. One process = one session. Named after the Buran shuttle (15.11.1988 — the first spacecraft to launch, complete its mission, and land fully autonomously).

[![CI](https://github.com/GamePad64/buran/actions/workflows/ci.yml/badge.svg)](https://github.com/GamePad64/buran/actions/workflows/ci.yml)

## Philosophy

1. **Minimalism in the style of Pi.** No built-in agent tools. Everything is external, via MCP. If a feature can be expressed as config or an MCP server, it does not go into harness code.
2. **One binary, zero runtime dependencies.** The user installs one file and everything works. No node/python needed for basic operation.
3. **Providers are config, not code.** OpenAI-compatible endpoints only. The difference between providers is `base_url`, `api_key`, and a model name.
4. **MCP is the plugin system.** All tools — including ACT, the recommended default tool layer — are external MCP servers over stdio. No WASM, no per-provider SDKs.

## Quickstart

```bash
# Build (stable Rust, no runtime dependencies)
cargo install --path crates/buran        # the agent server
cargo install --path crates/buran-tui    # the terminal client

# Check your setup: provider, MCP servers, skills; offers ACT on first run
buran doctor

# Serve over ACP stdio (editors/clients speak ACP; buran-tui drives it too)
buran serve

# Or drive a session from the terminal
buran-tui
```

## Configuration

Global config at `~/.config/buran/config.toml`; project-level `buran.config.toml` layers on top (lists concatenate, `provider` merges field-wise). `BURAN_API_KEY` overrides any configured API key.

```toml
# ~/.config/buran/config.toml
[provider]
base_url = "https://api.openai.com/v1"   # any OpenAI-compatible endpoint
api_key = "sk-..."                        # or leave unset and export BURAN_API_KEY
model = "gpt-4o-mini"

# Optional: permission policy for MCP tool calls (default "ask")
[permissions]
policy = "ask"        # allow_all | deny_all | ask

# Optional: extra skill directories
skills_dirs = ["~/.buran/skills"]
```

### MCP servers — `.mcp.json`

Project MCP servers use the de-facto standard `.mcp.json` format:

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/project"]
    }
  }
}
```

Global MCP servers go under `[[mcp_servers]]` in `config.toml` with the same `name`/`command`/`args` fields.

### AGENTS.md

Project instructions, read automatically (the way Claude Code reads CLAUDE.md). Every ancestor directory from the repo root down to the project may carry an `AGENTS.md`; layers concatenate root-first, nearest file last.

### Skills

`SKILL.md` files with frontmatter (`name`, `description`); descriptions are injected into the system prompt, the body loads on demand when the model emits `<skill>Name</skill>`. Directory layout:

```
.buran/skills/
  greet/
    SKILL.md        # ---\nname: greet\ndescription: Say hello\n---\n...
```

### ACT — the recommended tool layer

On first run, `buran doctor` detects an empty MCP configuration and offers **ACT** (MCP-over-stdio) as the ready-made tool set — including its filesystem component, which is the way buran gets file tools (there is no bundled fs server; all tools are external, by design). The offer prints the ready-to-paste config line; the harness itself has no ACT-specific code.

## ACP contract

`buran serve` speaks ACP over stdio (`initialize` → `newSession` → `prompt` → streamed `session/update`, with `session/request_permission` for tool calls). Clients declare their capabilities at `initialize`; a client without fs capability simply gets a catalog without fs tools — honest degradation, no crash, no bundled fallback.

The client may call only `initialize / newSession / loadSession / prompt / stop / setSessionMode / setSessionModel`. Filesystem and terminal access live on the client side; the LLM loop and tool execution live on the agent side.

## HTTP: hclient

All HTTP goes through [hclient](https://github.com/GamePad64/hclient) — sans-io at the core, native transport (tokio + rustls), its own SSE decoding for streaming responses, and `hclient-mock` for tests. No reqwest, no hand-rolled SSE, no provider SDKs.

## License

MIT. See [LICENSE](LICENSE).
