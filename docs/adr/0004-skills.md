# ADR 0004: Skills — frontmatter scan, first-wins merge, `<skill>` tag loading

- Status: accepted
- Date: 2026-11-11
- Deciders: buran owner (Alexander), via plan design rounds

## Context

Skills follow the OpenClaw/Anthropic `SKILL.md` format (frontmatter `name`
+ `description`, body on demand). The open questions: where skills live,
how duplicates merge, and how a model loads a body without a built-in tool.

## Decision

- **Locations**: `Config.skills_dirs` (global and project lists
  concatenate, ADR 0003). Each directory is scanned for
  `<dir>/<skill>/SKILL.md` and one nested group level
  `<dir>/<group>/<skill>/SKILL.md`.
- **Merge**: first directory wins on a duplicate `name` — the scan is
  deterministic (dirs in config order, entries sorted within a dir), so
  conflicts resolve predictably rather than erroring.
- **Injection**: scanned descriptions go into the system prompt; the body
  is NOT preloaded.
- **Loading convention**: when a turn's first assistant message starts with
  `<skill>Name</skill>`, the loop loads that skill's body and injects it as
  a system-role continuation message before the next round. The tag text
  stays in the transcript and the stream. This is a prompt convention, not
  a tool — keeping with the zero-built-in-tools philosophy (a tool would
  also require the model to know it exists; the tag is in the system
  prompt from turn one).
- **Frontmatter parsing**: minimal `key: value` line parsing between `---`
  markers — no YAML dependency. Unsupported keys are ignored.

## Consequences

- Skills-over-MCP (remote sources) can later plug in as an MCP server that
  serves the same two operations (list descriptions, fetch body); no format
  change is needed.
- A model that never emits the tag simply never loads bodies — the failure
  mode is graceful.
