# ADR 0003: Config merge — global → project precedence, BURAN_API_KEY override

- Status: accepted
- Date: 2026-11-11
- Deciders: buran owner (Alexander), via plan design rounds

## Context

buran reads config from `~/.config/buran/config.toml` (global) and
`buran.config.toml` + `.mcp.json` (project). The merge rules must be
predictable and testable without a real HOME.

## Decision

- **Precedence**: global loads first, project layers on top. Lists
  (`mcp_servers`, `skills_dirs`) concatenate (global entries first);
  the `provider` block merges field-wise with project fields winning.
  There is no override mechanism that removes a global entry — delete it
  from the global file instead.
- **AGENTS.md layering**: every ancestor directory from the repo root down
  to the project directory may carry an `AGENTS.md`; all layers concatenate
  root-first (root speaks first, the nearest file last and most specific).
- **API key**: `BURAN_API_KEY` overrides any configured API key. The key
  never prints — the doctor reports only whether one is present.
- **`.mcp.json`**: project MCP servers come exclusively from the de-facto
  standard `.mcp.json` format (not from `buran.config.toml`), matching
  Claude Code's convention.

## Consequences

- `Config::load(project_dir, home_reader)` takes an injected HOME reader,
  so tests build fixture configs without touching the real one.
- Malformed files are named errors (`error must name the file`), and the
  doctor suppresses its ACT offer when a config is unparseable (a broken
  config is not an empty config).
