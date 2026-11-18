# Subagents v2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** delegation via a bundled `buran subagents` MCP server (spawn/wait),
children as real `buran serve --subagent` processes, depth ≤ 2, marked transcripts.

**Spec:** docs/superpowers/specs/2026-11-18-subagents-design.md

## Global Constraints

- No harness branches on "subagent" — the server is one more MCP stdio server; the `--subagent` depth filter is one config-load filter.
- Logs to stderr/file only; never stdout under `serve` or `subagents` (MCP channel).
- `cargo clippy -- -D warnings` + `cargo fmt --check` + full workspace green per task.
- Conventional commits. American English.
- Committing to main is standing-consented; pushing needs the owner's "continue".

## Review Focus

- Child ACP handshake races: spawn returns before initialize resolves — `wait` on a child still handshaking must queue, not error.
- A child that dies mid-turn (crash, OOM): `wait` must return `status: "crashed"`-ish error, not hang until timeout.
- Pop-before-compact ordering mistakes from the recovery task — same class of bug lives in any "take session, mutate, return" code here.
- `subagent_of` must survive `Store::head` and `list` (bounded head scan) — a field read lazily could miss the marker.
- Drop-cleanup race: parent killed while a child is mid-handshake (not yet in the table).

---

### Task 1: `[subagents]` config + `--subagent` flag filters the registry

**Files:**
- Modify: `crates/buran/src/config.rs`
- Modify: `crates/buran/src/main.rs` (Serve gets `--subagent` flag; new `Subagents` subcommand stub)
- Modify: `crates/buran/src/acp/server.rs` (serve path accepts a filtered config)
- Test: `crates/buran/tests/config.rs`, `crates/buran/tests/cli_subagents.rs` (new)

**Interfaces:**
- Produces: `SubagentsConfig { enabled: bool, provider: Option<SummarizerProvider-shaped>, max_children: usize (4), timeout_secs: u64 (600) }` at `Config.subagents`; `buran serve --subagent` drops the `subagents` entry from the MCP list before McpManager::start.

- [ ] **Step 1: RED — config test** ([subagents] parse: enabled/max_children/timeout_secs/provider override; absent section → defaults with enabled=false)
- [ ] **Step 2: RED — filter test**: config with subagents+fs entries, loaded through the `--subagent` path (a helper fn, not the binary), yields only fs.
- [ ] **Step 3: GREEN** — implement fields + `filter_for_subagent(config) -> Config` pure fn; wire `--subagent` through clap → run::serve → the filter.
- [ ] **Step 4: suite green, commit** `feat: [subagents] config + --subagent depth filter`

### Task 2: `subagent_of` transcript marking

**Files:**
- Modify: `crates/buran/src/session.rs` (TranscriptMeta `subagent_of: Option<String>`)
- Modify: `crates/buran/src/store/mod.rs` (SessionInfo carries it; `create` accepts it)
- Modify: `crates/buran/src/run.rs` (sessions_list renders `[subagent]`)
- Test: `crates/buran/tests/store_title.rs` (extend) or new `store_subagent.rs`

**Interfaces:**
- Consumes: Task 1's `--subagent` flag.
- Produces: `TranscriptMeta.subagent_of: Option<String>` (serde default/skip), `SessionInfo.subagent_of`, `sessions list` prints the marker; old transcripts load unchanged.

- [ ] **Step 1: RED** — seeded jsonl with `subagent_of` → list carries it; without → None; old-format file → loads fine.
- [ ] **Step 2: GREEN** — field + plumbing + list marker.
- [ ] **Step 3: suite green, commit** `feat: subagent_of transcript marking + list marker`

### Task 3: `buran subagents` — MCP server skeleton with `spawn`

**Files:**
- Modify: `crates/buran/src/subagents.rs` (new module; rmcp server like the doctor pattern)
- Modify: `crates/buran/src/main.rs`, `crates/buran/src/run.rs`
- Test: `crates/buran/tests/subagents_server.rs` (new)

**Interfaces:**
- Produces: `subagents::serve(config: Config, parent_session_id: Option<String>) -> Result<()>` — a stdio MCP server exposing `spawn` (validates cap, spawns `buran serve --subagent` via the same `AcpClient::spawn`-shaped helper, returns `{ agent_id }`). Child table internal.
- Note: reuse the TUI crate's client patterns conceptually, but the server lives in the buran crate (single binary) — the ACP client code is small (initialize/new/prompt) and mirrored, not imported cross-crate (tui does not depend on buran).

- [ ] **Step 1: RED** — speak MCP stdio to `buran subagents` (a test client via the same bash-script harness as client_vs_fake_agent, roles reversed): tools/list shows spawn+wait; spawn returns an id (child = real `buran serve --subagent` with a config pointing at a mock LLM server — a tiny localhost SSE stub is FORBIDDEN per AGENTS.md; instead: the child's provider points at a port served by a test-local hclient-mock-backed axum? NO — use the http_integration pattern: a real bound TcpListener serving canned SSE is exactly what http_integration does NOT do; resolve by giving the child a provider that returns an error and asserting spawn/wait LIFECYCLE (not text) — text correctness is Task 4's job with an in-process trick). REFINE during planning: lifecycle tests use an unreachable provider; text tests (Task 4) use a loopback SSE stub test-double clearly labeled test-only.
- [ ] **Step 2: GREEN** — module + spawn.
- [ ] **Step 3: commit** `feat: buran subagents — MCP skeleton, spawn`

### Task 4: `wait` — completion, final text, cached results

**Files:**
- Modify: `crates/buran/src/subagents.rs`
- Test: `crates/buran/tests/subagents_server.rs`

**Interfaces:**
- Produces: `wait(agent_id, timeout_secs?) -> { status, final_text }`; unknown id → tool error; finished id → cached; child crash → error with the child's stderr tail.

- [ ] **Step 1: RED** — wait on a finished id returns cached text; wait on unknown id errors; crashed child errors (kill -9 the child process in the test).
- [ ] **Step 2: GREEN** — result map + status handling.
- [ ] **Step 3: commit** `feat: subagents wait — final text, cache, crash reporting`

### Task 5: cap + cleanup + e2e with real text

**Files:**
- Modify: `crates/buran/src/subagents.rs`
- Test: `crates/buran/tests/subagents_server.rs`

**Interfaces:**
- Consumes: Task 3's table, Task 4's wait.

- [ ] **Step 1: RED** — max_children enforced (5th spawn errors); client disconnect kills children within a grace period; full loop: spawn with a WORKING child (loopback SSE stub, test-only double, labeled as such) → wait → final_text matches the stub's answer; child transcript carries `subagent_of`.
- [ ] **Step 2: GREEN.**
- [ ] **Step 3: commit** `feat: subagents cap, cleanup, e2e delegation loop`

### Task 6: docs + budgets

**Files:**
- Modify: `README.md` (Subagents section), `docs/benchmarks.md` (third row: parent + N children), `docs/adr/0011-subagents.md`

- [ ] **Step 1:** ADR 0011 (decisions + the allow_all ruling + depth-filter ruling), README, budgets re-measure.
- [ ] **Step 2:** suite green, commit `docs: subagents ADR 0011, README, budgets`.
