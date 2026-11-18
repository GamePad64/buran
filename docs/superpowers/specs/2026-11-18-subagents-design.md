# Subagents v2 — design

**Date:** 2026-11-18
**Status:** approved in chat (owner: "+" on this design), implementation plan follows
**ADRs touched:** new ADR 0011 (subagents); ADR 0008 budgets re-measured after landing

## Goal

Delegation: the model hands a subtask to a parallel agent and collects the
final text — without leaving the MCP tool surface. The roadmap's v2 item,
verbatim: "subagents as a bundled MCP server `buran-subagents` (spawn/send/
wait; the parent acts as an ACP client to fresh child processes; depth ≤ 2;
the result is the final text in a tool-result)".

## Shape

One bundled MCP server **inside the buran binary** — a new subcommand
`buran subagents` speaking MCP-over-stdio. No harness code branches on
"subagent": the server is registered like any external MCP server, and the
harness cannot tell it apart from one.

```json
{ "mcpServers": { "subagents": { "command": "buran", "args": ["subagents"] } } }
```

The MCP server holds a child table and exposes exactly TWO tools:

- **`spawn(prompt, cwd?, timeout_secs?) → { agent_id }`** — spawns
  `buran serve --subagent` as a child process, drives the ACP handshake
  (initialize → newSession → prompt_detached — the TUI client's proven
  pattern), returns immediately.
- **`wait(agent_id, timeout_secs?) → { status: "done"|"timeout", final_text }`**
  — awaits the child's stop reason; final text is the tool result the
  model reads.

No `send`, no `kill`, no `list` in v2: spawn/wait closes the delegation
loop. Extensions come when a real workflow asks.

## Config: `[subagents]` (owner chose override-capable, option «б»)

```toml
[subagents]
enabled = true                     # false / absent → the server errors and exits
provider = { base_url = "...", model = "...", api_key_env = "..." }  # cheaper child model
max_children = 4                   # hard cap of live children (default 4)
timeout_secs = 600                 # default child wall-clock
```

- **Inheritance:** a child receives the parent's provider, MCP servers, and
  skills — except what `[subagents]` overrides (provider only in v2).
  The child runs its own `McpManager` (its own MCP children): process
  isolation holds.
- **Depth ≤ 2, mechanically:** the spawned child runs `buran serve
  --subagent`; that flag filters the `subagents` entry out of the child's
  MCP server list (one filter at config load, not a harness branch). A
  child therefore has no spawn tool — grandchildren are impossible.
- `enabled = false` or no section → `buran subagents` prints an error on
  stderr and exits non-zero: the tool's absence is the user's config
  choice, honored loudly.

## Lifecycle and isolation

- **Child command:** `buran serve --subagent`. Normal config load, minus
  the subagents MCP entry. Transcripts land in the shared sessions store
  (uuidv7 filenames), **explicitly marked**: `--subagent` mints a fresh
  uuid written into a new optional `subagent_of: Option<String>` field in
  the transcript's metadata head (the parent session's id). `SessionInfo`
  carries it through, so `buran sessions list` and the picker can show or
  filter child sessions (`[subagent of <parent8>]` suffix); serde
  `skip_serializing_if`/`default` keeps pre-v2 transcripts wire-compatible
  (ADR 0007 forward-compat: unknown/missing fields are tolerated both
  ways). The field references the PARENT SESSION id, not a second uuid —
  one id per relationship, and the catalog stays self-describing.
- **Permissions — v2 simplification, documented:** children run MCP calls
  with `allow_all`. Parent-ask-through-child (relaying the child's
  `session/request_permission` up two ACP channels) is deferred to v2.1:
  it is a subsystem of its own. Rationale: a subagent is a short-lived
  worker in a trusted cwd; the owner approved this explicitly in chat.
- **Cleanup:** the server keeps a `ChildHandle` per agent (child process +
  ACP connection + a `stop_rx`). Dropping the handle kills the child
  (SIGTERM; same Drop→kill pattern as rmcp transports). Parent process
  exit drops all handles — no orphans.
- **Cap:** `spawn` past `max_children` returns a tool error listing the
  live count; `wait` frees a slot as the child finishes.

## Resources (ADR 0008)

A child ≈ the session RSS row already measured (~9 MB harness + its MCP
children's node). The parent harness does not grow (the subagents server
runs inside the parent process). Third benchmarks row after landing.

## Non-goals (v2)

- Parent-ask-through-child permission relay (v2.1 if asked).
- `send`/`interrupt`/`list` tools (when a workflow asks).
- Child pool sharing / reuse across spawns (children are one-shot).
- Cross-machine children (that is the A2A/v3 story).

## Tests (TDD, each task RED first)

1. Config: `[subagents]` parse, defaults, `enabled = false`, inheritance
   defaults; depth filter (`--subagent` drops the subagents MCP entry).
2. Integration: spawn → wait → `final_text` equals the child's answer,
   against a real `buran serve --subagent` child with a mock provider
   (config-file injection, not a network mock — the child is a process).
3. Cap: `max_children = 4`, fifth spawn errors with the live count.
4. Cleanup: dropping the MCP client (connection close) kills children
   within a grace period.
5. `wait` on an unknown id errors; `wait` after completion returns the
   cached result (id → result map keeps finished children until reaped).
6. Marking: a `--subagent` child's transcript head carries
   `subagent_of: <parent session id>`; `sessions list` renders the
   `[subagent]` marker; a normal session has no field.
