# ADR 0011: Subagents — bundled `buran subagents` MCP server

- Status: accepted
- Date: 2026-11-18
- Deciders: buran owner (Alexander), design chat + spec review

## Context

The roadmap's v2 item: delegation — the model hands a subtask to a parallel
agent and collects the final text, without leaving the MCP tool surface.
The design chat (2026-11-18) settled the shape; see
`docs/superpowers/specs/2026-11-18-subagents-design.md` for the full spec.

## Decision

- **One bundled MCP server**: the `buran subagents` subcommand speaks
  MCP-over-stdio, registered like any external server. No harness code
  branches on "subagent".
- **Exactly two tools**: `spawn` (id returned immediately) and `wait`
  (final text). No send/kill/list — extensions come when a workflow asks.
- **Children are real processes**: `buran serve --subagent` via the ACP
  SDK's `AcpAgent` (the SDK owns the process; the driver's lifetime IS
  the child's lifetime). Full process isolation: own McpManager, own
  transcript in the shared store.
- **Depth ≤ 2, mechanical**: `--subagent` filters the `subagents` entry
  from the child's MCP registry at config load — one filter, no branch.
- **Config `[subagents]`** (owner chose override-capable, option «б»):
  `enabled` (default false — opt-in), `provider` override (cheaper child
  model), `max_children` (4), `timeout_secs` (600). Everything not
  overridden is inherited from the parent.
- **Transcript marking**: the spawner passes `--subagent-of <parent-id>`;
  the parent's own session id travels via `BURAN_SESSION_ID`, exported at
  newSession (one process = one session makes the process-global env safe).
  `subagent_of: Option<String>` in the transcript head; `sessions list`
  renders `[subagent]`.
- **Children run `allow_all`** for MCP tool calls (v2 simplification,
  owner-approved in chat): relaying permission asks through two ACP
  channels is a subsystem of its own, deferred to v2.1. A subagent is a
  short-lived worker in a trusted cwd.

## Consequences

- Delegation works with zero harness changes: adding the server to
  `.mcp.json` + enabling in config is the whole opt-in.
- The parent's RSS does not grow (the server runs in the parent process);
  each child costs its own ~9 MB harness + inherited MCP children.
- Overflow recovery and auto-compaction work inside children unchanged —
  they are per-session machinery.
- **Known sharp edge**: the parent's session id is exported process-
  globally at newSession. Valid under "one process = one session" (FIXED);
  a future multi-session process model must replace this channel.

## Open questions

- v2.1: permission relay (parent-ask-through-child).
- v2.1: `send`/`interrupt` tools if mid-flight steering proves needed.
- Cross-machine children remain the A2A/v3 story (ADR 0009/0010 remain
  the transport layer under it).
