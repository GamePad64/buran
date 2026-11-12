# ADR 0006: Compaction — built-in compactor with a `Summarizer` seam; slash commands as the trigger surface

- Status: accepted
- Date: 2026-11-12
- Deciders: buran owner (Alexander), via design discussion

## Context

The spec defers context compaction to v1.5 (open question 1, "manual trigger
is enough for MVP") and defers transcript storage to v1.5 (open question 4).
The transcript *types* already pin the wire shape: `SessionState` carries
`compaction_points: Vec<usize>` (`llm/loop_.rs`), and
`TranscriptRecord::CompactionPoint { at_message, summary }` exists in
`session.rs` — append-only canonical JSONL, history is never deleted.

Design discussion (2026-11-12) settled the shape: one built-in
one-size-fits-all compactor **with explicit component boundaries**, so it can
move to MCP later if a real need appears — without refactoring the loop, the
JSONL canon, or the projection. A general slash-command mechanism is laid
down at the same time; `/compact` is its first resident.

Surveyed practice (pi, anvil, Aider, Gemini CLI, Claude Code, OpenCode):
every harness converges on the same skeleton — trigger → cut point → LLM
summary → rebuild projection. Variation lives in (a) trigger thresholds =
config, (b) the summarization prompt = text, (c) oversized-span policy =
algorithm. Aider shows a second-model summarizer is a real use case
(`--summarizer-model`); pi disables thinking and caps output by the
summarizer's own limit.

## Decision

### Component boundary (`crates/buran/src/compact/`)

Two units, one seam:

```rust
/// The ONLY LLM dependency of compaction. Builtin impl calls LlmClient;
/// a future MCP impl would call tools/call. The contract below is the
/// future MCP envelope (`summarize_span`), so the move is a new impl,
/// not a refactor.
pub trait Summarizer: Send + Sync {
    fn summarize<'a>(&'a self, span: &[ChatMessage], instructions: Option<&str>)
        -> Pin<Box<dyn Future<Output = Result<String, CompactError>> + Send + 'a>>;
}

/// Invariant logic. Does not know who summarizes.
pub struct Compactor { /* keep_recent_tokens, output_reserve */ }
```

**Invariant logic lives in `Compactor` and is NOT pluggable** — it is
correctness, not policy:

- Cut point: walk backwards from the tail accumulating the estimated budget
  of `keep_recent_tokens`; cut only at user/assistant message boundaries;
  **never** between `assistant_with_tool_calls` and its tool result.
- Compaction appends `CompactionPoint { at_message, summary }` to
  `SessionState.compaction_points`; `history` stays append-only (the JSONL
  canon is never rewritten).
- File operations (read/modified) are extracted **mechanically** from
  tool-call messages in the span and attached to the entry — never mined
  from summary prose (pi's `details.readFiles`/`modifiedFiles` pattern).

**Projection** is the single integration point: `run_prompt` builds the
request from the summary of the last compaction + messages from its kept
boundary onward, instead of the whole `history`. Repeated compactions chain:
the next span starts at the previous kept boundary (pi's
`firstKeptEntryId`), so content still surviving verbatim is summarized
again too.

### Summarizer seam

- **Builtin `Summarizer`**: calls the session model by default;
  `[compaction] model` names an alternative provider entry (the Aider
  use case). Its output budget is capped by the *summarizer's* limit,
  not the session model's. Thinking is disabled for the summary call.
- **MCP move path** (not built): `impl Summarizer for McpSummarizer` —
  `manager.call(server, "summarize_span", { span, instructions, budget })`.
  One config line (`summarizer = { server = "...", tool = "summarize_span" }`),
  one new file (~60 lines), zero changes to loop/cut-point/JSONL.
  Compaction calls are harness-initiated and deliberately bypass the
  permission gate — a modal prompt would freeze auto-compaction; trust for
  a configured compaction MCP server is config-level, like MCP servers
  themselves.
- **No plugin ABI beyond this trait.** Custom summary formats are served by
  a future `[compaction] prompt_file` override, not code.

### Trigger policy

- **Manual**: `/compact [instructions]` — instructions focus the summary
  (pi's pattern), threaded through `Summarizer::summarize`.
- **Auto** (`[compaction] mode = "auto"`): after a turn completes (tool
  results appended) and before the next prompt, compact if
  `est_tokens(history) > window − reserve_tokens`.
- **Token estimate**: `total_chars / 4` — no tokenizer dependency;
  documented limitation. `window` comes from
  `[provider] model_context_window` (hard default 128k when absent).
- **Overflow recovery**: a provider context-overflow error (or final
  `stopReason: "length"` with no tool calls) selects ONE compact-and-retry
  attempt (pi's recovery pattern). If compaction then fails, the turn
  fails with an honest error — no silent degradation.

### Slash-command mechanism (`crates/buran/src/commands/`) — laid down now

A general registry, not a /compact special case:

- **Registry**: builtin commands (`compact` first) + one command per skill
  (v1.5: skill name → `/name`, body loads via the ADR 0004 `<skill>` tag
  convention — no new loading mechanism).
- **Parsing**: in the ACP server's prompt handler, a prompt whose text is
  `/name args` resolves against the registry; an unknown command is NOT an
  error — the text goes to the LLM as-is (the model may still make sense
  of it).
- **Announcement**: after `newSession` (and whenever the set changes), the
  server pushes `SessionUpdate::AvailableCommandsUpdate` — a stable ACP
  v1 variant; clients (Zed) render the slash menu. Types already ship in
  `agent-client-protocol-schema`.
- **Execution**: `compact` runs before any LLM round; skill commands
  behave exactly like a user prompt (the tag convention does the rest).

### Failure semantics

All-or-nothing: if `Summarizer` fails, no `CompactionPoint` is written,
`history` is untouched, the turn continues on the old context (or fails in
the overflow-recovery path). No partial compactions, ever.

## Consequences

- `CompactionPoint` records start being actually written in v1.5; the JSONL
  type needs no change (this ADR is its consumer contract).
- The compaction window estimate is crude (chars/4); real tokenizers are a
  future config option, not a v1.5 dependency.
- Map-reduce / rolling-update oversized-span strategies are deferred: v1.5
  single-pass falls back to the session model when the span exceeds the
  summarizer's window, and fails honestly if that also overflows.
- The MCP move path stays an ADR-documented contract. It is built when a
  real algorithm or a real implementation appears — not speculatively.
- The command registry gives skills a client-visible surface (ADR 0004's
  deferred "skills as commands" closes here) without inventing protocol.
