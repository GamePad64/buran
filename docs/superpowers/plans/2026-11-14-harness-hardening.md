# Harness Hardening v1.5.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the seven approved hardening items so a long-running session survives transient LLM errors, detects its own loops, records token usage, honors client mode switches, keeps MCP tools available after server death, persists transcripts behind an in-process writer task, and ships reproducible binaries on tag.

**Architecture:** All changes stay inside `crates/buran` (TUI untouched). LLM retry is attached at `LlmClient` construction using hclient's `ClientBuilder::retry`. The loop guard is a small state machine inside `AgentLoop::run_turn`. Usage is observed on `ChatStream` and appended as a `TranscriptRecord::Usage` after each turn. `session/set_mode` is handled in the ACP server layer; the chosen policy is stored per `SessionHandle` and read by the permission pump. MCP restart is lazy inside `McpManager::call`/`catalog` with a 3-restart cap. The store writer is an in-process tokio task per session fed by `mpsc<StoreCmd>`; `SessionFile` gains `spawn_writer`. Release CI is a new tag-triggered workflow using the exact matrix.

**Tech Stack:** Rust 2024, tokio, hclient 0.1.0-alpha.20 (with `default-transport` + `test-util`), agent-client-protocol 5, rmcp 3.10, ratatui/crossterm (TUI only, untouched), GitHub Actions.

**Spec:** docs/superpowers/specs/2026-11-14-harness-hardening-design.md

## Global Constraints

- Never write to stdout under `serve` (stdout is the ACP JSON-RPC channel).
- Logs go to stderr/file only.
- All HTTP uses `hclient` (no reqwest, no hand-rolled SSE). Retry uses `hclient::retry::Standard` + `TestTimer` in tests.
- `cargo clippy -- -D warnings` and `cargo fmt --check` must pass.
- MSRV policy: latest stable (no pinned `rust-version` floor beyond what the compiler needs).
- No built-in tool implementations except the fallback bundled fs server (not touched here).
- Providers are config, not code.
- One binary, zero runtime dependencies for the core experience.
- The store writer is in-process only (no separate process).
- `setSessionMode` is implemented; `setSessionModel` / `set_config_option` (unstable) are out of scope.
- No new ACP stop-reason variants; `LoopGuard` surfaces as `StopReason::EndTurn` + in-band note.
- No TUI changes.
- No background MCP health loop (restart is purely on-call).
- No crates.io publish step.

## Review Focus

These five input classes/failure modes are implied by the spec but are not exercised by the most obvious per-item tests. Each task that owns the relevant code must contain an explicit test for the item.

1. Mid-stream SSE death after first event (after partial tool-call deltas) must fail the turn without retrying at the hclient layer (risk of duplicate tool calls). → Task 3 (retry tests) + Task 5 (loop).
2. A tool call that is identical by `(qualified_name, canonical_json_args_hash)` but differs in arg value must NOT increment the streak guard. → Task 7 (streak tests).
3. A resumed session containing a `TranscriptRecord::Usage` line must replay successfully; the usage line is ignored by the message-replay path (only `Message` and `CompactionPoint` affect `SessionState`). → Task 11 (usage append) + Task 17 (writer byte-identity).
4. `set_mode("deny_all")` followed by a tool call must produce a tool result containing the denial string and must NOT send a `session/request_permission` to the client. → Task 13 (set_mode integration).
5. An MCP server that dies after a successful `tools/list` but before the next `tools/call` must be respawned on the call (lazy restart), and the call must succeed if the respawn works. → Task 15 (MCP restart tests).

---

### Task 1: Add `TranscriptRecord::Usage` variant

**Files:**
- Modify: `crates/buran/src/session.rs`
- Test: `crates/buran/tests/session_types.rs`

**Interfaces:**
- Produces: `TranscriptRecord::Usage { prompt_tokens: u64, completion_tokens: u64, total_tokens: u64 }` (new variant, tag `usage`)
- Later tasks consume this for append and replay-ignore logic.

- [ ] **Step 1: Add the variant (failing compile first)**

```rust
// In crates/buran/src/session.rs, inside TranscriptRecord enum
/// Token usage observed at the end of one LLM turn.
Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
},
```

