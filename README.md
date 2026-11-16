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
context_window = 128000                   # optional: tokens; auto-compact threshold

# Optional: permission policy for MCP tool calls (default "ask")
[permissions]
policy = "ask"        # allow_all | deny_all | ask

# Optional: context compaction (ADR 0006). Auto is ON by default: after a
# turn, if the window estimate crosses context_window − reserve, the
# conversation is summarized and the next request carries summary + tail.
[compaction]
auto = true
reserve = 8192       # tokens kept under the window for replies + tools

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

## Sessions

Every conversation persists as an append-only JSONL transcript in `~/.local/state/buran/sessions/`, named by its UUIDv7 id (filename order = chronological order; ADR 0007: the directory listing is the catalog — no index, no SQLite).

```sh
buran sessions list            # newest first: id, started_at, title, cwd, forked_from
buran-tui --resume latest      # most recent session
buran-tui --fork <id>          # branch; the original is never touched
buran sessions rm <id>         # delete
```

Switching sessions live: **Ctrl-R** in buran-tui opens the session picker — sessions created in the current directory, newest first, titled by their first user message. `Enter` attaches via `loadSession`; the conversation replays into the view. Sessions from other directories are counted, not listed (they cannot be loaded here). A session's title is its first user message, whitespace-collapsed, capped at 80 characters.

Resume semantics: `serve` validates the id before the ACP channel opens; a client then attaches to the stored conversation via `session/loadSession` (cwd must match the session's recorded cwd), and the agent replays the stored conversation to the client as `session/update` notifications before the load response. A torn final line (crash mid-append) replays up to the last good line; unknown record types are skipped for forward compatibility.

Compaction: `/compact [instructions]` summarizes the older half of the conversation into a summary; the LLM window becomes summary + kept tail instead of the full history. **Auto-compaction is on by default** (ADR 0006): after a turn, when the window estimate crosses `context_window − reserve` (default window 128k, reserve 8k), the same compaction runs silently and the client sees a `[auto-compacted N messages]` note. Set `[compaction] auto = false` to require the manual command.

## ACP contract

`buran serve` speaks ACP over stdio (`initialize` → `newSession` → `prompt` → streamed `session/update`, with `session/request_permission` for tool calls). Clients declare their capabilities at `initialize`; a client without fs capability simply gets a catalog without fs tools — honest degradation, no crash, no bundled fallback.

The client may call only `initialize / newSession / loadSession / prompt / stop / setSessionMode / setSessionModel`. Filesystem and terminal access live on the client side; the LLM loop and tool execution live on the agent side.

## Transports: stdio (default), HTTP, socket

- **stdio** is the default and the seam `buran-tui`, Zed, and `ssh host -- buran serve` use.
- **HTTP**: `buran serve --listen 127.0.0.1:8080` serves ACP over HTTP/1.1 + WebSocket on `/acp` (plus `GET /health`) via the official [`agent-client-protocol-http`](https://crates.io/crates/agent-client-protocol-http) stack — one connection per ACP session, each with its own MCP children (ADR 0010). Works natively on Windows, macOS, Linux. HTTP/2 is a deliberate later amendment; TLS and auth beyond loopback belong to the reverse proxy in front (caddy/nginx), not to buran.
- **Unix socket / TCP without HTTP**: stdio is transport-agnostic, so `socat UNIX-LISTEN:buran.sock,fork EXEC:buran serve` (or `TCP-LISTEN:...`) fronts it with zero code — the Unix recipe; Windows has `--listen` instead.

## HTTP: hclient

All outbound HTTP goes through [hclient](https://github.com/GamePad64/hclient) — sans-io at the core, native transport (tokio + rustls), its own SSE decoding for streaming responses, and `hclient-mock` for tests. No reqwest, no hand-rolled SSE, no provider SDKs. The `--listen` server side is the official ACP HTTP transport crate, not a buran-built stack.

## License

MIT. See [LICENSE](LICENSE).
