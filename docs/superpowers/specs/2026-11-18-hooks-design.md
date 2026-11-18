# Hooks v2.1 — design

**Date:** 2026-11-18
**Status:** approved in chat (owner: "+"), implementation plan follows
**Principle:** buran provides extension POINTS; the logic lives outside,
user-configured. Not "everything is MCP" — hooks are a second, complementary
extension surface (MCP for tools, hooks for user logic around events).

## Goal

User-configurable reaction points on harness events, covering the two
Claude-Code-hook workflows the owner actually uses:
1. **rtk-style result rewriting** (`PostToolUse`): compress/rewrite tool
   output before it enters the model's context (60–90% token savings).
2. **Notification on user input** (`Notification`): bell/notify when the
   agent waits for the user.

## Config

```toml
[hooks]
post_tool_use = "rtk hook post-tool"   # may REWRITE the tool result
on_turn_end = "my-bell-script"          # observe-only
```

Each hook is one shell command string. Absent keys = no hook. No built-in
hook implementations — the logic (rtk, bell, notify-send) is entirely the
user's.

## Contract

- The harness spawns the hook as a child process per event, feeding a JSON
  event document on **stdin**; the hook writes JSON on **stdout**.
- **`post_tool_use`** event: `{"event": "post_tool_use", "server": "...",
  "tool": "...", "arguments": {...}, "result": "..."}`. Response:
  `{"result": "..."}` — the rewritten tool result that enters history (and
  the next LLM request). Any other/absent/invalid response = the original
  result passes unchanged.
- **`on_turn_end`** event: `{"event": "on_turn_end", "session_id": "...",
  "stop_reason": "..."}`. Output ignored (observe-only).
- **Fail-open**: hook fails to spawn, times out, or emits garbage → warning
  logged, original result passes. Hooks must never break a turn.
- **Timeout**: `hooks.timeout_secs`, default 10. A timed-out hook's output
  is discarded (original passes).
- Execution: `sh -c` (user-configured command strings are shell by
  definition); stdin piped, stdout captured, stderr passes to buran's
  stderr (log sink).

## Call sites

- `post_tool_use`: in the turn loop, after each MCP tool result is
  produced, before it is appended to history. Applies per tool call
  (including failed calls — the hook may rewrite error text too).
- `on_turn_end`: where the TurnSummary resolves (before the pump exits).

## Non-goals (v2.1)

- PreToolUse (blocking/denying a call before execution) — needs a deny
  semantics design; deferred until the owner asks.
- Per-tool hook filtering (`hooks.match`) — one hook, all tools, v2.1.
- Hook chaining / multiple hooks per event — one command per event.

## Tests (TDD, RED first each)

1. Config: `[hooks]` parse + defaults (absent = disabled).
2. Runner: spawn, stdin JSON, stdout JSON; fail-open on spawn failure,
   garbage output, and timeout; rewrite applied when valid.
3. Wiring: a turn's tool result passes through the hook (unit-level:
   hook fn called with the right event document; integration-level with a
   real `sh -c` hook rewriting text).
4. `on_turn_end` fires with the stop reason; hook failure does not affect
   the turn result.
