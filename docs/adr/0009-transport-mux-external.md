# ADR 0009: The ACP transport stays pluggable stdio; muxing stays external

- Status: accepted
- Date: 2026-11-14
- Deciders: buran owner (Alexander), via design discussion ("acp over http")

## Context

The owner asked whether `buran serve` should speak HTTP so several sessions
can run at once, and whether the agentic loop should become a separate
process inside buran (parent owns MCP servers and logging via IPC) — an
in-process orchestrator of many agents. The worry, stated plainly: that is
a control plane, and buran becomes "fat like claude code and opencode, not
like pi".

Facts measured before deciding:

- ACP is transport-agnostic JSON-RPC over newline-delimited stdio. The
  protocol does not know or care what a "socket" is.
- buran's `serve` is already transport-blind in effect: it reads stdin,
  writes stdout, spawns one session per process (2 ms cold start, ADR 0008).
- **Probe, 2026-11-14, zero code changes**: `socat UNIX-LISTEN:… EXEC:buran serve`
  completes the full ACP `initialize` handshake over a Unix socket;
  `socat TCP-LISTEN:127.0.0.1:41234 …` completes it over TCP, and two
  concurrent TCP connections initialize independently (fork per
  connection = two sessions). The HTTP question is answered by socat
  before any code exists.
- The official `agent-client-protocol-http` crate (5.3.0, published
  2026-11-14) exists but its server feature drags axum + tower-http and its
  client drags reqwest — reqwest is forbidden in buran (AGENTS.md: hclient
  only), and an axum/tower stack in the serve binary would strain the
  15 MB binary budget (ADR 0008) for a problem socat already solves.

## Decision

- **`buran serve` keeps speaking only newline-delimited JSON-RPC on
  stdio.** No socket listener, no HTTP server, no session multiplexer in
  the harness. One process = one session stays the contract (AGENTS.md,
  FIXED).
- **Multi-session and non-stdio transports are composition, not code**:
  - Unix socket / TCP: `socat UNIX-LISTEN:… EXEC:buran serve` (verified
    above; a documented recipe belongs in the README).
  - HTTP/SSE or WebSocket front: a tiny external facade (or any WS→stdio
    bridge) remains possible *without touching buran*, because stdio is
    the seam. If demand materializes, the facade is a separate ~200-line
    binary or a third-party one; it is not an axum dependency inside
    buran.
  - SSH federation (v3 direction): `ssh host -- buran serve` works today
    precisely because the transport is stdio — the stdio assumption is
    what makes the tunnel free.
- **No in-process orchestrator.** The "parent owns MCP + logging, children
  run loops" daemon shape was considered and declined for v1: it converts
  buran into a session manager (connection table, health checks, restart
  logic, IPC protocol for MCP dispatch) — a second program hidden inside
  the first. That is the Claude Code `bg-spare` fleet shape measured at
  1.75 GB on this machine. The same isolation is available with plain
  processes and is what the TUI, Zed, and socat already do.
- **hclient rule untouched**: buran's HTTP stack is for LLM calls (client
  side). `agent-client-protocol-http` would add a server stack; not
  adopted. If ACP-over-HTTP ever becomes an interop requirement, revisit
  via a new ADR with measured binary/RSS cost.

## Consequences

- Concurrency = process count. N sessions cost N × 6.8 MB + N × MCP
  servers — linear, visible in `ps`, killable individually, matching the
  Unix-philosophy requirement. A machine running 50 sessions spawns 50
  processes; if that ever hurts, the fix is a facade binary, not a rewrite
  of buran.
- The README gains a "remote/multi-session" recipe section (socat one-
  liners) instead of a `--listen` flag.
- The stdio contract between `buran serve` and `buran-tui` (already the
  v1 seam) is now also the contract for every future transport: anything
  that can speak to a pipe can drive buran.
- Cost if wrong: if buran later needs true in-daemon sessions (e.g.
  subagent orchestration wanting shared MCP connections), that is v2's
  `buran-subagents` MCP server — process-per-child there too — so the
  decision revisits only if the process model itself fails, which no
  measurement so far suggests.
