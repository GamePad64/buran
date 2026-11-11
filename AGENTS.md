# AGENTS.md — buran

_Lightweight agent harness in Rust, in the style of Pi: an ACP agent with MCP tools. One binary, zero built-in tools._

## What this project is

**buran** (cargo bin: `buran`, short command `bu`) is a headless ACP agent server with a separate TUI binary. One process = one session. Named after the Buran shuttle (15.11.1988 — the first spacecraft to launch, complete its mission, and land fully autonomously).

The project starts from zero — **the repo currently has no code**. Your task: build the MVP to the spec below. The spec is settled through design rounds; do not revisit architectural decisions marked "FIXED" — they are agreed with the owner.

## Philosophy (read before starting)

1. **Minimalism in the style of Pi.** No built-in agent tools. Everything is external, via MCP. If a feature can be expressed as config or an MCP server, it does not go into harness code.
2. **One binary, zero runtime dependencies.** The user installs one file and everything works. No node/python needed for basic operation.
3. **Providers are config, not code.** OpenAI-compatible endpoints only. The difference between providers is `base_url`, `api_key`, and a model name. No per-provider plugins.
4. **WASM plugins — no (v1).** MCP-over-stdio is already a cross-language plugin system. wasmtime would add tens of megabytes and contradict the "one binary" goal.

## Architecture (FIXED — do not revisit)

Three layers, each with its own owner:

| Layer | Protocol | Role |
|---|---|---|
| Tools/data | **MCP** (JSON-RPC, stdio) | agents ↔ tools. All tools are MCP servers, including ACT |
| Client ↔ agent | **ACP** (agent-client-protocol, stdio) | editor/client ↔ agent: sessions, streamed `session/update`, permission requests |
| Agent ↔ agent (v3) | **A2A** | network federation, hides internals |

Key ACP rule: the client may call only `initialize / newSession / loadSession / prompt / stop / setSessionMode / setSessionModel`. The client cannot invoke a tool on the agent — only `prompt` moves it. Filesystem, terminal, permissions live on the **client** side (`fs/read_text_file`, `fs/write_text_file`, `terminal/create`, `session/request_permission`); the LLM loop and tool-call execution live on the **agent** side.

## Required surfaces (FIXED — MVP minimum)

1. **AGENTS.md** — project instructions, read automatically by the harness (the way Claude Code reads CLAUDE.md). Format compatible with the gateway-style AGENTS.md; CLAUDE.md is a symlink.
2. **`.mcp.json`** — project MCP server config (the de-facto standard format).
3. **Skills**: `SKILL.md` with frontmatter (`name`, `description`) — descriptions are injected into the system prompt, the body is loaded on relevance. Format compatible with OpenClaw/Anthropic skill-creator. Native + skills-over-MCP (remote skill sources plug in as MCP).
4. **ACT as the recommended default**: docs and the first-run flow recommend ACT (MCP-over-stdio) as the ready-made tool layer. **Not a single code branch about ACT.**

## HTTP client: hclient (FIXED)

All HTTP in buran goes through **hclient** — the owner's own cross-platform async HTTP client workspace (`https://github.com/GamePad64/hclient`, published pre-release on crates.io). Do **not** pull in `reqwest` or any other HTTP stack.

- Dependency: `hclient` with `default-features = false`, enable exactly what you need:
  - `default-transport` (native: tokio + rustls + system resolver) — this is what buran wants; `Client::new()` requires the feature flag and an ambient tokio runtime.
  - `default-http2` if needed for SSE-heavy endpoints later.
- The LLM provider call is a plain `POST /chat/completions` with `stream: true` returning SSE. Use **`hclient::sse::{SseStream, SseEvent}`** — hclient ships its own SSE decoding with MIME-token-boundary checking; do not hand-roll an SSE parser.
- Tests use **`hclient-mock`** (`MockTransport::new()`, `push_response`, `push_response_frames`, `push_response_frames_then_error`) — this is the workspace's own mock, built for exactly this. For LLM streaming tests, push frame responses through `MockTransport` rather than running a real HTTP server. Do **not** add `wiremock` or spawn test HTTP servers: hclient's architecture makes in-process mock transports the intended seam.
- hclient is sans-io at the core with swappable transport/TLS/resolver/runtime seams. buran is a plain native consumer: `Client::new()` + `default-transport` is the whole story. Do not pick transports manually.

