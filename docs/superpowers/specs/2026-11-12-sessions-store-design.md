# Sessions Store — Design (v1.5)

Date: 2026-11-12
Status: approved design, pre-plan
References: ADR 0006 (compaction, CompactionPoint consumer), ADR 0007
(append-only JSONL canon, derived-index policy), spec open question 4
(session transcript format, "types can be laid down now; storage can wait").

## Purpose

buran sessions currently live only in memory: one `serve` process owns one
`SessionState`, and when the process exits the conversation is gone. v1.5
adds persistence: every session is an append-only JSONL transcript on disk
(the ADR 0007 canon), resumable across process restarts through both the
CLI (`--resume`, `--fork`) and ACP (`session/loadSession`).

Success criteria:

- A prompt → stop → new-process → load cycle restores the full history;
  the model (fed by `hclient-mock` in tests) answers questions about the
  pre-restart conversation.
- Fork creates an independent branch file; the original is never modified.
- `buran sessions list` / `rm` manage sessions from the terminal.
- Replay tolerates a torn final line (crash during append) and skips
  unknown record types (ADR 0007 forward compatibility).

## Non-goals

- Compaction (ADR 0006) — but the reader must surface
  `TranscriptRecord::CompactionPoint` from day one so the compactor can
  use the store without a format change.
- Auto-retention/TTL — cleanup is manual (`buran sessions rm`).
- A catalog index (`sessions.toml` was considered and dropped): the
  directory listing IS the catalog. Any future index is a derived,
  rebuildable cache per ADR 0007.
- Cross-machine session transfer, sharing, export formats (ASR remains
  an external converter concern).

## Storage layout

```
$HOME/.local/state/buran/sessions/
  0193e4c7-…-….jsonl      # one file per session, named by its UUIDv7
```

- **Session IDs are UUIDv7** (`uuid` crate, feature `v7`). Lexicographic
  file order equals chronological order, so "latest" resolves by max
  filename without reading any file content. The existing
  `new_session_id()` nanos-based generator in `acp/server.rs` is replaced
  by `Uuid::now_v7().to_string()`.
- The first line of every file is `TranscriptRecord::Metadata`
  (model, provider, started_at, cwd; optional forked_from). It is written
  at creation and never rewritten; `ended_at` stays unwritten (`None`) —
  nothing in v1.5 needs it, and rewriting the head would violate
  append-only.
- Subsequent lines are `TranscriptRecord::Message(ChatMessage)` — one per
  message, in the exact order they enter `SessionState.history`. Replay is
  therefore `filter_map` over records. `CompactionPoint` records (future)
  interleave and are carried through by the reader.

No `sessions.toml`, no SQLite. The file system is the single source of
truth; every future view (list, search, catalog) is derived from the
files.

## Components

### `store` module (`crates/buran/src/store/`)

```
Store { root: PathBuf }                    // root = …/state/buran/sessions
  fn create(meta: TranscriptMeta) -> SessionFile        // new uuid, head line
  fn open(id: &str) -> (SessionFile, Vec<TranscriptRecord>)  // replay + append handle
  fn list() -> Vec<SessionInfo>             // id, model, started_at, cwd?, forked_from?
  fn fork(id: &str, new_meta: TranscriptMeta) -> SessionFile  // copy records, new id
  fn remove(id: &str) -> io::Result<()>
SessionFile { file: std::fs::File }
  fn append(&mut self, record: &TranscriptRecord) -> io::Result<()>
```

- `append` serializes one record + `\n` and `write_all`s immediately — no
  BufWriter. Records are rare (a handful per turn); crash tolerance and
  simplicity beat micro-buffering.
- `SessionInfo` fields come from reading each file's first line (head
  read). At expected session counts (tens–hundreds) this is
  milliseconds; the derived-index door stays open per ADR 0007.
- `fork` copies all records except the head, writes a fresh head (new id,
  `started_at` = now). Lineage: `TranscriptMeta` gains an optional
  `forked_from: Option<String>` (serde `skip_serializing_if = "None"`),
  so the fork's head carries the original id inside the file — no
  parallel channel needed.
- Reader rules: unknown record types are skipped; a JSON parse failure on
  the final line is a warning + stop (torn tail), anywhere else is an
  error.

### Wiring (`acp/server.rs`)

- `SessionHandle` gains `store_file: Option<store::SessionFile>` (None in
  tests that construct sessions directly; Some in every real `serve`).
- Write points mirror history growth, after the turn completes on the
  server side: the user prompt message, assistant messages, tool-call
  messages, tool results — the exact `ChatMessage`s that were pushed into
  `SessionState.history`, appended in the same order. The loop
  (`llm/loop_.rs`) stays I/O-free per ADR 0001; the store is a
  server-layer concern.
- `session/loadSession`: look up the file by `session_id`; require
  `request.cwd == metadata.cwd` — note: `TranscriptMeta` gains `cwd:
  PathBuf` for this check (the load contract needs it and the spec's
  loadSession mandates the match); rebuild `SessionState` (history +
  `compaction_points`, so ADR 0006 compaction rides for free); attach a
  fresh append handle to the same file.
- Metadata head written at `newSession` time: model + provider from the
  resolved config, started_at now, cwd from the request.

### CLI

- `buran serve --resume <id>` / `--resume latest`: `latest` = max UUIDv7
  filename in the store (for `--resume` with no value, latest is the
  default spelling). The process loads the session before announcing
  readiness; ACP clients then `prompt` into a continuing conversation.
- `buran serve --fork <id>`: `store.fork`, then behave like `--resume` on
  the new id.
- `buran-tui --resume <id> [--fork <id>]`: passes the flag through to the
  spawned `buran serve`.
- `buran sessions list`: table — id, started_at, model, forked_from (head
  reads).
- `buran sessions rm <id>`: delete file. Missing id →
  `session not found: <id> (see buran sessions list)`.

## Error handling

- `loadSession`/`--resume` with an unknown id: JSON-RPC error (ACP) /
  CLI error with the `buran sessions list` hint.
- cwd mismatch on load: error naming both paths (ACP contract: the
  request cwd must match the session's cwd).
- Torn final line: warning to stderr, replay stops at the last good line,
  appends continue after it. A parse failure anywhere else is fatal to
  the load — the file is corrupt beyond the crash-tolerance contract.
- Store I/O errors (disk full, permissions): the turn's *response to the
  client* still succeeds (conversation continues in memory), the error is
  logged to stderr, and `serve` keeps running — persistence failure
  degrades to the pre-v1.5 behavior rather than killing a live session.

## Testing

- `store` unit tests on `tempfile`: create/open roundtrip, append-and-
  replay ordering, fork (original untouched, lineage recorded), torn-tail
  tolerance (truncate mid-line → warning + partial replay), unknown-type
  skip, UUIDv7 monotonic ordering of created ids.
- Server integration (spawn `buran serve` as a child, `hclient-mock`
  frames): prompt → graceful stop → second `serve` process →
  `loadSession` → `session/prompt` "what did I say before?" → mock-verified
  correct answer; cwd-mismatch error path; unknown-id error path.
- CLI tests: `sessions list`/`rm` against a tempdir store via the config
  override used elsewhere in tests.

## Open items (for the plan, not the spec)

- Exact `SessionId` wire handling when clients echo our v7 string back in
  `prompt`/`loadSession` (they are opaque strings per ACP — no work
  expected).
- Whether `buran sessions list` gains `--json` output (trivial, decide at
  implementation).