Run: `cargo check -p buran 2>&1 | cat`
Expected: FAIL (no Serialize/Deserialize yet; or enum exhaustiveness in replay if any).

- [ ] **Step 2: Implement Serialize/Deserialize and add round-trip test**

Add `#[serde(untagged)]` or keep the existing `#[serde(tag = "type", rename_all = "snake_case")]` pattern; `Usage` will serialize with `"type":"usage"`.

```rust
// crates/buran/tests/session_types.rs
#[test]
fn usage_record_round_trips() {
    let record = TranscriptRecord::Usage {
        prompt_tokens: 12,
        completion_tokens: 7,
        total_tokens: 19,
    };
    let json = serde_json::to_string(&record).unwrap();
    let back: TranscriptRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, record);
}
```

Run: `cargo test -p buran --test session_types usage_record_round_trips -v`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/buran/src/session.rs crates/buran/tests/session_types.rs
git commit -m "feat: add TranscriptRecord::Usage variant"
```

### Task 2: Surface last observed usage from ChatStream

**Files:**
- Modify: `crates/buran/src/llm/client.rs`
- Test: `crates/buran/tests/llm_client_mock.rs`

**Interfaces:**
- Produces: `impl ChatStream { pub fn last_usage(&self) -> Option<Usage> }`
- Consumes: the existing `ChatChunk` which already carries `usage: Option<Usage>`

- [ ] **Step 1: Add field and populate it (write test first)**

First write a failing test that expects usage to be observable.

```rust
// crates/buran/tests/llm_client_mock.rs
#[tokio::test]
async fn last_usage_is_available_after_final_chunk() {
    let m = hclient::mock::MockTransport::new();
    m.push_response_frames(http::Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(vec![
            r#"data: {"choices":[{"delta":{"content":"hi"}}]}\n\n"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3,"total_tokens":8}}\n\n"#,
            "data: [DONE]\n\n",
        ])
        .unwrap());
    let llm = LlmClient::with_transport(provider(), hclient::Client::builder(m).build().unwrap());
    let req = ChatRequest::new("test-model", vec![]);
    let mut s = llm.stream_chat(&req).await.unwrap();
    while let Some(_) = s.next().await.unwrap() {}
    let u = s.last_usage().expect("usage must be present");
    assert_eq!(u.prompt_tokens, 5);
    assert_eq!(u.completion_tokens, 3);
}
```

Run: `cargo test -p buran --test llm_client_mock last_usage_is_available_after_final_chunk -v`
Expected: FAIL (method does not exist)

- [ ] **Step 2: Implement the field and the method**

Add to `ChatStream`:
```rust
last_usage: Option<Usage>,
```
In the chunk handling loop, when `chunk.usage` is `Some(u)`, store it (last wins).

```rust
pub fn last_usage(&self) -> Option<Usage> {
    self.last_usage
}
```

Run the test again.
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/buran/src/llm/client.rs crates/buran/tests/llm_client_mock.rs
git commit -m "feat: ChatStream::last_usage surfaces final usage chunk"
```

### Task 3: Wire usage through the LLM loop and into the transcript (post-turn append)

**Files:**
- Modify: `crates/buran/src/llm/loop_.rs`, `crates/buran/src/llm/types.rs` (if needed for `TurnEvent`), `crates/buran/src/acp/server.rs`
- Test: `crates/buran/tests/llm_loop_mock.rs` (unit for loop), `crates/buran/tests/acp_integration.rs` (integration)

**Interfaces:**
- Produces: `TurnEvent::Usage(Usage)` (or carry inside `TurnDone`); `stream_round` returns usage; `run_turn` surfaces it.
- Consumes: `ChatStream::last_usage`

- [ ] **Step 1: Extend stream_round and TurnSummary (write failing test first)**

In `llm_loop_mock.rs` add a test that a usage-only final chunk produces a usage observation.

Run first → FAIL.

Then modify `stream_round` to return `Option<Usage>` and `run_turn` to track the last usage for the turn.

- [ ] **Step 2: Add a TurnEvent variant or surface via summary**

Keep `TurnDone(FinishReason)` but make `run_turn` also able to report usage. For simplicity per spec, surface via a new event or return it in the summary and have the server read it on `TurnDone`.