## MVP scope

Implement:

1. **`buran serve`** (bin `buran`) — headless ACP server (stdio): `initialize`, `newSession`, `prompt`, `stop`, streamed `session/update`, permission requests out.
2. **`buran-tui`** (separate bin, e.g. `bu-tui` or feature-gated second binary) — TUI client over ACP: ratatui + crossterm. Minimum: session, text stream, approve/deny permission. The TUI is a separate showcase binary, not part of `serve`; the ACP stdio contract between the two binaries is the seam.
3. **LLM loop**: OpenAI-compatible endpoint (base_url + api_key + model from config), streaming via hclient SSE, tool-call loop against connected MCP servers.
4. **MCP client**: spawn stdio servers from `.mcp.json`, `tools/list` → `tools/call`. Use `rmcp` (the official Rust MCP SDK).
5. **Config**: providers + MCP servers + skills (global `~/.config/buran/config.toml` + project-level; project MCP servers are read from `.mcp.json`).
6. **Skills loader**: scans local skill directories, parses frontmatter, injects descriptions, serves bodies on demand.
7. **First-run doctor**: on startup, probe configured MCP servers; if none, offer ACT as the default tool set (interactively, no code branch).
8. **ACP graceful degradation**: if the client does not declare fs/terminal capabilities in `initialize`, the agent must fall back to a bundled filesystem MCP server (enabled by one config line) or honestly lose filesystem tools without crashing.

Explicitly **not** in MVP: subagents (v2, below), A2A (v3), WASM plugins (rejected), context compaction (see Open questions), A2A discovery.

## Crates (do not drag in extras)

- `agent-client-protocol` (Zed) — ACP types and JSON-RPC plumbing.
- `rmcp` — MCP SDK.
- **`hclient`** (+ `hclient-mock` as dev-dependency) — all HTTP and SSE. No reqwest, no other HTTP stack, no hand-rolled SSE.
- `tokio` + `serde`/`serde_json` — runtime and configs.
- `ratatui` + `crossterm` — only in the TUI binary (separate bin or feature, so `buran serve` does not carry them).
- `tracing` + `tracing-subscriber` — logs to file/stderr, never into the ACP stdio channel (!).

**Forbidden**: wasmtime/extism in v1; provider-specific SDKs (openai, anthropic crates); reqwest or any HTTP client other than hclient.

## Toolchain / MSRV

- Edition 2024.
- **MSRV policy: "the latest stable release"** — do not pin a version number. Use a `rust-toolchain.toml` with `channel = "stable"` and `components = ["rustfmt", "clippy"]` (the hclient workspace does exactly this; copy the pattern). No `rust-version = "x.y"` floor in Cargo.toml beyond what the compiler needs, and no MSRV CI job — the policy itself is the statement.
- `cargo clippy -- -D warnings` and `cargo fmt --check` must pass.

## Testing

- Unit tests: parsing config/AGENTS.md/.mcp.json/SKILL.md frontmatter; LLM loop against **`hclient-mock`** `MockTransport` (queued SSE frame responses via `push_response_frames`); MCP handshake against a mock stdio process.
- ACP integration test: spawn `buran serve` as a child process, drive initialize → newSession → prompt with `MockTransport`-backed LLM responses, assert the `session/update` stream.
- Do not spawn real HTTP servers in tests; in-process mock transports are the seam hclient exposes for this.
- CI: GitHub Actions, rust stable, cargo cache.

## Code style

