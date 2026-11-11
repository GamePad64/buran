# ADR 0001: Agent loop shape — channel-based `run_turn`

- Status: accepted
- Date: 2026-11-11
- Deciders: buran owner (Alexander), via plan design rounds

## Context

The agent loop is the hot path between the LLM stream, MCP tool calls, the
session transcript, and ACP clients. Its API shape determines how streaming,
permission prompts, and cancellation compose — and how testable the loop is
without a real provider.

## Decision

`AgentLoop::run_turn` is channel-based:

- **Events out**: `mpsc::Sender<TurnEvent>` — every delta, tool-call update,
  permission query, and the final `TurnEvent::Done(TurnSummary)` flow through
  one ordered channel. The pump that forwards events to ACP clients awaits
  the channel until the sender drops; it is never aborted (an aborted pump
  loses buffered events — observed: the final `PromptResponse` arrived with
  zero notifications).
- **Permissions in**: `mpsc::Receiver<PermissionDecision>` paired with the
  query the loop emits; a closed channel or cancellation denies.
- **Cancellation**: `tokio::watch<bool>`; the loop checks it between rounds
  and inside streaming, and maps a cancel to a clean stop summary (partial
  text recorded, no error).

Rejected alternative: a callback trait (`on_event(&self, event: &TurnEvent)`,
`decide(&self, query: &PermissionQuery) -> Decision`). Callbacks fight the
borrow checker when the caller also holds the session state, and cancellation
would need a second mechanism — the channels already carry it.

## Consequences

- Tests drive the loop with `hclient-mock` transports and plain channel ends;
  no trait mocks.
- The ACP server owns the pump; its semantics (await, never abort) are pinned
  by the Task 9 integration tests.
- The TUI mirrors the same shape client-side: one unified event stream plus
  detached prompts (Task 13).
