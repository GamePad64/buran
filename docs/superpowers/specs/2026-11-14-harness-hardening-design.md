# Harness Hardening — Design (v1.5.1)

Date: 2026-11-14
Status: approved design, pre-plan
References: AGENTS.md (minimalism, one binary, providers are config, hclient only, no stdout under serve), sessions-store v1.5 design, hclient 0.1.0-alpha.20 retry surface, ACP v1 `session/set_mode` and `SessionModeState`, ADR 0007 (unknown record types are skipped).

## Purpose

A harness that can be left running a session for an hour must survive transient provider hiccups, detect its own runaway loops, record what it spent, honor client mode switches, keep MCP tools available after a server crash, and ship reproducible binaries.

The lgtm'd scope (prioritization 1→2→4→6→7 plus release CI):

1. LLM retry/backoff on transient errors (429/5xx, unsent, Retry-After).
2. Tool-loop guard: both a blunt round-trip budget (`max_turns`) and a same-tool streak detector.
4. Per-turn token usage recorded into the session transcript (plus a tracing line).
6. `session/set_mode` handler with three policy-derived modes (`allow_all`, `deny_all`, `ask`).
7. MCP server restart-on-next-call (lazy, inside `McpManager`).
+ Release CI that produces binaries for the specified matrix on tag push.

Success criteria:

- A 429 or 503 from the provider is retried by hclient before the loop ever sees an error; after policy exhaustion the original error surfaces cleanly.
- A model that emits the identical tool call (server + name + args) four turns in a row is cut by the streak guard; history ends with an in-band note; the ACP stop reason is `EndTurn`.
- After a normal turn that receives usage in the final chunk, the transcript contains a `TranscriptRecord::Usage` line; a resumed session can be inspected for cumulative cost.
- A client calls `session/set_mode` with one of the three advertised modes; subsequent tool calls observe the new permission policy for that session only.
- An MCP stdio server that dies mid-session is respawned on the next `tools/call` (or `tools/list`); repeated crash-looping is limited and eventually degrades permanently for that process.
- `git tag vX.Y.Z && git push --tags` produces a GitHub Release with tarballs for all listed targets; `cargo build --release` on a fresh machine for those targets is validated by CI.

## Non-goals

- Context compaction writer (ADR 0006 reader exists; writer is out of scope).
- Any TUI changes (mode switcher UI, usage display, etc.). The TUI crate is untouched.
- `session/set_config_option` or model selector (requires `unstable_session_options` feature; deferred).
- Per-provider retry configuration or custom backoff policies in `config.toml`.
- Background health-check loop for MCP servers (restart is purely on-call).
- crates.io publish step (explicitly "next step" per owner).
- New ACP stop-reason variants (reuse `EndTurn` + in-band note for the guard).
- Usage surfaced in `sessions list` output (record only; presentation is later).

## Ownership and layering (AGENTS.md check)

Per AGENTS.md: "If a feature can be expressed as config or an MCP server, it does not go into harness code."

All six items fail that test:

- Retry lives at the hclient POST layer owned by `LlmClient`; an external MCP cannot wrap the provider call.
- Loop guard must observe the exact sequence of rounds and tool calls inside `AgentLoop`; nothing outside the turn can enforce it.
- Usage is observed only in the SSE stream path and must be appended to the transcript the server owns.
- `set_mode` is a stable ACP method the client is allowed to call; answering it is protocol compliance inside the agent.
- MCP restart requires ownership of the child process map (`McpManager`); a wrapper MCP cannot respawn the servers the manager already spawned.
- Release CI is repo plumbing, not runtime code.

TUI impact: zero in v1.5.1. All changes are in `crates/buran` or `.github/workflows`.

## Architecture overview

- `LlmClient` (crates/buran/src/llm/client.rs) is the single place that builds the `hclient::Client` and calls `stream_chat`. Retry policy is attached here.
- `AgentLoop` (crates/buran/src/llm/loop_.rs) owns turn counting and tool history. The streak guard is a small state machine inside `run_turn`.
- `ChatStream` gains a way to surface the last observed `Usage`; the loop and ACP server wire it to a new `TranscriptRecord` variant on turn end.
- `acp/server.rs` registers the `SetSessionModeRequest` handler and advertises `modes` in `NewSessionResponse`. `SessionHandle` gains a per-session `PermissionPolicy` override.
- `McpManager` (crates/buran/src/mcp/manager.rs) keeps the original configs and respawns on call failure.
- No changes to `buran-tui`. The ACP seam is sufficient.