- Rust 2024 edition, American English everywhere (code, comments, doc-comments, commit messages, README, docs).
- Errors: `thiserror` for library types, `anyhow` at the main/CLI boundary.
- CLI: `clap` with derive. Subcommands: `serve` (default), `tui`, `doctor`, `mcp list`.
- Comments and doc-comments in American English; commits follow conventional commits (`feat:`, `fix:`, `docs:`, `chore:`).
- Logs: stderr/file only. **Never write to stdout under `serve`** — stdout is the ACP JSON-RPC channel.
- Match the hclient workspace's bar for comments: explain *why*, with the measurement or commit that backs it, rather than restating *what*.

## Open questions (decide yourself, record as ADRs in `docs/adr/`)

1. **Context compaction**: strategy for trimming long sessions (token threshold? manual `/compact`? summary LLM call?). For MVP a manual trigger is enough.
2. **Permission mapping**: how ACP `session/request_permission` maps to confirm/deny for MCP tool-calls (per-server? per-tool? per-session flag?).
3. **Skills locations**: global directory + project directory? Merge strategy?
4. **Session transcript format** (preparation for v1.5, not an MVP blocker): canonical JSONL (messages, tool-calls + results, model metadata, compaction points) — the artifact for future resume/fork. Types can be laid down now; storage can wait.

## Roadmap (for orientation; MVP = v1)

- **v1**: `serve` + TUI + MCP + AGENTS.md + skills + doctor (this scope).
- **v1.5**: sessions store (canonical JSONL) + `--resume` / `--fork` + ACP `loadSession`. Precedent: OpenClaw continues Claude Code sessions via the session catalog + fork-session.
- **v2**: subagents as a bundled MCP server `buran-subagents` (spawn/send/wait; the parent acts as an ACP client to fresh child processes; depth ≤ 2; the result is the final text in a tool-result).
- **v3**: CTO orchestration over workers: `ssh host -- buran serve` (the stdio tunnel preserves ACP's stdio assumption), the worker lives inside its machine's perimeter (its own fs/its own MCP), the coordinator receives reports, not credentials. A2A (AgentCard, opaque tasks) only when workers become numerous/heterogeneous.

## MVP Definition of Done

- [ ] `cargo build --release` yields the binaries; they work on a fresh machine without runtime dependencies.
- [ ] `buran serve` completes the handshake with an `agent-client-protocol` test client (or Zed, if available).
- [ ] The TUI binary drives a session against `buran serve` end-to-end: streamed text, one tool-call through MCP, one approve/deny.
- [ ] `.mcp.json` with one server (e.g. `npx @modelcontextprotocol/server-filesystem`) is picked up and the tool is available to the model.
- [ ] The project's AGENTS.md is injected into the system prompt.
- [ ] A SKILL.md skill from a skills directory is visible by description; the body loads on demand.
- [ ] Doctor with no configured MCP servers offers ACT (a config line, not code).
- [ ] A client without fs capability does not crash the server (bundled fs-server fallback per config).
- [ ] LLM streaming goes through hclient's `SseStream` (no hand-rolled SSE), tests use `hclient-mock`.
- [ ] CI green: fmt, clippy, tests.

## Do not

- Do not add built-in tool implementations to the binary (except the fallback bundled fs server from "ACP graceful degradation" — and that one is also an MCP server, just bundled).
- Do not introduce per-provider logic in code (config only).
- Do not drag in wasmtime/extism/heavy SDKs.
- Do not use any HTTP client other than hclient; do not hand-roll SSE parsing.
- Do not write to stdout from `serve`.
- Do not block stdio flows with sync I/O inside the tokio context (use `tokio::io` or `spawn_blocking` for child-process stdin/stdout).
- Do not pin a toolchain version (MSRV policy: latest stable).

## Repository

- GitHub: `GamePad64/buran` (main, empty at start).
- License: MIT (the owner confirmed this earlier for the harness; add the LICENSE file).
- Owner: @GamePad64 (Alexander). Architectural questions are fixed above; for doubts, draft an ADR in `docs/adr/NNNN-title.md` and proceed with the most reasonable option, marking the open question at the end of the file.