Minimal: change `stream_round` return to include `Option<Usage>`, thread it to `run_turn` return, then in server after `run_turn` read `last_usage` from the stream object if we keep it, or pass the usage out.

Decision in this plan: `stream_round` returns the usage observed in that round; `run_turn` accumulates the last non-None usage for the turn and returns it in an extended `TurnSummary { finish, tool_calls_made, usage: Option<Usage> }`.

First write the test that asserts the usage reaches the caller of `run_turn`.

- [ ] **Step 3: In acp/server.rs post-turn guard, append Usage record**

After the turn, if usage is present, append `TranscriptRecord::Usage { ... }` via the handle's store (or the future writer handle).

Degrade on error (log, continue).

Run the integration test that drives a turn with usage in the final chunk and asserts a `TranscriptRecord::Usage` line appears in the transcript file.

- [ ] **Step 4: Commit**

```bash
git add crates/buran/src/llm/loop_.rs crates/buran/src/acp/server.rs crates/buran/tests/llm_loop_mock.rs crates/buran/tests/acp_integration.rs
git commit -m "feat: per-turn usage is recorded to transcript"
```

### Task 4: Implement same-tool streak guard in AgentLoop (max_turns already exists)

**Files:**
- Modify: `crates/buran/src/llm/loop_.rs`
- Test: `crates/buran/tests/llm_loop_mock.rs`

**Interfaces:**
- Produces: `AgentLoop::with_same_tool_streak_guard(self, n: u32) -> Self`
- Inside `run_turn`: `streak_count: u32`, `last_signature: Option<String>`
- Produces on cut: `FinishReason::LoopGuard` (new internal variant), `TurnEvent::TurnDone(LoopGuard)`, and a system note in history.

- [ ] **Step 1: Add FinishReason::LoopGuard (internal)**

In `types.rs`:
```rust
/// Internal: streak guard or other loop policy cut the turn.
LoopGuard,
```
It is never parsed from the wire.

- [ ] **Step 2: Add the builder method and state machine (write test first)**

Write a test in `llm_loop_mock.rs`:

```rust
#[tokio::test]
async fn same_tool_streak_guard_cuts_after_n_identical_rounds() {
    // Script 5 identical fs__read calls.
    // Assert that after 4 the loop stops with LoopGuard,
    // history ends with a system note containing "loop guard" or similar,
    // and only 4 tool calls were executed.
}
```

Run → FAIL.

Implement `with_same_tool_streak_guard`, compute a stable signature `(name, sha256(args_json_canonical))` or simple json string after canonicalization, compare sets of calls per round.

On reaching threshold: push a system message note, send `TurnDone(LoopGuard)`, return.

- [ ] **Step 3: Map LoopGuard to StopReason::EndTurn in server (no new ACP variant)**

In `acp/server.rs`:
```rust
FinishReason::LoopGuard => StopReason::EndTurn,
```
Plus the note is already in history so the model sees it on resume.

- [ ] **Step 4: Commit**

```bash
git add crates/buran/src/llm/loop_.rs crates/buran/src/llm/types.rs crates/buran/src/acp/server.rs crates/buran/tests/llm_loop_mock.rs
git commit -m "feat: same-tool streak guard cuts runaway loops"
```

### Task 5: Retry policy attached at LlmClient construction (hclient Standard + Transient)

**Files:**
- Modify: `crates/buran/src/llm/client.rs`
- Test: `crates/buran/tests/llm_client_mock.rs`

**Interfaces:**
- In `LlmClient::new`: after `Client::new()?`, do not attach retry here (production path).
- Production path: the caller that builds the real client (`agent_builder_with_factory`) will wrap with retry using the real Tokio clock.
- Test path: `with_transport` stays, but we add a constructor or test helper that builds with `retry(timer, policy)`.
- For the real path, change the factory or the place that does `LlmClient::new(provider)` to do the builder + retry.

Per spec: "In `LlmClient::new`, obtain the client via `hclient::Client::builder()...retry(...)`"

Current `LlmClient::new` calls `hclient::Client::new()`. We will change it to build with retry using the ambient default clock.

For tests that want to observe sleeps, they will use `with_transport` + a client that was built with `TestTimer`.

- [ ] **Step 1: Change LlmClient::new to attach Standard + Transient policy (4 attempts)**