## Section 1: LLM retry/backoff

**Decision:** Retry is owned entirely by hclient. The loop never sees transient failures that the policy can handle.

Implementation:
- In `LlmClient::new`, obtain the client via `hclient::Client::builder().retry(<Tokio timer>, policy).build()` (or equivalent path that ends with a `Client` carrying the policy).
- Policy: `hclient::retry::Standard` with `statuses: RetryStatuses::Transient` (408, 429, 500, 502, 503, 504), `backoff: Backoff { base: Duration::from_millis(500), max: Duration::from_secs(30), max_attempts: Some(4) }`, `max_retry_after: 30s`, `retry_unsent: true`.
- POST bodies are `RequestBody::Full` (via `.json(req)`), so replay is safe.
- Mid-stream SSE death (after the first event) is not retried at this layer; the turn fails. (Accepted risk: re-issuing a half-streamed turn could duplicate tool calls.)

Testing (hclient-mock, no real HTTP):
- Push 429 then success → one retried request, success.
- Push `Retry-After: 2` header → `TestTimer` records a sleep of ~2s.
- Exhaust attempts → the terminal error is returned to the caller.
- Non-retryable status (e.g. 400) → no retry.

## Section 2: Tool-loop guard

**Decisions:** Keep the existing `max_turns` blunt ceiling. Add a same-tool streak guard. Streak threshold defaults to 4 consecutive identical rounds. Cut reason is `FinishReason::LoopGuard`; ACP surface is `StopReason::EndTurn` plus an assistant-visible note appended to history.

A "round" for streak purposes is one LLM response that produced one or more tool calls. Identity is `(qualified_name, canonical_json_args_hash)`. Mixed tool calls in one response count as the set for that round.

Implementation:
- `AgentLoop::with_same_tool_streak_guard(mut self, n: u32)` (default 4 via `new`).
- Inside `run_turn`, maintain `streak_count` and `last_tool_signature: Option<String>`.
- After executing tool calls in a round, compute the signature of the set. If it matches the previous round's, increment; else reset.
- On reaching the threshold, stop, push a system note into history, emit `TurnEvent::TurnDone(LoopGuard)`, and return the summary with the new finish reason.
- Map `LoopGuard` → `StopReason::EndTurn` (no new ACP variant).

The blunt `max_turns` path remains unchanged and fires `MaxTurnRequests`.

Testing:
- Mock LLM emits the exact same `fs__read` call 5 times → cut on the 4th, note present in transcript, stop reason `EndTurn`.
- Same tool name, different args → no streak.
- `max_turns` still works independently.

## Section 3: Usage recording

**Decision:** Append a `TranscriptRecord::Usage` line after each turn that observed usage. Also emit one `tracing::info!` line (model, latency, tokens) to stderr/file.

New record (added to `session.rs` or a sibling types file):

```rust
TranscriptRecord::Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}
```

Because ADR 0007 already specifies that unknown record types are skipped on replay, old buran binaries will ignore the line on resume. No format version bump is required.

Implementation sketch:
- `ChatStream` exposes `fn last_usage(&self) -> Option<Usage>` (populated when a chunk carrying `usage` arrives).
- `stream_round` returns the usage alongside text/tool-calls/finish.
- `run_turn` accumulates (or takes the final) usage for the turn and emits `TurnEvent::Usage(u)`.
- The ACP event pump, on `TurnDone`, appends the record via the session's `store_file` if present (same guard pattern as message appends).
- A tracing line is emitted regardless of persistence (best-effort).

Testing:
- Mock SSE ends with a usage-only chunk → record appears in the file after the turn.
- Stream with no usage field → no record written.
- Replay of a file containing the usage line succeeds; the usage line is ignored by the message-replay path (only `Message` and `CompactionPoint` affect `SessionState`).

## Section 4: `session/set_mode`

**Decision:** Implement the stable `session/set_mode` only. Model selection (if ever) will use the unstable `session/set_config_option` path later.

Modes advertised:
- `allow_all` — permission policy `AllowAll`
- `deny_all` — permission policy `DenyAll`
- `ask` — permission policy `Ask`

`current_mode_id` at session creation reflects the config policy.

