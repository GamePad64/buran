# ADR 0002: Permission mapping — ACP `session/request_permission` ↔ MCP tool calls

- Status: accepted
- Date: 2026-11-11
- Deciders: buran owner (Alexander), via plan design rounds

## Context

MCP tool calls must pass through an ACP permission flow when the configured
policy is `ask`. The open questions: which policies exist, which outcome
options buran offers, what happens on timeout, and whether trust is scoped
per server or per tool.

## Decision

- **Policies** (`Config.permission`): `allow_all`, `deny_all`, `ask` (the
  default). The policy is per-config, not per-server, in v1.
- **Options offered**: all four kinds — `allow_once`, `allow_always`,
  `reject_once`, `reject_always`. The "always" kinds cost nothing (no
  persistence; they do not outlive the call) and clients that prefer them
  get them. The client resolves its choice by option `kind`, never by
  position or name.
- **Timeout**: `Config.permission_timeout` (default 120 s, not TOML-exposed
  in v1). On timeout the loop behaves as if the client answered
  "reject": the tool call is denied and the model sees the denial.
- **Per-server/per-tool trust lists**: deferred. Nothing in v1 persists an
  `allow_always` across sessions, so per-server scoping has nothing to
  scope yet.

## Consequences

- A slow or dead client cannot hang the loop; the timeout bounds the wait.
- Tests use a slow client (sleep then answer) rather than a parked handler:
  handlers run on the client dispatch loop, and a handler that never returns
  blocks all client message processing.
- When v1.5 adds session persistence, `allow_always` gains meaning and
  per-server trust lists become the natural extension point.