```rust
use hclient::retry::{Backoff, RetryStatuses, Standard};
use std::time::Duration;

pub fn new(provider: &ProviderConfig) -> Result<Self, LlmError> {
    let policy = Standard {
        statuses: RetryStatuses::Transient,
        backoff: Backoff {
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
            max_attempts: Some(4),
        },
        retry_unsent: true,
        max_retry_after: Duration::from_secs(30),
        ..Standard::default()
    };
    let client = hclient::Client::builder(hclient::default_transport()?)
        .retry(hclient::DefaultClock, policy)   // or the right clock type
        .build()?;
    Ok(Self { client, provider: provider.clone() })
}
```

First write a compile test or just run `cargo check`.

- [ ] **Step 2: Write retry tests using MockTransport + TestTimer (no real HTTP)**

Test cases (from spec):
- Push 429 then success → one retried request, success.
- Push `Retry-After: 2` header → `TestTimer` records a sleep of ~2s.
- Exhaust attempts → the terminal error is returned.
- Non-retryable (400) → no retry.

Use `hclient::mock::TestTimer`.

Example skeleton:
```rust
let timer = hclient::mock::TestTimer::new();
let client = hclient::Client::builder(m.clone())
    .retry(timer.clone(), hclient::retry::Standard::transient())
    .build()
    .unwrap();
```

Run tests.
Expected: PASS (and sleeps asserted).

- [ ] **Step 3: Commit**

```bash
git add crates/buran/src/llm/client.rs crates/buran/tests/llm_client_mock.rs
git commit -m "feat: LLM client uses hclient Standard retry with Transient policy"
```

### Task 6: Update production client construction site to supply a real clock if needed

**Files:**
- Modify: `crates/buran/src/acp/server.rs` (the factory site)

If `LlmClient::new` now requires a clock or builds one internally, ensure the production path works. If the change in Task 5 already works with `default_transport`, this may be a no-op or just documentation.

- [ ] Verify that `buran serve` still builds and basic prompt works (manual or via an integration that uses the real path if possible without network).

### Task 7: Streak guard threshold is configurable via `with_same_tool_streak_guard` (default 4)

Already partially done in Task 4. Add a unit test that a different threshold (e.g. 2) cuts earlier.

- [ ] Add test exercising `with_same_tool_streak_guard(2)` cuts on the 2nd identical round.
- [ ] Commit if not already included.

### Task 8: Permission policy per session (store override in SessionHandle)

**Files:**
- Modify: `crates/buran/src/acp/server.rs`, `crates/buran/src/config.rs` (reuse existing `PermissionPolicy`)
- Test: `crates/buran/tests/acp_integration.rs`

**Interfaces:**
- `NewSessionResponse` (and `LoadSessionResponse`) carries `modes: Some(SessionModeState { current_mode_id, available_modes })`
- `SessionHandle` gains `permission_override: Option<PermissionPolicy>`
- The pump reads the effective policy for the session.

- [ ] **Step 1: Define the three mode ids and build SessionModeState**

```rust
const MODE_ALLOW_ALL: &str = "allow_all";
const MODE_DENY_ALL: &str = "deny_all";
const MODE_ASK: &str = "ask";
```

In `build_agent`, when responding to `newSession`, construct:
```rust
let current = match config.permission { ... };
let modes = SessionModeState::new(current, vec![
    SessionMode::new(MODE_ALLOW_ALL, "Allow all"),
    SessionMode::new(MODE_DENY_ALL, "Deny all"),
    SessionMode::new(MODE_ASK, "Ask"),
]);
NewSessionResponse::new(id).modes(modes)
```

Do the same for `LoadSessionResponse` (it also has `.modes(...)`).

- [ ] **Step 2: Register SetSessionModeRequest handler**

```rust
.on_receive_request(async move |req: SetSessionModeRequest, responder, _cx| {
    let mode = req.mode_id.0.as_ref();
    let policy = match mode {
        "allow_all" => Some(PermissionPolicy::AllowAll),
        "deny_all" => Some(PermissionPolicy::DenyAll),
        "ask" => Some(PermissionPolicy::Ask),
        _ => None,
    };
    if let Some(p) = policy {
        // store on handle
        if let Some(h) = state.sessions.lock()...get_mut(&req.session_id) {
            h.permission_override = Some(p);
        }
        responder.respond(SetSessionModeResponse::new());
    } else {
        responder.respond_with_error(Error::new(INVALID_PARAMS, "unknown mode"));
    }
}, ...)
```

