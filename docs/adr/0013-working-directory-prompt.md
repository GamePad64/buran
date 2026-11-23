# ADR 0013: Working directory in the system prompt
- Status: accepted
- Date: 2026-11-16
## Context
The model must know which directory the session works in: relative paths
in AGENTS.md instructions, tool calls, and user references ("this repo",
"the config file") all resolve against it. Without an explicit anchor the
model guesses from tool output — wrong, expensive, and invisible to the user.
## Decision
`build_system_prompt` prepends two lines:

```
Working directory: <cwd>
Tool calls and relative paths resolve against this directory.
```

The cwd is the **client-declared** session directory
(`NewSessionRequest.cwd` / `LoadSessionRequest.cwd`), not the directory
`buran serve` was launched in — one process serves sessions in different
directories, each announcing its own. The line is FIRST, before AGENTS.md
layers and the skills block, so it can never be pushed out of attention
by long project instructions.

Consistency with the store: `loadSession` already rejects a cwd mismatch
against the transcript head's `TranscriptMeta.cwd` (sessions store spec),
so the injected cwd and the recorded session cwd can never disagree.
## Consequences
- No new config, no new ACP surface: the feature rides `req.cwd`, which
  ACP always carries.
- A client that lies about cwd gets a lying prompt — but tool calls still
  execute in the buran process's cwd for process-relative operations; the
  injected cwd is an announcement, tools with path arguments receive it
  in their arguments. This matches how AGENTS.md-layer resolution already
  uses `req.cwd` (ADR 0003).
## Open question
Tool servers that default to the buran process's cwd (rather than taking
explicit paths) may act in a different directory than announced. The
honest fix is passing cwd at MCP server spawn time; deferred until a real
server needs it.
