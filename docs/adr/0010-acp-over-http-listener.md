# ADR 0010: `buran serve --listen` — first-party HTTP/1.1 transport; HTTP/2 deferred

- Status: accepted
- Date: 2026-11-14
- Deciders: buran owner (Alexander), via design discussion
- Supersedes: the "no socket listener in the harness" clause of ADR 0009.
  Everything else in 0009 stands unchanged: stdio remains the default and
  the seam, there is no in-process session orchestrator, and non-stdio
  composition (socat, ssh tunnels) remains valid.

## Context

ADR 0009 declined an HTTP transport on two grounds: the official
`agent-client-protocol-http` crate required reqwest/axum (reqwest is
forbidden by AGENTS.md), and a daemon shape risked growing a control
plane. The external-mux answer also had a platform hole the owner named:
**there is no socat on Windows** — the socat recipe is Unix-only, so
"composition, not code" quietly meant "no non-stdio transport on Windows
at all". The owner then asked for HTTP/1 + HTTP/2 explicitly, and later
scoped it back: **HTTP/2 is not wanted yet**.

Measured before deciding (2026-11-14, probe workspace):

- `agent-client-protocol-http` 5.3.0's server feature is axum-based and
  its client feature is reqwest-based — buran uses **only `server`**;
  hclient remains the sole HTTP *client* (LLM calls), so the reqwest
  concern never materializes.
- `AcpHttpServer::new(factory)` takes any factory returning
  `ConnectTo<Client>`; buran's `agent_builder_with_factory()` builder
  already implements that trait (verified:
  `agent-client-protocol` 5.3.0, `jsonrpc.rs:2112`). Integration needs no
  refactor of agent code.
- Dependency cost of `server`: ~110 crates (axum + hyper + tower +
  tungstenite + futures). Linked-size probe (fat LTO + strip, one real
  handler): **+1.6 MB** over an empty binary. Realistic buran total:
  ~11–11.5 MB, inside the 15 MB budget (ADR 0008).
- Default axum features pulled by the crate contain **no h2 crate** —
  the stack is HTTP/1.1 + WebSocket. Enabling HTTP/2 later = one feature
  line + the `h2` crate (probe: +3 deps, negligible binary delta on the
  router itself).

## Decision

- **`buran serve` gains `--listen <addr>`** (e.g.
  `buran serve --listen 127.0.0.1:8080`). No flag = exactly today's stdio
  behavior; the stdio contract stays FIXED and is what buran-tui and Zed
  use.
- **Transport: `agent-client-protocol-http` server feature** serving
  `POST /acp` (client→agent), `GET /acp` SSE (agent→client), and
  `GET /acp` WebSocket upgrade, per that crate's transport document.
  `GET /health` enabled (its default).
- **HTTP/1.1 only.** The axum `http2` feature is NOT enabled. HTTP/2 is a
  deliberate later amendment to this ADR (one feature line + `h2`), not a
  v1 goal; clients needing multiplexing today use multiple connections or
  WebSocket.
- **Per-connection factory, no orchestrator.** Each ACP connection gets a
  fresh builder → its own `McpManager` → its own MCP child processes.
  Shared state is the tokio runtime, nothing else. There is still no
  session table, no health-check loop, no IPC protocol — the crate's
  connection registry is the entire "orchestration" surface.
- **No TLS and no auth in-process.** Default bind is loopback; exposure
  beyond the machine is done by a reverse proxy (caddy/nginx) that owns
  TLS, HTTP/2-to-the-browser, and authentication. buran speaks plain
  HTTP/1.1 to the proxy — the Unix one-task-one-tool split. CORS support
  ships because the crate provides it (`CorsOptions`, default: off); it
  is documented, not turned on.
- **stdout rule unchanged**: under `--listen`, stdout carries nothing;
  logs stay on stderr.

## Consequences

- Windows reaches buran over HTTP natively — no socat needed on any
  platform; the socat recipe stays in the README for stdio purists.
- N HTTP sessions share one process and its runtime: RAM per extra
  session drops by buran's process floor (~2.7 MB idle RSS each), while
  keeping full MCP isolation per connection. This is not the Claude Code
  fleet shape: there is no coordinator logic to grow.
- Binary budget: the 15 MB ceiling of ADR 0008 now has the axum stack
  inside it; benchmarks.md re-measures on every release as before. If a
  future feature push crosses 15 MB, ADR 0008 amendment, not silent
  growth.
- HTTP/2, when wanted: enable axum `http2` (adds `h2`), re-measure, amend
  this ADR. TLS/ALPN-based browser HTTP/2 arrives with the reverse proxy,
  not with buran.
- Cost if wrong: a listener inside buran is a network-facing surface
  (DoS, parsing bugs) that stdio never was. Mitigation for now: loopback
  default, `axum`'s hardened parsing, no auth/TLS claims made. If
  exposure requirements grow beyond a reverse proxy, that is an ADR.
