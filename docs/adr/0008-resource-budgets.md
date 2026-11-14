# ADR 0008: Resource budgets are a release-gating, measured property

- Status: accepted
- Date: 2026-11-14
- Deciders: buran owner (Alexander), via design discussion

## Context

The owner's requirement: buran must feel "shustry" (fast), consume few
resources, and stay a reliable Unix-philosophy tool — explicitly *not* grow
"fat like Claude Code and opencode, but like pi". "Lightweight" as a README
word decays silently: every feature PR adds a little, nothing regresses a
number, and one day the binary is 40 MB. The differentiator needs to be a
measured property with a gate, not an adjective.

Measured baseline at decision time (docs/benchmarks.md, method there):
session RSS 6.8 MB, idle serve RSS 2.7 MB, cold start p50 2 ms, stripped
binary 8.8 MB. Peers on the same gateway: pi (leanest TS peer) ~104 MB
one-shot peak / 148 MB interactive; Claude Code 350 MB install,
~100–400 MB/process. The gap is real; the risk is drifting into it.

## Decision

Three budgets, enforced at release time, with the measurement method fixed
in docs/benchmarks.md:

| Budget | Limit | Current (2026-11-14) |
|---|---|---|
| Session RSS (one prompt turn, peak) | ≤ 10 MB | 6.8 MB |
| Cold start (spawn → first RPC accepted), p50 | ≤ 100 ms | 2 ms |
| Stripped release binary (buran serve bin) | ≤ 15 MB | 8.8 MB |

- **Gate, not aspiration**: every release re-measures the buran column
  (scripts in `docs/benchmarks-scripts/`). A regression past a budget is a
  release blocker — fix or justify an ADR amendment first.
- **Dependency corollary**: any new crate in the `buran` serve binary must
  be weighed against the binary-size budget. A feature that costs >1 MB
  stripped needs an ADR of its own before it lands.
- **The budgets apply to the harness, not its tools**: MCP servers are
  separate processes (often node, 50–100 MB each) and are excluded by
  definition — buran must not grow a bundled runtime to "save" their RAM;
  that is the Claude Code shape we are avoiding.
- Measurement lives in docs/benchmarks.md with reproduction scripts;
  the doc, not this ADR, carries the current numbers.

## Consequences

- CI/release process gains a manual (scriptable) measurement step; fully
  automating RSS polling in CI is possible later but not required for the
  gate to bind — the discipline is the point.
- Future features with real footprint costs (e.g. an HTTP daemon, context
  compaction caches, subagent orchestration) must fit the budgets or amend
  this ADR with new explicit numbers. "It's convenient" is not an
  amendment argument; "measured X, budget amended to Y because the feature
  earns it" is.
- The 100 ms cold-start budget leaves room for transports that spawn
  per-connection processes (fork per session is ~2 ms today), which is the
  load-bearing fact behind ADR 0009's "spawn, don't multiplex" stance.