Implementation:
- In `build_agent`, the `NewSessionResponse` (and `LoadSessionResponse` future) carries `modes: Some(SessionModeState { current_mode_id, available_modes })`.
- Register `on_receive_request` for `SetSessionModeRequest`.
- Validate `mode_id` against the three; on unknown → `INVALID_PARAMS`.
- Store the chosen policy in `SessionHandle` (add `permission_override: Option<PermissionPolicy>` or just overwrite a per-handle policy field).
- The prompt pump reads the handle's effective policy for that session (instead of or after the global config policy).
- Response is `SetSessionModeResponse::new(current_mode_id)`.
- No `current_mode_update` notifications are sent autonomously (buran never changes mode by itself).

Testing (acp_integration):
- `newSession` response includes the three modes.
- `set_mode("deny_all")` followed by a tool call → tool result is the denial string, no permission round-trip.
- Unknown mode id → error response, session policy unchanged.

## Section 5: MCP restart-on-call

**Decision:** Lazy, on-call only. Restart is attempted once per failing call; a server that repeatedly crashes is eventually marked permanently failed for the lifetime of the process.

Implementation (inside `McpManager`):
- Keep the original `Vec<McpServerConfig>` from `start`.
- Wrap the clients map in a `tokio::sync::Mutex` (or equivalent) so `call` and `catalog` can respawn.
- On `call` or `list_all_tools` failure that indicates a dead transport (rmcp service error on closed channel, or client absent), attempt `start_one` again (same 10s bound).
- If the respawn succeeds, retry the original operation once.
- Maintain a per-server restart counter. After 3 restarts for a given server name in this process, move it to `failures` and stop retrying.
- `shutdown` still closes whatever is currently live.

Testing (existing mock stdio server):
- Server dies after first successful call → next `call` respawns it and the call succeeds.
- Crash-looper (dies on every handshake or every call) → after 3 attempts it is in `failures()` and further calls surface the normal error.
- Healthy server path is unchanged (no extra spawns).

## Section 6: Release CI

**Decision:** Tag-driven binary release. No crates.io publish in this pass.

Workflow: `.github/workflows/release.yml`
- Trigger: `push: tags: ['v*']`
- Matrix (as specified):
  - `windows-{amd64, arm64}`
  - `linux-{amd64, arm64, rva23}-{musl, gnu}`
  - `macos-arm64`
- Build both `buran` and `buran-tui` in release mode for each target.
- Package as `buran-<target>.tar.gz` (or .zip on Windows).
- Attach all artifacts to a GitHub Release using `softprops/action-gh-release`.
- The existing `ci.yml` remains the PR gate (fmt, clippy -D warnings, test, `cargo build --release` on ubuntu).

The string "rva23" is recorded exactly as provided; the actual Rust target triple mapping (e.g. `riscv64gc-unknown-linux-gnu`) will be resolved in the implementation plan / workflow authoring step.

## Testing strategy (overall)

- Unit: config parsing for any new knobs (loop guard threshold); transcript record round-trip.
- LLM loop: `llm_loop_mock.rs` + `hclient-mock` `TestTimer` for retry and usage.
- MCP: `mcp_mock_server.rs` harness for restart.
- ACP integration: `acp_integration.rs` scenarios for `set_mode` and usage append; load/resume after usage record.
- Gate: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, `cargo build --release`.
- Release workflow is exercised on a tag in a fork or by the owner; the PR workflow continues to run the matrix build check on ubuntu.

## Global constraints (verbatim from AGENTS.md)

- Never write to stdout under `serve` (stdout is the ACP JSON-RPC channel).
- Logs go to stderr/file only.
- All HTTP uses `hclient` (no reqwest, no hand-rolled SSE).
- `cargo clippy -- -D warnings` and `cargo fmt --check` must pass.
- MSRV policy: latest stable (no pinned `rust-version` floor beyond what the compiler needs).
- No built-in tool implementations except the fallback bundled fs server (not touched here).
- Providers are config, not code.
- One binary, zero runtime dependencies for the core experience.

## Open questions / deferred (explicitly parked)

- How (if ever) `sessions list` or TUI surfaces the usage records.
- Whether a future `session/set_config_option` (Model category) will be added behind the unstable feature.
- Exact Rust target triple for the "rva23" string in the release matrix.
- Background health monitoring for MCP (vs pure on-call restart).
- Any TUI affordance for mode display or switching (explicit non-goal for v1.5.1).

---

This spec captures the conversational design that received "lgtm". It is now ready for user review of the written document before any implementation plan is written.