- [ ] **Step 3: Change the pump to read the per-session override**

Before the `match policy { ... }` in the event pump, read the handle's effective policy.

- [ ] **Step 4: Integration test**

```rust
// newSession response includes the three modes
// set_mode("deny_all") followed by tool call → tool result has denial string, no permission round-trip
// unknown mode id → error, policy unchanged
```

Run the tests.
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/buran/src/acp/server.rs crates/buran/tests/acp_integration.rs
git commit -m "feat: session/set_mode with three policy modes (per-session override)"
```

### Task 9: LoadSessionResponse must also advertise modes

- [ ] In the `loadSession` handler, after rebuilding the handle, respond with `LoadSessionResponse::new().modes(...)` carrying the same three modes (current from config at load time, or from the handle if we persist override — for MVP current from config is acceptable; override is runtime only).

### Task 10: Keep original MCP configs in McpManager for restart

**Files:**
- Modify: `crates/buran/src/mcp/manager.rs`
- Test: `crates/buran/tests/mcp_mock_server.rs`

- [ ] Add field `configs: Vec<McpServerConfig>` stored at `start`.
- [ ] Make `clients` behind a `tokio::sync::Mutex` (or equivalent) so `call` and `catalog` can mutate.
- [ ] Commit the change (even if restart logic is in next task).

### Task 11: Lazy restart-on-call inside McpManager (cap 3 restarts)

**Files:**
- Modify: `crates/buran/src/mcp/manager.rs`
- Test: `crates/buran/tests/mcp_mock_server.rs`

**Interfaces:**
- On `call` or `list_all_tools` failure that looks like dead transport (rmcp service error on closed channel, or client absent), attempt `start_one` again.
- If respawn succeeds, retry the original operation once.
- Per-server restart counter; after 3 moves to `failures` permanently for this process.

- [ ] **Step 1: Write a test that a server dies after first successful call → next call respawns and succeeds**

Use the existing half-dead or a new script that answers one `tools/list`, then closes, then on respawn answers again.

Run → FAIL.

- [ ] **Step 2: Implement the restart logic**

Wrap the lookup + call in a retry-once path when the error indicates a dead client.

Maintain a `HashMap<String, u32>` restart counts (or inside the struct).

After 3, push to `failures` and stop retrying.

- [ ] **Step 3: Crash-looper test**

Server that dies on every handshake or every call → after 3 attempts it is in `failures()` and further calls surface the normal error.

- [ ] **Step 4: Commit**

```bash
git add crates/buran/src/mcp/manager.rs crates/buran/tests/mcp_mock_server.rs
git commit -m "feat: McpManager restarts servers lazily on call with 3-attempt cap"
```

### Task 12: Create the store writer task module

**Files:**
- Create: `crates/buran/src/store/writer.rs`
- Modify: `crates/buran/src/store/mod.rs` (re-export, `SessionFile` gains method)

**Interfaces:**
- `StoreCmd::Append { record: TranscriptRecord, ack: Option<oneshot::Sender<Result<(), StoreError>>> }`
- `pub struct WriterHandle { tx: mpsc::Sender<StoreCmd> }`
- `impl WriterHandle { pub fn append(&self, r: &TranscriptRecord); pub async fn append_acked(&self, r: &TranscriptRecord) -> Result<(), StoreError>; }`
- `SessionFile::spawn_writer(self) -> WriterHandle` (consumes the file into the task)

The task owns the `SessionFile`, does blocking `write_all` inside the dedicated task.

- [ ] **Step 1: Define the types and the task (no tests yet)**

Write the module. The task loop receives on `mpsc`, calls `file.append`, and sends ack if present.

- [ ] **Step 2: Add tests in a new or existing store test file**

Ordering: 10 commands → 10 lines in order.

Ack: success resolves; serialize/write failure returns the error; dropped receiver → `append_acked` errors.

Byte-identity: a transcript produced via the writer equals today's direct-append output (compare two files byte-for-byte after same sequence).

- [ ] **Step 3: Commit**

```bash
git add crates/buran/src/store/writer.rs crates/buran/src/store/mod.rs
git commit -m "feat: in-process store writer task with ack seam"
```

### Task 13: Wire the writer task into the ACP server

**Files:**
- Modify: `crates/buran/src/acp/server.rs`
- Modify: `crates/buran/src/store/mod.rs` (if needed for `SessionFile`)

- [ ] Change `SessionHandle.store_file: Option<WriterHandle>`
- [ ] In `newSession`: after `store.create(...)`, call `.spawn_writer()` to get the handle.
- [ ] In `loadSession`: after `store.open(...)`, spawn writer from the returned `SessionFile`.
- [ ] Replace direct `file.append(...)` calls with `handle.append(...)` (fire-and-forget) or `append_acked` where future durability would wait (for this pass, fire-and-forget is used for post-turn writes).
- [ ] Degradation: if send to writer fails (task dead), log and continue.

- [ ] Run the byte-identity test from Task 12 against a real transcript written by the server path.
- [ ] Commit.

### Task 14: Update resume path to work with writer (no behavior change for replay)

- [ ] Ensure that after load, appends via the writer produce the same JSONL lines as before (byte-identical test already covers this).

### Task 15: Create release workflow

**Files:**
- Create: `.github/workflows/release.yml`

**Matrix (exact as specified):**
- `windows-{amd64, arm64}`
- `linux-{amd64, arm64, rva23}-{musl, gnu}`
- `macos-arm64`

For `rva23` resolve to the correct Rust target triple: `riscv64gc-unknown-linux-gnu` (or musl variant). The plan records the mapping.

Build both `buran` and `buran-tui` in release mode.

Package as `buran-<target>.tar.gz` (zip on Windows).

Use `softprops/action-gh-release` to attach artifacts.

Trigger: `push: tags: ['v*']`

The existing `ci.yml` stays as PR gate.

- [ ] Write the workflow file.
- [ ] Add a small note in the file about the rva23 mapping.
- [ ] Commit (workflow change).

```bash
git add .github/workflows/release.yml
git commit -m "ci: add tag-triggered release workflow with specified matrix"
```

### Task 16: Ensure LoadSessionResponse carries modes (parity with NewSession)

Already noted in Task 9. If not done, implement here and add a test that a loaded session's `LoadSessionResponse` includes the three modes.

### Task 17: Final gate and cross-checks

- [ ] Run `cargo fmt --check`
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] Run `cargo test --workspace`
- [ ] Run `cargo build --release`
- [ ] Verify that no test writes to stdout under serve paths (existing purity test + manual inspection).
- [ ] Verify that the Review Focus items each have a dedicated test that was written and passes.

If any gate fails, fix in a follow-up commit before declaring the plan complete.

- [ ] Commit the final adjustments.

```bash
git commit -m "chore: final fmt/clippy/test gate for hardening plan"
```

## Notes for implementers

- When using `hclient::Client::builder(transport).retry(timer, policy)`, the timer must implement the required bounds (see hclient docs). In tests use `hclient::mock::TestTimer`.
- `Standard::transient()` is a convenience; the plan spells the fields explicitly for the exact values (4 attempts, 500ms base, 30s max, etc.).
- The streak signature must be stable across JSON formatting differences; canonicalize args before hashing or stringifying.
- The writer task must be per-session (one `mpsc` pair per `SessionFile`).
- `append_acked` is the seam for future pre-effect durability; this pass only uses fire-and-forget appends.
- Release matrix "rva23" is mapped in the workflow to `riscv64gc-unknown-linux-gnu` (and the musl counterpart if needed). Confirm the exact triple with `rustc --print target-list | grep riscv` at implementation time.

## Self-review checklist (done before handoff)

- [ ] Every numbered requirement in the spec has at least one task step that implements it.
- [ ] No "TBD", "TODO", or "implement later" left in task bodies.
- [ ] Every `Expected:` line in a step has a concrete command or assertion.
- [ ] Type names and method signatures introduced in early tasks are used exactly in later tasks.
- [ ] The five Review Focus items each have an explicit test step in the owning task.
- [ ] Global constraints (no stdout under serve, hclient only, etc.) are called out in the relevant task or as a standing requirement.

Plan complete. Ready for execution-method choice.