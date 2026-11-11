# ADR 0005: Bundled fs server — superseded, external MCP only

- Status: superseded (never implemented)
- Date: 2026-11-11
- Deciders: buran owner (Alexander)

## Context

The original plan included a bundled filesystem MCP server as the fallback
for ACP clients that declare no fs capability: when the client cannot serve
`fs/read_text_file`, buran would spawn its own fs server so the model keeps
filesystem tools. A config line (`bundled_fs_server = true`, on by default)
would activate it; sandbox rules would restrict it to the project directory.

## Decision

**Dropped by owner decision mid-Task-11 (2026-11-11):** "FS mcp не делаем.
Даже FS делаем внешний. В частности, можно будет взять ACT filesystem
компонент."

No bundled fs MCP server ships at all. Filesystem tools come exclusively
from external MCP servers configured in `.mcp.json` or the global config —
for example the ACT filesystem component. The `Config.bundled_fs_server`
field, its default, and its test assertions were removed so no dead config
remains.

## Graceful degradation (spec bullet satisfied differently)

The spec allows two honest behaviors for a client without fs capability:
a bundled fallback **or** honestly losing filesystem tools. buran takes the
second branch: a client that declares no fs capability and configures no
external fs MCP server simply gives the model no fs tools. The server never
crashes; the tool catalog just lacks fs entries.

## Consequences

- buran carries zero filesystem code — consistent with the
  zero-built-in-tools philosophy; one less bundled server to sandbox and
  maintain.
- Users who want fs tools configure any MCP filesystem server (ACT
  filesystem is the recommended default, per the doctor's ACT offer).
- If a bundled fallback is ever wanted, it should arrive as a separate
  installable MCP server, not as code inside `buran serve`.
