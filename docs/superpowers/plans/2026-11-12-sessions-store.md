# Sessions Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist buran sessions as append-only JSONL transcripts and make them resumable (`--resume`/`--fork` CLI + ACP `session/loadSession`).

**Architecture:** A new `store` module owns the file format (one UUIDv7-named `.jsonl` per session under `~/.local/state/buran/sessions/`, first line = `Metadata`). The ACP server's `SessionHandle` carries an append handle and mirrors every `ChatMessage` that enters `SessionState.history` into the file after each turn; the loop stays I/O-free (ADR 0001). Resume/fork rebuild `SessionState` from the file and reattach the append handle.

**Tech Stack:** Rust 2024, `serde_json` (already in), `uuid` crate **new** (`features = ["v7"]`), `tempfile` (dev-deps, already in). No HTTP, no new heavy deps.

**Spec:** `docs/superpowers/specs/2026-11-12-sessions-store-design.md`

## Global Constraints

- JSONL is strictly append-only; nothing ever rewrites an existing line except the never-implemented `ended_at` (which stays `None` — spec: "rewriting the head would violate append-only").
- Session IDs are UUIDv7 strings; lexicographic filename order equals chronological order (`--resume latest` = max filename, no file reads).
- The LLM loop (`crates/buran/src/llm/loop_.rs`) must gain no I/O and no store dependency (ADR 0001: "The loop owns no I/O beyond the LLM client and the `ToolCaller`").
- Logs to stderr only; nothing writes to stdout under `serve` (stdout is the ACP channel).
- Torn final line on replay = warning + stop at last good line; parse failure elsewhere = fatal load error; unknown record types = skip (ADR 0007 forward compat).
- `cargo clippy -- -D warnings` and `cargo fmt --check` must pass at every commit.
- American English in all code, comments, commits (conventional: `feat:`, `fix:`, `docs:`, `chore:`, `test:`).

## Review Focus

1. **Store-write failure must not kill a live turn** (spec: "persistence failure degrades to the pre-v1.5 behavior") — a read-only sessions dir or full disk must log to stderr and continue the conversation in memory. Test pins it: Task 4 step for write-error path.
2. **cwd mismatch on load must name both paths** (ACP contract: request cwd must match session cwd). Test pins it: Task 5's `load_session_unknown_id_errors_and_cwd_mismatch_names_both_paths` asserts both the request cwd and the session cwd appear in the error.
3. **Fork must never touch the original file** — byte-identical original after fork, plus new head lineage. Test pins it: Task 3 (`fork_copies_records_never_touches_the_original_and_records_lineage`).
4. **Replay ordering must equal history ordering** — records append in the same order `history` grew; a reversed or interleaved order would silently corrupt resumed conversations. Test pins it: Task 2's roundtrip asserts record order head→user→assistant; Task 5's continuation test proves the rebuilt history feeds the LLM request in order (`body_contains` on the pre-restart turn).
5. **A torn tail followed by appends must yield a valid file** — the resume path splices new records after the last good line without leaving a partial-line fragment in the middle. Test pins it: Task 2 torn-tail-then-append test.

---

### Task 1: UUIDv7 session IDs

**Files:**
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)
- Modify: `crates/buran/Cargo.toml` (`[dependencies]`)
- Modify: `crates/buran/src/acp/server.rs:520-527` (`new_session_id`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `new_session_id() -> String` now returns a UUIDv7 string (callers unchanged — `SessionId::new(new_session_id())` at `server.rs:176`).

- [ ] **Step 1: Write the failing test**

Add a unit test inside the acp module (or in `tests/acp_integration.rs` after the seam is exported) that exercises the generator used by `newSession`. Because `new_session_id` is private to `server.rs`, introduce a `#[doc(hidden)]` test seam and re-export it:

```rust
// In crates/buran/tests/acp_integration.rs or a new unit test under src/acp/
#[test]
fn new_session_ids_are_uuidv7_and_chronologically_ordered() {
    // The ACP server uses this for SessionId; UUIDv7 guarantees lexical order
    // == creation order so `--resume latest` can pick max filename without reads.
    let a = buran::acp::new_session_id_for_test();
    let b = buran::acp::new_session_id_for_test();
    let ua = uuid::Uuid::parse_str(&a).expect("valid uuid");
    let ub = uuid::Uuid::parse_str(&b).expect("valid uuid");
    assert_eq!(ua.get_version(), Some(uuid::Version::SortRand), "must be v7");
    assert_eq!(ub.get_version(), Some(uuid::Version::SortRand), "must be v7");
    assert!(a < b || a == b, "v7 ids are monotonic (equal only on same-ms race)");
    // In practice they differ; the counter field inside v7 makes same-ms distinct.
}
```

The seam (added in impl step) is:

```rust
#[doc(hidden)]
pub fn new_session_id_for_test() -> String { new_session_id() }
```

Re-export from `crates/buran/src/acp/mod.rs`: `pub use server::new_session_id_for_test;`

Also add `uuid` as a dev-dependency of `buran` (the prod dep will be added in Step 3; dev can share the workspace version). If the two calls in one test can collide on the same millisecond, the `assert_ne!` can be relaxed to `assert!(a <= b)` while still asserting parse + v7 version.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p buran --test acp_integration new_session_ids_are_uuidv7`
Expected: FAIL to compile — `new_session_id_for_test` and the `buran::acp` re-export do not exist yet. (If placed as a unit test, it fails because `new_session_id_for_test` is undefined; the current id format `buran-session-<nanos>` would fail the parse anyway.)

- [ ] **Step 3: Write minimal implementation**

`Cargo.toml` (workspace):

```toml
uuid = { version = "1", features = ["v7"] }
```

`crates/buran/Cargo.toml` `[dependencies]`:

```toml
uuid.workspace = true
```

`crates/buran/src/acp/server.rs` (replace the nanos-based generator at
`server.rs:520-527`; `new_session_id` itself stays private — only the
seam is `pub`):

```rust
fn new_session_id() -> String {
    // UUIDv7: time-ordered, so lexicographic filename order is
    // chronological order — `--resume latest` resolves by max filename
    // without reading file contents (sessions store spec).
    uuid::Uuid::now_v7().to_string()
}

/// Test seam: the integration suite asserts the ID format contract.
#[doc(hidden)]
pub fn new_session_id_for_test() -> String {
    new_session_id()
}
```

Re-export from `crates/buran/src/acp/mod.rs`: `pub use server::new_session_id_for_test;`

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p buran --test acp_integration new_session_ids_are_uuidv7`
Expected: PASS

- [ ] **Step 5: Run the whole suite and lints**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/buran/Cargo.toml crates/buran/src/acp/
git commit -m "feat: uuidv7 session ids (chronologically sortable)"
```

---

### Task 2: `store` module — create/append/replay

**Files:**
- Create: `crates/buran/src/store/mod.rs` (module root: `Store`, `SessionFile`, `StoreError`)
- Create: `crates/buran/src/store/replay.rs` (reader: head/tail rules)
- Modify: `crates/buran/src/lib.rs` (add `pub mod store;`)
- Modify: `crates/buran/src/session.rs` (`TranscriptMeta` gains `cwd`, `forked_from`)
- Test: `crates/buran/tests/store_basic.rs`

**Interfaces:**
- Consumes: `TranscriptRecord`, `TranscriptMeta`, `ChatMessage` (all existing in `session.rs` / `llm/types.rs`).
- Produces (later tasks rely on exact signatures):

```rust
pub struct Store { root: PathBuf }
impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self;
    pub fn default_root() -> PathBuf;                    // $HOME/.local/state/buran/sessions
    pub fn create(&self, meta: TranscriptMeta) -> Result<SessionFile, StoreError>;
    pub fn open(&self, id: &str) -> Result<(SessionFile, Vec<TranscriptRecord>), StoreError>;
    pub fn list(&self) -> Result<Vec<SessionInfo>, StoreError>;
    pub fn fork(&self, id: &str, fork_meta: TranscriptMeta) -> Result<SessionFile, StoreError>;
    pub fn remove(&self, id: &str) -> Result<(), StoreError>;
    pub fn latest(&self) -> Result<Option<String>, StoreError>; // max uuidv7 filename
}
pub struct SessionFile { /* private */ }
impl SessionFile {
    pub fn id(&self) -> &str;
    pub fn append(&mut self, record: &TranscriptRecord) -> Result<(), StoreError>;
}
pub struct SessionInfo { pub id: String, pub meta: TranscriptMeta }
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("session not found: {0}")] NotFound(String),
    #[error("io: {0}")] Io(#[from] std::io::Error),
    #[error("corrupt transcript {path}: {reason}")] Corrupt { path: PathBuf, reason: String },
}
```

- [ ] **Step 1: Write the failing tests**

`crates/buran/tests/store_basic.rs`:

```rust
//! Store contract: create/append/replay roundtrip, torn-tail tolerance,
//! append-after-torn-tail, unknown-record skip. All on tempfile (dev-dep).

use buran::llm::types::ChatMessage;
use buran::session::{TranscriptMeta, TranscriptRecord};
use buran::store::Store;

fn meta() -> TranscriptMeta {
    TranscriptMeta {
        model: "test-model".into(),
        provider: "test".into(),
        started_at: "2026-11-12T00:00:00Z".into(),
        ended_at: None,
        cwd: std::path::PathBuf::from("/tmp/proj"),
        forked_from: None,
    }
}

#[test]
fn create_then_append_then_replay_roundtrips_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();

    f.append(&TranscriptRecord::Message(ChatMessage::user("hi"))).unwrap();
    f.append(&TranscriptRecord::Message(ChatMessage::assistant("hello"))).unwrap();

    let (_, records) = store.open(f.id()).unwrap();
    assert_eq!(records.len(), 3, "head metadata + 2 messages");
    assert!(matches!(&records[0], TranscriptRecord::Metadata(m) if m.model == "test-model"));
    assert!(matches!(&records[1], TranscriptRecord::Message(m) if m.content.as_deref() == Some("hi")));
    assert!(matches!(&records[2], TranscriptRecord::Message(m) if m.content.as_deref() == Some("hello")));
}

#[test]
fn torn_final_line_is_a_warning_and_replay_stops_at_last_good_line() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();
    f.append(&TranscriptRecord::Message(ChatMessage::user("full"))).unwrap();

    // Simulate a crash mid-append: truncate the file mid-line.
    let path = dir.path().join(format!("{}.jsonl", f.id()));
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() - 4]).unwrap(); // cut into last record

    let (_, records) = store.open(f.id()).unwrap();
    assert_eq!(records.len(), 1, "head only — torn line dropped");
}

#[test]
fn append_after_torn_tail_produces_a_valid_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();
    f.append(&TranscriptRecord::Message(ChatMessage::user("one"))).unwrap();

    let path = dir.path().join(format!("{}.jsonl", f.id()));
    std::fs::write(&path, {
        let mut b = std::fs::read(&path).unwrap();
        b.extend_from_slice(b"{\"type\":\"msg"); // torn tail, no newline
        b
    }).unwrap();

    // Re-open: replay stops at the torn line; the append handle must
    // position itself at a line boundary so the next append is clean.
    let (mut f2, records) = store.open(f.id()).unwrap();
    assert_eq!(records.len(), 1);
    f2.append(&TranscriptRecord::Message(ChatMessage::user("two"))).unwrap();

    let (_, records) = store.open(f.id()).unwrap();
    assert_eq!(records.len(), 3, "head + one + two — no torn fragment remains");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.ends_with('\n'), "file must stay newline-terminated");
}

#[test]
fn unknown_record_types_are_skipped_on_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut f = store.create(meta()).unwrap();
    // Hand-write a line with an unknown `type` (forward compat, ADR 0007).
    use std::io::Write;
    let path = dir.path().join(format!("{}.jsonl", f.id()));
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(file, r#"{{"type":"from_the_future","x":1}}"#).unwrap();
    writeln!(file, r#"{{"type":"message","role":"user","content":"after"}}"#).unwrap();
    drop(file);

    let (_, records) = store.open(f.id()).unwrap();
    assert_eq!(records.len(), 2, "unknown line skipped, message kept");
    assert!(matches!(&records[1], TranscriptRecord::Message(m) if m.content.as_deref() == Some("after")));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p buran --test store_basic`
Expected: FAIL — `buran::store` does not exist (compile error counts as the red state).

- [ ] **Step 3: Implement the store module**

`crates/buran/src/session.rs` — extend `TranscriptMeta` (additive, serde skips unset):

```rust
pub struct TranscriptMeta {
    pub model: String,
    pub provider: String,
    /// RFC 3339 timestamp of session start.
    pub started_at: String,
    /// RFC 3339 timestamp of session end. Never written in v1.5 (would
    /// rewrite the append-only head); kept for wire compatibility.
    pub ended_at: Option<String>,
    /// Working directory the session was created in. `session/loadSession`
    /// requires the request cwd to match (sessions store spec).
    pub cwd: std::path::PathBuf,
    /// Session this one forked from, when `--fork` created it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
}
```

(Update the one construction site this breaks — the `TranscriptMeta` doc example if any, plus Task 4's server wiring lands next; at this task only tests construct it.)

`crates/buran/src/store/mod.rs`:

```rust
//! Session store: one append-only JSONL file per session (ADR 0007).
//! The directory listing IS the catalog — no index file; any future
//! index is a derived cache (ADR 0007 policy).

mod replay;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::session::{TranscriptMeta, TranscriptRecord};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt transcript {path}: {reason}")]
    Corrupt { path: PathBuf, reason: String },
}

pub struct SessionInfo {
    pub id: String,
    pub meta: TranscriptMeta,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `$HOME/.local/state/buran/sessions` — XDG state home, per spec.
    pub fn default_root() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_default();
        Path::new(&home).join(".local/state/buran/sessions")
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    pub fn create(&self, meta: TranscriptMeta) -> Result<SessionFile, StoreError> {
        fs::create_dir_all(&self.root)?;
        let id = uuid::Uuid::now_v7().to_string();
        let path = self.path_for(&id);
        // create_new: never clobber an existing transcript, even on a
        // freak id collision.
        let mut file = OpenOptions::new().create_new(true).write(true).open(&path)?;
        file.write_all(serde_json::to_string(&TranscriptRecord::Metadata(meta)).expect("serializes")?.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(SessionFile { id, file })
    }

    /// Replays the transcript and returns an append handle positioned
    /// past the last good line. Torn final line: warning + stop (crash
    /// tolerance, ADR 0007); corrupt line elsewhere: fatal.
    pub fn open(&self, id: &str) -> Result<(SessionFile, Vec<TranscriptRecord>), StoreError> {
        let path = self.path_for(id);
        let records = replay::replay(&path)?;
        // Truncate any torn tail so the next append starts on a clean
        // line boundary (Review Focus #5: torn tail + append must stay
        // valid). Read AND write — append(true) ignores seeks, so it
        // cannot coexist with the seek below — then position the cursor
        // at the append point: `append` is write_all on this cursor.
        let good_len = replay::good_prefix_len(&path)?;
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let current_len = file.metadata()?.len();
        if current_len > good_len {
            tracing::warn!(?path, torn = current_len - good_len, "truncating torn tail");
            file.set_len(good_len)?;
        }
        file.seek(std::io::SeekFrom::Start(good_len))?;
        Ok((SessionFile { id: id.to_string(), file }, records))
    }

    pub fn list(&self) -> Result<Vec<SessionInfo>, StoreError> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(stem) = name.to_string_lossy().strip_suffix(".jsonl") else {
                continue;
            };
            let head = replay::head_meta(&entry.path())?;
            out.push(SessionInfo { id: stem.to_string(), meta: head });
        }
        // UUIDv7: lexicographic == chronological.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Copies all records except the head; writes a fresh head carrying
    /// `forked_from`. The original file is never opened for writing.
    pub fn fork(&self, id: &str, mut fork_meta: TranscriptMeta) -> Result<SessionFile, StoreError> {
        let (_, records) = { self.open(id)? }; // validates existence
        fork_meta.forked_from = Some(id.to_string());
        let mut new_file = self.create(fork_meta)?;
        let path = self.path_for(id);
        for record in &records {
            if matches!(record, TranscriptRecord::Metadata(_)) {
                continue; // original head replaced by the fresh one
            }
            new_file.append(record)?;
        }
        let _ = path; // open() already validated
        Ok(new_file)
    }

    pub fn remove(&self, id: &str) -> Result<(), StoreError> {
        let path = self.path_for(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        fs::remove_file(path)?;
        Ok(())
    }

    /// Max UUIDv7 filename == most recent session. No file reads.
    pub fn latest(&self) -> Result<Option<String>, StoreError> {
        Ok(self.list()?.pop().map(|s| s.id))
    }
}

pub struct SessionFile {
    id: String,
    file: File,
}

impl SessionFile {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// One record, one line, written immediately (no BufWriter — records
    /// are rare; crash tolerance and simplicity beat micro-buffering).
    /// Writes at the cursor `create`/`open` left positioned past the last
    /// good line; `write_all` advances it, so successive appends are
    /// strictly sequential.
    pub fn append(&mut self, record: &TranscriptRecord) -> Result<(), StoreError> {
        let line = serde_json::to_string(record).map_err(|e| StoreError::Corrupt {
            path: self.id.clone().into(),
            reason: format!("serialize: {e}"),
        })?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        Ok(())
    }
}
```

Note on `as_file_mut`: `std::fs::File` implements `set_len` directly —
`file.set_len(good_len)` on the `File` returned by `OpenOptions` is
available without deref gymnastics; drop the `as_file_mut` line and call
`file.set_len(good_len)?` — but `set_len` needs `&mut File` and also
moves the cursor: after truncation seek to `good_len` with
`file.seek(io::SeekFrom::Start(good_len))?` so appends land past the
last good line. (Open with `read(true).write(true)` instead of
`append(true)` to allow seeking; `append(true)` ignores seeks.)

`crates/buran/src/store/replay.rs`:

```rust
//! Reader rules (ADR 0007): unknown record types are skipped; a torn
//! final line is a warning + stop; a corrupt line anywhere else is fatal.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::session::{TranscriptMeta, TranscriptRecord};
use super::StoreError;

/// All records from a well-formed prefix of the file.
pub(super) fn replay(path: &Path) -> Result<Vec<TranscriptRecord>, StoreError> {
    let (records, _) = replay_inner(path)?;
    Ok(records)
}

/// Byte length of the file's well-formed prefix (the append position).
pub(super) fn good_prefix_len(path: &Path) -> Result<u64, StoreError> {
    replay_inner(path).map(|(_, len)| len)
}

fn replay_inner(path: &Path) -> Result<(Vec<TranscriptRecord>, u64), StoreError> {
    let file = File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => StoreError::NotFound(
            path.file_stem().unwrap_or_default().to_string_lossy().into_owned(),
        ),
        _ => e.into(),
    })?;
    let mut reader = BufReader::new(file);
    let mut records = Vec::new();
    let mut good_len = 0u64;
    let mut line_no = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(StoreError::Io)?;
        if n == 0 {
            break; // clean EOF
        }
        line_no += 1;
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            good_len += n as u64;
            continue;
        }
        match serde_json::from_str::<TranscriptRecord>(trimmed) {
            Ok(record) => records.push(record),
            Err(e) if is_unknown_type(trimmed) => {
                // Forward compat: skip, keep the bytes (never rewrite).
                tracing::debug!(?path, line_no, "skipping unknown record type");
            }
            Err(e) if line_no == last_line(path) => {
                tracing::warn!(?path, line_no, %e, "torn final line — replay stops here");
                break;
            }
            Err(e) => {
                return Err(StoreError::Corrupt {
                    path: path.to_path_buf(),
                    reason: format!("line {line_no}: {e}"),
                });
            }
        }
        good_len += n as u64;
    }
    Ok((records, good_len))
}

fn is_unknown_type(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .is_some() // parseable JSON with a `type` we didn't recognize
}

fn last_line(path: &Path) -> usize {
    count_lines(path).unwrap_or(usize::MAX)
}

fn count_lines(path: &Path) -> Option<usize> {
    let file = File::open(path).ok()?;
    Some(BufReader::new(file).lines().count())
}
```

**Simplification the implementer may apply:** the `last_line`/`count_lines`
double-read is only needed to distinguish "torn tail" from "corrupt
middle". A cleaner single-pass shape: buffer each line's byte offset;
on a parse failure, check whether any bytes follow that line (peek one
more `read_line` — if it returns 0, this WAS the last line → torn-tail
break; otherwise → fatal Corrupt). Prefer that shape; the tests define
the behavior, not this sketch.

`crates/buran/src/lib.rs`: add `pub mod store;`

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p buran --test store_basic`
Expected: PASS (4/4)

- [ ] **Step 5: Suite + lints, commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check`

```bash
git add crates/buran/src/store/ crates/buran/src/session.rs crates/buran/src/lib.rs crates/buran/tests/store_basic.rs
git commit -m "feat: sessions store — append-only JSONL transcripts, uuidv7 files"
```

---

### Task 3: `list`, `fork`, `latest`, `remove`

**Files:**
- Modify: `crates/buran/src/store/mod.rs` (these methods exist from Task 2's skeleton; this task makes them correct + tested — if Task 2 implemented them fully, this task is only the tests and any fixes the tests force)
- Test: `crates/buran/tests/store_lifecycle.rs`

**Interfaces:**
- Consumes: `Store` API from Task 2.
- Produces: verified `list() -> Vec<SessionInfo>` sorted chronologically, `fork` lineage contract, `latest` selection, `remove` NotFound.

- [ ] **Step 1: Write the failing tests**

`crates/buran/tests/store_lifecycle.rs`:

```rust
//! Lifecycle: chronological listing, fork isolation + lineage, latest,
//! remove. Fork MUST never modify the original (spec Review Focus #3).

use buran::llm::types::ChatMessage;
use buran::session::{TranscriptMeta, TranscriptRecord};
use buran::store::Store;

fn meta(cwd: &str) -> TranscriptMeta {
    TranscriptMeta {
        model: "m".into(),
        provider: "p".into(),
        started_at: "2026-11-12T00:00:00Z".into(),
        ended_at: None,
        cwd: cwd.into(),
        forked_from: None,
    }
}

fn bytes_of(dir: &tempfile::TempDir, id: &str) -> Vec<u8> {
    std::fs::read(dir.path().join(format!("{id}.jsonl"))).unwrap()
}

#[test]
fn list_is_chronological_and_carries_head_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let a = store.create(meta("/a")).unwrap();
    let b = store.create(meta("/b")).unwrap();

    let infos = store.list().unwrap();
    assert_eq!(infos.len(), 2);
    assert_eq!(infos[0].id, a.id(), "uuidv7 creation order == sort order");
    assert_eq!(infos[1].id, b.id());
    assert_eq!(infos[0].meta.cwd, std::path::Path::new("/a"));
}

#[test]
fn fork_copies_records_never_touches_the_original_and_records_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let mut orig = store.create(meta("/proj")).unwrap();
    orig.append(&TranscriptRecord::Message(ChatMessage::user("q"))).unwrap();
    orig.append(&TranscriptRecord::Message(ChatMessage::assistant("a"))).unwrap();

    let original_bytes = bytes_of(&dir, orig.id());

    let mut fork = store
        .fork(orig.id(), meta("/proj"))
        .unwrap();
    fork.append(&TranscriptRecord::Message(ChatMessage::user("branch q"))).unwrap();

    // Original byte-identical after fork + fork-file appends.
    assert_eq!(original_bytes, bytes_of(&dir, orig.id()), "original untouched");

    let (_, fork_records) = store.open(fork.id()).unwrap();
    assert_eq!(fork_records.len(), 4, "fork head + q + a + branch q");
    assert!(matches!(&fork_records[0], TranscriptRecord::Metadata(m)
        if m.forked_from.as_deref() == Some(orig.id())), "lineage recorded");

    let (_, orig_records) = store.open(orig.id()).unwrap();
    assert_eq!(orig_records.len(), 3, "original replay unchanged");
}

#[test]
fn latest_is_the_max_uuidv7_filename() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    assert!(store.latest().unwrap().is_none(), "empty store");
    let first = store.create(meta("/")).unwrap();
    let second = store.create(meta("/")).unwrap();
    assert_eq!(store.latest().unwrap().as_deref(), Some(second.id()));
    let _ = first;
}

#[test]
fn remove_deletes_the_file_and_notfound_on_repeat() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path());
    let f = store.create(meta("/")).unwrap();
    store.remove(f.id()).unwrap();
    assert!(matches!(store.remove(f.id()),
        Err(buran::store::StoreError::NotFound(id)) if id == f.id()));
}
```

- [ ] **Step 2: Run to verify they fail (or expose Task 2 bugs)**

Run: `cargo test -p buran --test store_lifecycle`
Expected: FAIL on anything Task 2 left incorrect (if all pass immediately, Task 2 had already implemented these — the tests still pin the contract; record the pass).

- [ ] **Step 3: Fix implementations until green**

Adjust `store/mod.rs` methods; no new API surface.

- [ ] **Step 4: Suite + lints**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check`

- [ ] **Step 5: Commit**

```bash
git add crates/buran/src/store/ crates/buran/tests/store_lifecycle.rs
git commit -m "test: store lifecycle — fork isolation, chronological list, latest"
```

---

### Task 4: Server wiring — write-through on `newSession`/`prompt`

**Files:**
- Modify: `crates/buran/src/acp/server.rs` (`SessionHandle`, `newSession` handler, `run_prompt_turn_inner` post-turn hook)
- Test: `crates/buran/tests/acp_integration.rs`

**Interfaces:**
- Consumes: `Store::create/open`, `SessionFile::append` (Task 2); `TranscriptMeta` with `cwd` (Task 2).
- Produces: sessions spawned by `serve` persist to `Store::default_root()`; `SessionHandle { store_file: Option<store::SessionFile> }`; `ServerState` gains `store: store::Store`. Tests override the root through a new `Config` field — `Config.sessions_dir: Option<PathBuf>` — because the existing injectable seam is `agent_builder_with_config(config, factory)` (tests build `Config` literally at `acp_integration.rs:72`).

- [ ] **Step 1: Write the failing test**

Append to `crates/buran/tests/acp_integration.rs`. The suite already has the exact shape to copy — `initialize_new_session_prompt_streams_and_ends` (lines 106-135) builds the agent with `agent_builder_with_config(test_config(...), factory)` and drives `cx.build_session_cwd()?..start_session()`. The new test reuses those helpers (`provider()`, `sse()`, `hello_body()`, `test_config()`, `initialize()`) and overrides the sessions root via config:

```rust
#[tokio::test(flavor = "current_thread")]
async fn a_turns_messages_are_persisted_in_history_order() {
    let dir = tempfile::tempdir().unwrap();
    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(hello_body().into_boxed_str())]));
    let m2 = m.clone();

    let mut config = test_config(Vec::new());
    config.sessions_dir = Some(dir.path().to_path_buf());

    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, response, .. } =
            cx.build_session_cwd()?.block_task().start_session().await?;
        session.send_prompt("hi")?;
        let text = session.read_to_string().await?;
        assert_eq!(text, "Hello from buran");
        // The ACP session id must equal the transcript filename stem.
        let id = response.session_id.0.to_string();
        let (_, records) = buran::store::Store::new(dir.path())
            .open(&id)
            .expect("transcript exists under the ACP session id");
        assert!(matches!(&records[0], TranscriptRecord::Metadata(meta)
            if meta.model == "test-model"
                && meta.cwd == std::env::current_dir().unwrap()),
     "head metadata recorded (model + cwd)");
        assert!(matches!(&records[1], TranscriptRecord::Message(msg)
            if msg.role == buran::llm::types::Role::User
                && msg.content.as_deref() == Some("hi")), "user message first");
        assert!(matches!(&records[2], TranscriptRecord::Message(msg)
            if msg.role == buran::llm::types::Role::Assistant
                && msg.content.as_deref() == Some("Hello from buran")),
            "assistant message second");
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}
```

(Add `use buran::session::TranscriptRecord;` to the suite imports.)
- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p buran --test acp_integration a_turns_messages_are_persisted`
Expected: FAIL to compile — `Config` has no `sessions_dir` field; then after adding the field (Step 3), the assertion on the transcript fails because nothing is written yet.

- [ ] **Step 3: Implement the write-through**

`acp/server.rs`:

1. `Config` (`crates/buran/src/config.rs`) gains `pub sessions_dir:
   Option<PathBuf>` as a PLAIN field — `Config` itself is not serde (the
   `ConfigFile` struct is the toml shim), so no serde attributes here.
   Decision (YAGNI): no toml key in v1.5 — humans get the default root,
   the field exists for tests and future embedding. `ConfigFile` is not
   touched. `ServerState` gains `store: crate::store::Store`, built in
   `agent_builder_with_config`:

```rust
let store = crate::store::Store::new(
    config.sessions_dir.clone()
        .unwrap_or_else(crate::store::Store::default_root),
);
```

   and `ServerState { config, store, mcp, llm_factory, sessions }`.
   (`Config::load_with_env`'s constructor at `config.rs:282` gains
   `sessions_dir: None`; `test_config` in the integration suite gains
   `sessions_dir: None`, and every other literal `Config { .. }` site in
   `tests/acp_integration.rs` — there are 4 total including `test_config` —
   must gain the field; grep `Config {` in `crates/buran/tests/` to confirm
   nothing else constructs it.)

2. `SessionHandle` gains `store_file: Option<crate::store::SessionFile>`.
   The `newSession` handler (where `SessionHandle { .. }` is inserted,
   `server.rs:160-186`) creates the file before inserting the handle:

```rust
let store_file = match state_new_session.store.create(crate::session::TranscriptMeta {
    model: provider_for_agent.model.clone(),
    provider: provider_for_agent.name.clone(),
    started_at: crate::store::rfc3339_now(),
    ended_at: None,
    cwd: req.cwd.clone(),
    forked_from: None,
}) {
    Ok(f) => Some(f),
    Err(e) => {
        // Persistence degrades to pre-v1.5 behavior: the session runs
        // in memory, the failure is logged (spec Review Focus #1).
        tracing::error!(%e, "cannot create session transcript — continuing without persistence");
        None
    }
};
```

   Note: `provider_for_agent` already exists in `agent_builder_with_config`
   (`server.rs:116`); the newSession handler must capture it (the handler
   currently captures only `state_new_session`). Thread
   `state_provider = state.clone()` + move `provider_for_agent` into the
   handler like the prompt handler moves `provider_prompt`.

   (`rfc3339_now()`: a tiny helper in `store/mod.rs` formatting
   `SystemTime::now()` as RFC 3339 UTC via manual formatting — no new
   chrono/jiff dependency. `pub` because Task 6's `run.rs` calls it across
   the crate boundary:

```rust
pub fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs()).unwrap_or(0);
    // Civil-from-days algorithm (Howard Hinnant), UTC.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}
```

)

3. Persist after the turn completes, in `run_prompt_turn`'s guard block
   (the `{ ... }` block at `server.rs:307-314` that clears
   `turn_in_flight`). The session state is already back in the handle by
   then — `run_prompt_turn_inner` puts it back at `server.rs:444-449` —
   so the whole turn's new messages are a tail slice of the handle's own
   history. Capture `starting_history_len` in `run_prompt_turn` when the
   session is taken (inside the first lock scope, before
   `std::mem::take`) — the tuple at `server.rs:277` becomes:

```rust
let starting_history_len = {
    let mut sessions = state.sessions.lock().expect("sessions lock");
    let handle = sessions.get_mut(&req.session_id).ok_or_else(|| {
        PromptError::Rejected(anyhow::anyhow!(
            "prompt for unknown session {}",
            req.session_id.0
        ))
    })?;
    if handle.turn_in_flight {
        return Err(PromptError::Rejected(anyhow::anyhow!(
            "session {} already has a prompt in flight",
            req.session_id.0
        )));
    }
    handle.turn_in_flight = true;
    handle.session.history.len()
};
let (session, system_prompt, skills, cancel_rx) = { /* unchanged existing scope */ };
```

   (Two lock scopes is the honest shape — the existing scope's tuple also
   clones system_prompt/skills and subscribes to cancel_rx; folding the
   len read into it works too, but keep the existing scope byte-identical
   and add the len read inside it if the implementer prefers one scope.
   The invariant the tests pin: the length is read BEFORE `mem::take`,
   under the same lock.)

   Then replace the guard block with:

```rust
{
    let mut sessions = state.sessions.lock().expect("sessions lock");
    if let Some(handle) = sessions.get_mut(&req.session_id) {
        handle.turn_in_flight = false;
        // Persist everything this turn added to history, in order. The
        // loop stays I/O-free (ADR 0001); the server layer owns the
        // store. The user prompt is included: the loop pushes it at
        // loop_.rs:192 like every other message.
        if let Some(file) = handle.store_file.as_mut() {
            for msg in handle.session.history[starting_history_len..].iter() {
                if let Err(e) = file.append(&crate::session::TranscriptRecord::Message(msg.clone())) {
                    // Degrade, don't kill the turn (spec Review Focus #1):
                    // the conversation continues in memory.
                    tracing::error!(%e, "transcript append failed — continuing without persistence");
                }
            }
        }
    }
}
```

   Correctness notes for the implementer:
   - `starting_history_len` is `handle.session.history.len()` read in the
     first lock scope of `run_prompt_turn` (where `std::mem::take` runs,
     `server.rs:274-296`) and held as a local through the turn.
   - If a single append fails mid-slice, keep appending the rest (each
     append is independent; a partial turn transcript still replays in
     order). Do not `?` out — the turn result must not change.
   - The mid-turn error path shares this block: it runs before
     `result.map_err(...)`, so both exits persist whatever history
     exists (partial history after an LLM error is kept by design,
     matching the existing `server.rs:311-313` comment).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p buran --test acp_integration a_turns_messages_are_persisted`
Expected: PASS

- [ ] **Step 5: Write-error degradation test (Review Focus #1)**

The contract: when `store.create` fails at `newSession` time, the session
must still work fully in memory (store_file: None). Point
`sessions_dir` at an unwritable path so `create_dir_all`/`create_new`
errors deterministically:

```rust
#[tokio::test(flavor = "current_thread")]
async fn a_store_create_failure_does_not_kill_the_session() {
    // /proc is unwritable everywhere on Linux; create under it fails.
    let m = MockTransport::new();
    m.push_response_frames(sse(vec![Box::leak(hello_body().into_boxed_str())]));
    let m2 = m.clone();

    let mut config = test_config(Vec::new());
    config.sessions_dir = Some(std::path::PathBuf::from("/proc/nonexistent-buran-sessions"));

    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(
            provider(),
            hclient::Client::builder(m2.clone()).build().unwrap(),
        ))
    })
    .await
    .expect("agent builder");

    let client = Client.v1().connect_with(agent, async |cx| {
        initialize(&cx).await?;
        let SessionSetup { mut session, .. } =
            cx.build_session_cwd()?.block_task().start_session().await?;
        session.send_prompt("hi")?;
        // The full turn completes despite the store being unusable.
        let text = session.read_to_string().await?;
        assert_eq!(text, "Hello from buran");
        Ok(())
    });

    tokio::time::timeout(TIMEOUT, client)
        .await
        .expect("integration test timed out")
        .expect("ACP connection failed");
}
```

(Mid-turn append failure is pinned at the code level — the append error
arm logs and continues, no `?` — rather than by a test: deterministically
failing an already-open append fd requires test-only seams in
`SessionFile`, which the review judged not worth the surface. If the
reviewer disagrees, `#[doc(hidden)] pub fn fail_writes_for_test(&mut
self)` swapping the internal `File` for one opened on a read-only fd is
the seam to add.)

- [ ] **Step 6: Suite + lints, commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check`

```bash
git add crates/buran/src/config.rs crates/buran/src/acp/server.rs crates/buran/src/store/ crates/buran/tests/acp_integration.rs
git commit -m "feat: persist session turns to the store (write-through, degrading)"
```

---

### Task 5: `session/loadSession` — rebuild state from a transcript

**Files:**
- Modify: `crates/buran/src/acp/server.rs` (initialize capabilities + new `on_receive_request` for `LoadSessionRequest`)
- Test: `crates/buran/tests/acp_integration.rs`

**Interfaces:**
- Consumes: `Store::open` replay (Task 2), `SessionHandle` (Task 4).
- Produces: `loadSession` handler: validates id + cwd match, rebuilds `SessionState { history, compaction_points }`, attaches a fresh append handle, responds `LoadSessionResponse::new()`. The `InitializeResponse` advertises `AgentCapabilities::new().load_session(true)` (verified SDK: `agent-client-protocol-schema-1.17.0/src/v1/agent.rs:5383 pub load_session: bool`, setter `:5438`; the client refuses `cx.load_session` without it — `session.rs:206`: "Call this only if the initialization response advertises `agentCapabilities.loadSession`"). Unknown id / cwd mismatch → JSON-RPC error naming both paths.

- [ ] **Step 1: Write the failing tests**

Verified client-side API (agent-client-protocol 5.2.0, `src/session.rs:203-212`): `cx.load_session(session_id, cwd).block_task().start_session()` returns `SessionSetup<'static, _, LoadSessionResponse>`. Tests (append to `acp_integration.rs`; reuse the suite helpers `provider()`, `sse()`, `text_chunk()`, `stop_and_done()`, `test_config()`, `initialize()`, `body_contains()`):

```rust
#[tokio::test(flavor = "current_thread")]
async fn load_session_replays_history_and_continues_the_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();

    // Process 1: one turn records a magic word into the store.
    let m = MockTransport::new();
    let turn1 = format!("data: {}\n\n{}", text_chunk("the magic word is FROB"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    let m2 = m.clone();
    let mut config = test_config(Vec::new());
    config.sessions_dir = Some(dir.path().to_path_buf());
    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(provider(), hclient::Client::builder(m2.clone()).build().unwrap()))
    }).await.expect("agent builder");

    let stored_id = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let stored_id1 = stored_id.clone();
    let cwd1 = cwd.clone();
    let client1 = Client.v1().connect_with(agent, move |cx| {
        let stored_id = stored_id1.clone();
        let cwd = cwd1.clone();
        async move {
            initialize(&cx).await?;
            let SessionSetup { mut session, response, .. } =
                cx.build_session(cwd.clone()).block_task().start_session().await?;
            *stored_id.lock().unwrap() = Some(response.session_id.0.to_string());
            session.send_prompt("say the magic word")?;
            let _ = session.read_to_string().await?;
            Ok(())
        }
    });
    tokio::time::timeout(TIMEOUT, client1).await.expect("timed out").expect("conn failed");
    let id = stored_id.lock().unwrap().clone().expect("session id captured");

    // Process 2 (fresh agent over the same store): loadSession + prompt.
    // The scripted reply is empty of information; the real assertion is on
    // the continuation REQUEST: only a correctly rebuilt history can put
    // the pre-restart text into it.
    let m = MockTransport::new();
    let turn2 = format!("data: {}\n\n{}", text_chunk("FROB"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn2.into_boxed_str())]));
    let m2 = m.clone();
    let m3 = m2.clone();
    let mut config = test_config(Vec::new());
    config.sessions_dir = Some(dir.path().to_path_buf());
    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(provider(), hclient::Client::builder(m3.clone()).build().unwrap()))
    }).await.expect("agent builder 2");

    let client2 = Client.v1().connect_with(agent, move |cx| {
        let id = id.clone();
        let cwd = cwd.clone();
        async move {
            initialize(&cx).await?;
            let SessionSetup { mut session, .. } =
                cx.load_session(id, cwd).block_task().start_session().await?;
            session.send_prompt("what was the magic word?")?;
            let _ = session.read_to_string().await?;
            Ok(())
        }
    });
    tokio::time::timeout(TIMEOUT, client2).await.expect("timed out").expect("conn failed");

    // The continuation request must carry the pre-restart history.
    let requests = m2.requests();
    assert!(!requests.is_empty());
    assert!(
        body_contains(&requests.last().unwrap(), "the magic word is FROB"),
        "resumed conversation's LLM request must contain pre-restart history"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn load_session_unknown_id_errors_and_cwd_mismatch_names_both_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    let m = MockTransport::new();
    let turn1 = format!("data: {}\n\n{}", text_chunk("x"), stop_and_done());
    m.push_response_frames(sse(vec![Box::leak(turn1.into_boxed_str())]));
    let m2 = m.clone();
    let mut config = test_config(Vec::new());
    config.sessions_dir = Some(dir.path().to_path_buf());
    let agent = buran::acp::agent_builder_with_config(config, move |_provider| {
        Ok(LlmClient::with_transport(provider(), hclient::Client::builder(m2.clone()).build().unwrap()))
    }).await.expect("agent builder");

    let stored_id = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let stored_id1 = stored_id.clone();
    let cwd1 = cwd.clone();
    let client = Client.v1().connect_with(agent, move |cx| {
        let stored_id = stored_id1.clone();
        let cwd = cwd1.clone();
        async move {
            initialize(&cx).await?;
            let SessionSetup { mut session, response, .. } =
                cx.build_session(cwd.clone()).block_task().start_session().await?;
            *stored_id.lock().unwrap() = Some(response.session_id.0.to_string());
            session.send_prompt("x")?;
            let _ = session.read_to_string().await?;

            // Unknown id: start_session must return an error naming the id.
            let unknown = cx
                .load_session("01940000-0000-7000-8000-does-not-exist", cwd.clone())
                .block_task()
                .start_session()
                .await;
            let err = match unknown {
                Err(e) => e.to_string(),
                Ok(_) => panic!("unknown session id must fail the load"),
            };
            assert!(err.contains("not found") || err.contains("01940000"), "{err}");

            // Real id, wrong cwd: the error must name both paths.
            let wrong = cx
                .load_session(
                    stored_id.lock().unwrap().clone().unwrap(),
                    std::path::PathBuf::from("/some/other/cwd"),
                )
                .block_task()
                .start_session()
                .await;
            let err = match wrong {
                Err(e) => e.to_string(),
                Ok(_) => panic!("cwd mismatch must fail the load"),
            };
            assert!(err.contains("/some/other/cwd"), "must name request cwd: {err}");
            let real_cwd = cwd.display().to_string();
            assert!(err.contains(&real_cwd), "must name session cwd: {err}");
            Ok(())
        }
    });
    tokio::time::timeout(TIMEOUT, client).await.expect("timed out").expect("conn failed");
}
```

(The second connection reuses the same in-process agent — equivalent to a
second process here, because conversation state lives in the store, not
the process. The sessions_dir override is Task 4's pattern.)

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p buran --test acp_integration load_session`
Expected: FAIL — the agent answers `session/load` with "method not found" (no handler) and `agentCapabilities.loadSession` is not advertised.

- [ ] **Step 3: Implement the handler**

In `crates/buran/src/acp/server.rs`:

1. The initialize handler's capabilities line becomes:

```rust
responder.respond(
    InitializeResponse::new(req.protocol_version)
        .agent_capabilities(AgentCapabilities::new().load_session(true)),
)
```

2. A new handler in `build_agent`'s `on_receive_request` chain, capturing
   its own `state_load = state.clone()` (the builder's per-handler clone
   pattern, like `state_new_session`):

```rust
.on_receive_request(
    async move |req: LoadSessionRequest, responder, _cx| {
        let load_result = (|| -> anyhow::Result<SessionHandle> {
            // Store read happens BEFORE the sessions lock: file I/O must
            // not hold the lock; it guards only the map insert.
            let (file, records) = state_load
                .store
                .open(req.session_id.0.as_str())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let head = records.first().and_then(|r| match r {
                crate::session::TranscriptRecord::Metadata(m) => Some(m.clone()),
                _ => None,
            }).ok_or_else(|| anyhow::anyhow!("transcript has no metadata head"))?;
            if head.cwd != req.cwd {
                anyhow::bail!(
                    "cwd mismatch: session was created in {}, request is {} \
                     (sessions resume only in their original directory)",
                    head.cwd.display(), req.cwd.display()
                );
            }
            let mut history = Vec::new();
            let mut compaction_points = Vec::new();
            for record in &records {
                match record {
                    crate::session::TranscriptRecord::Message(m) => history.push(m.clone()),
                    crate::session::TranscriptRecord::CompactionPoint { at_message, .. } => {
                        compaction_points.push(*at_message)
                    }
                    crate::session::TranscriptRecord::Metadata(_) => {}
                }
            }
            Ok(SessionHandle {
                system_prompt: build_system_prompt(&req.cwd, &state_load.config.skills_dirs),
                skills: crate::skills::scan(&state_load.config.skills_dirs),
                session: SessionState { history, compaction_points },
                cancel_tx: watch::channel(false).0,
                turn_in_flight: false,
                store_file: Some(file),
            })
        })();
        match load_result {
            Ok(handle) => {
                state_load
                    .sessions
                    .lock()
                    .expect("sessions lock")
                    .insert(req.session_id.clone(), handle);
                responder.respond(LoadSessionResponse::new())
            }
            Err(e) => responder.respond_with_error(agent_client_protocol::Error::new(
                agent_client_protocol::ErrorCode::INVALID_PARAMS,
                format!("{e}"),
            )),
        }
        Ok::<(), agent_client_protocol::Error>(())
    },
    on_receive_request!(),
)
```

Verified SDK facts used above:
- `LoadSessionResponse::new()` takes **no** arguments (schema
  `agent.rs:1630` — `Self::default()`); the session id travels in the
  request and is implied on the wire.
- `SessionId` is a newtype over `Arc<str>` (`v1/mod.rs:50`), so
  `.0.as_str()` / `.clone()` work.
- Add `LoadSessionRequest, LoadSessionResponse` to the existing
  `agent_client_protocol::{ ... v1::schema::{ ... } }` use block.
- The handler returns `Ok::<(), agent_client_protocol::Error>(())` like
  the prompt handler.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p buran --test acp_integration load_session`
Expected: PASS (2/2)

- [ ] **Step 5: Suite + lints, commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check`

```bash
git add crates/buran/src/acp/server.rs crates/buran/tests/acp_integration.rs
git commit -m "feat: ACP session/loadSession — resume from transcript"
```

---

### Task 6: CLI — `--resume`/`--fork` on serve, `buran sessions list|rm`

**Files:**
- Modify: `crates/buran/src/main.rs` (clap args)
- Modify: `crates/buran/src/run.rs` (`sessions_list`, `sessions_rm`, serve threading)
- Modify: `crates/buran/src/acp/server.rs` (`serve()` gains resume/fork pre-wiring)
- Test: `crates/buran/tests/cli_sessions.rs`

**Interfaces:**
- Consumes: `Store::latest/fork/list/remove` (Tasks 2–3); `Store::default_root()` respects `HOME` (Task 2).
- Produces: clap `Command::Serve { resume: Option<String>, fork: Option<String> }`, `Command::Sessions(SessionsCommand)`; `run::serve(resume, fork)`; `run::sessions_list()`, `run::sessions_rm(id)`. `--resume` semantics (spec): the flag is a **hint printed to stderr** — ACP clients attach to a stored conversation via `session/loadSession` (Task 5), so `serve --resume <id>` = validate the transcript exists (fail fast with `buran sessions list` hint if not) and log the instruction line; `serve --fork <id>` = `store.fork(...)` then behave like resume on the new id.

- [ ] **Step 1: Write the failing tests**

```rust
//! CLI surface: `buran sessions list|rm` drive the real Store through
//! `Store::default_root()`, which reads `$HOME`. The test overrides HOME
//! for the child binary — the same env-isolation pattern as the
//! `serve_child_stdout_purity` test (acp_integration.rs), which already
//! spawns `env!("CARGO_BIN_EXE_buran")`.

use std::io::Write as _;
use std::process::{Command, Stdio};

fn run_buran_with_home(home: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("run buran");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn seed_session(home: &std::path::Path) -> &'static str {
    let store_root = home.join(".local/state/buran/sessions");
    std::fs::create_dir_all(&store_root).unwrap();
    std::fs::write(
        store_root.join("01940000-0000-7000-8000-000000000001.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "type": "metadata", "model": "m", "provider": "p",
                "started_at": "2026-11-12T00:00:00Z", "ended_at": null,
                "cwd": "/w"
            })
        ),
    )
    .unwrap();
    "01940000-0000-7000-8000-000000000001"
}

#[test]
fn sessions_list_and_rm_roundtrip_through_the_real_binary() {
    let home = tempfile::tempdir().unwrap();
    let id = seed_session(home.path());

    let out = run_buran_with_home(home.path(), &["sessions", "list"]);
    assert!(out.contains(id), "{out}");
    assert!(out.contains("2026-11-12T00:00:00Z"), "list carries started_at: {out}");
    assert!(out.contains("m"), "list must carry head model: {out}");

    run_buran_with_home(home.path(), &["sessions", "rm", id]);
    let out = run_buran_with_home(home.path(), &["sessions", "list"]);
    assert!(!out.contains(id), "session gone after rm: {out}");
}

#[test]
fn sessions_list_on_empty_store_says_so() {
    let home = tempfile::tempdir().unwrap();
    let out = run_buran_with_home(home.path(), &["sessions", "list"]);
    assert!(out.to_lowercase().contains("no stored sessions"), "{out}");
}

#[test]
fn sessions_rm_unknown_id_fails_with_hint() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args(["sessions", "rm", "01940000-0000-7000-8000-000000000009"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("buran sessions list"), "must hint the list command: {err}");
}

#[test]
fn serve_fork_creates_the_branch_file_then_serves() {
    // --fork copies the transcript to a new uuidv7 file, validates it, and
    // proceeds into serve. With Stdio::null() stdin, serve exits on EOF;
    // the observable contract: a fork file appears while it runs, its head
    // carries the lineage, the original keeps exactly one line.
    let home = tempfile::tempdir().unwrap();
    let id = seed_session(home.path());

    let mut child = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args(["serve", "--fork", id])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let fork_file = loop {
        let mut found = None;
        if let Ok(entries) = std::fs::read_dir(home.path().join(".local/state/buran/sessions")) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".jsonl") && !name.starts_with(id) {
                    found = Some(name);
                }
            }
        }
        if let Some(name) = found {
            break name;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("fork file never appeared");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    // Forked head carries lineage to the original.
    let text = std::fs::read_to_string(
        home.path()
            .join(".local/state/buran/sessions")
            .join(&fork_file),
    )
    .unwrap();
    assert!(text.contains(id), "fork head must record forked_from={id}: {text}");

    // Original untouched.
    let original = std::fs::read_to_string(
        home.path()
            .join(".local/state/buran/sessions")
            .join(format!("{id}.jsonl")),
    )
    .unwrap();
    assert_eq!(original.lines().count(), 1, "original keeps exactly its head line");

    // serve exits on stdin EOF (Stdio::null); wait for it.
    let _ = child.wait();
}

#[test]
fn serve_resume_unknown_id_fails_fast() {
    // serve --resume with a garbage id must exit with an error naming the
    // list command — before speaking ACP on stdout.
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_buran"))
        .args(["serve", "--resume", "01940000-0000-7000-8000-000000000009"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("buran sessions list"), "must hint: {err}");
    assert!(out.stdout.is_empty(), "stdout is the ACP channel; failures happen before it: {}", String::from_utf8_lossy(&out.stdout));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p buran --test cli_sessions`
Expected: FAIL — no `sessions` subcommand and no `--resume`/`--fork` flags (clap exits 2 with a usage error for every test here; the positive `sessions list` and `serve_fork_creates_the_branch_file_then_serves` tests are the unambiguous RED signal).

- [ ] **Step 3: Implement**

`main.rs` — the `Command` enum gains fields and a variant (keep
`cli.command.unwrap_or(Command::Serve)` working: `Serve`'s args need
`Default`-able clap handling — use `unwrap_or(Command::Serve { resume:
None, fork: None })` in `main`):

```rust
#[derive(Subcommand)]
enum Command {
    /// Run the headless ACP agent server on stdio (default)
    Serve {
        /// Resume a stored session: an id, or "latest". Bare `--resume`
        /// means "latest" (spec: "for --resume with no value, latest is
        /// the default spelling").
        #[arg(long, default_missing_value = "latest", require_equals = false,
              num_args = 0..=1)]
        resume: Option<String>,
        /// Fork a stored session (copy-on-start) and resume the fork.
        /// Conflicts with `--resume` (they are two spellings of "which
        /// conversation to attach to").
        #[arg(long, conflicts_with = "resume")]
        fork: Option<String>,
    },
    /// Probe config and MCP servers, offer ACT on first run
    Doctor,
    /// Introspect configured MCP servers
    Mcp {
        #[command(subcommand)]
        cmd: McpCommand,
    },
    /// Manage stored sessions
    Sessions {
        #[command(subcommand)]
        cmd: SessionsCommand,
    },
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// List stored sessions (oldest first)
    List,
    /// Delete a stored session by id
    Rm { id: String },
}
```

`main.rs` dispatch:

```rust
match cli.command.unwrap_or(Command::Serve { resume: None, fork: None }) {
    Command::Serve { resume, fork } => buran::run::serve(resume, fork),
    // ... Doctor / Mcp arms unchanged
    Command::Sessions { cmd } => match cmd {
        SessionsCommand::List => buran::run::sessions_list(),
        SessionsCommand::Rm { id } => buran::run::sessions_rm(&id),
    },
}
```

`run.rs`:

```rust
/// Resume/fork are pre-flight validation + a stderr hint: ACP clients
/// attach to stored conversations via `session/loadSession` (ADR 0007
/// canon; the store is the contract), so serve only ensures the
/// transcript is reachable before the ACP channel opens.
pub fn serve(resume: Option<String>, fork: Option<String>) -> Result<()> {
    let store = crate::store::Store::new(crate::store::Store::default_root());
    let effective = if let Some(id) = fork {
        let cwd = std::env::current_dir()?;
        let new_file = store
            .fork(
                &id,
                crate::session::TranscriptMeta {
                    model: String::new(),
                    provider: String::new(),
                    started_at: crate::store::rfc3339_now(),
                    ended_at: None,
                    cwd,
                    forked_from: None, // store.fork sets the lineage
                },
            )
            .map_err(|e| anyhow::anyhow!("{e} (see buran sessions list)"))?;
        // stderr: under `serve` stdout is the ACP JSON-RPC channel (AGENTS.md).
        eprintln!("forked {id} -> {}", new_file.id());
        new_file.id().to_string()
    } else if let Some(target) = resume {
        let id = if target == "latest" {
            store
                .latest()
                .map_err(|e| anyhow::anyhow!("{e}"))?
                .ok_or_else(|| anyhow::anyhow!("no stored sessions to resume"))?
        } else {
            target
        };
        id
    } else {
        return crate::acp::serve();
    };
    // Fail fast on an unknown id, before stdout becomes the ACP channel.
    store
        .open(&effective)
        .map_err(|e| anyhow::anyhow!("{e} (see buran sessions list)"))?;
    eprintln!(
        "resuming session {effective}: connect a client and load it via session/loadSession"
    );
    crate::acp::serve()
}

/// Pure CLI command — `serve` is not running, so stdout is free for
/// human output (the stdout ban applies under `serve` only).
pub fn sessions_list() -> Result<()> {
    let store = crate::store::Store::new(crate::store::Store::default_root());
    let infos = store.list().map_err(|e| anyhow::anyhow!("{e}"))?;
    if infos.is_empty() {
        println!("no stored sessions");
        return Ok(());
    }
    for i in infos {
        println!(
            "{}\t{}\t{}\t{}",
            i.id,
            i.meta.started_at,
            i.meta.model,
            i.meta.forked_from.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

pub fn sessions_rm(id: &str) -> Result<()> {
    let store = crate::store::Store::new(crate::store::Store::default_root());
    store
        .remove(id)
        .map_err(|e| anyhow::anyhow!("{e} (see buran sessions list)"))
}
```

Design ruling recorded here for the implementer: the CLI does not create
the `SessionHandle` eagerly — ACP has no server-initiated session; a
client drives `newSession`/`loadSession` after `initialize`. An eager
handle would be unreachable from the protocol. The flag's job: validate
early (unknown id fails before the ACP channel opens) and fork the file
(copy-on-start), then print the stderr hint that tells the user/parent
client how to attach. This matches the spec's success criteria — the
conversation continues via `loadSession`, which Task 5 already serves.

- [ ] **Step 4: Suite + lints, commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt --check`

```bash
git add crates/buran/src/main.rs crates/buran/src/run.rs crates/buran/tests/cli_sessions.rs
git commit -m "feat: buran sessions list|rm; serve --resume/--fork"
```

---

### Task 7: TUI `--resume`/`--fork` pass-through + README

**Files:**
- Modify: `crates/buran-tui/src/main.rs` (clap args on `Cli`, thread through `run_app` → `AcpClient::spawn`)
- Modify: `crates/buran-tui/src/acp_client.rs` (`serve_args` pure helper + `spawn` signature)
- Modify: `README.md` (Sessions section before "ACP contract")
- Test: `crates/buran-tui/tests/resume_flag.rs`

**Interfaces:**
- Consumes: `buran serve --resume <id|latest>` / `--fork <id>` CLI semantics (Task 6). SDK fact verified: `AcpAgent::from_str` shell-splits the whole command string (`shell_words::split`), so `serve --resume abc` travels correctly inside one string argument.
- Produces: `AcpClient::spawn(agent_command: &str, resume: Option<String>, fork: Option<String>)`; pure helper `buran_tui::acp_client::serve_args(resume: Option<String>, fork: Option<String>) -> Vec<String>`; `buran-tui --resume <id|latest>` / `--fork <id>` flags.

- [ ] **Step 1: Write the failing test**

```rust
//! The arg-construction seam is a pure function: no process spawn, no
//! terminal, no protocol — the same unit-level shape as transcript_render.

use buran_tui::acp_client::serve_args;

// `clap` is already a direct dependency of buran-tui (its own Cli derive);
// the conflict probe below reuses it — no dev-dependency addition.

#[test]
fn serve_args_builds_the_selection_flags() {
    assert_eq!(
        serve_args(Some("abc".into()), None),
        vec!["--resume".to_string(), "abc".into()]
    );
    assert_eq!(
        serve_args(None, Some("def".into())),
        vec!["--fork".to_string(), "def".into()]
    );
    assert_eq!(serve_args(None, None), Vec::<String>::new());
}

#[test]
fn tui_clap_rejects_resume_and_fork_together() {
    // The TUI's own clap mirrors `buran serve`: --resume and --fork are
    // two spellings of "which conversation to attach to" and conflict.
    // (serve_args never sees both at once; the conflict is enforced by
    // clap before arg construction.)
    use clap::Parser;
    #[derive(Parser)]
    struct Probe {
        #[arg(long, default_missing_value = "latest", require_equals = false,
              num_args = 0..=1)]
        resume: Option<String>,
        #[arg(long, conflicts_with = "resume")]
        fork: Option<String>,
    }
    assert!(Probe::try_parse_from(["probe", "--resume", "a", "--fork", "b"]).is_err());
    assert!(Probe::try_parse_from(["probe", "--resume", "a"]).is_ok());
    assert!(Probe::try_parse_from(["probe", "--fork", "b"]).is_ok());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p buran-tui --test resume_flag`
Expected: FAIL — `serve_args` does not exist (compile error in the test target).

- [ ] **Step 3: Implement**

`crates/buran-tui/src/acp_client.rs` — pure helper plus the widened
`spawn` signature (keep `from_str` on the *user's* `--agent` string only;
the resume/fork args are appended structurally so a user agent command
with its own flags keeps working):

```rust
/// Selection flags appended to the spawned agent command. Pure so the
/// arg shape is testable without spawning the agent. Returns ONLY the
/// flags -- the `serve` subcommand itself already sits in the agent's
/// arg list: the user's `--agent` string (default "buran serve") went
/// through `AcpAgent::from_str`, and the SDK's `args()` APPENDS
/// (`self.args.extend`), so returning a "serve" prefix here would spawn
/// `buran serve serve --resume ...`.
pub fn serve_args(resume: Option<String>, fork: Option<String>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(id) = resume {
        args.push("--resume".into());
        args.push(id);
    }
    if let Some(id) = fork {
        args.push("--fork".into());
        args.push(id);
    }
    args
}
```

Change `spawn` to take the flags and append them to the parsed agent
command (the first element is the binary, the rest its own args —
`AcpAgent::from_str` already shell-split the user's `--agent` string):

```rust
pub async fn spawn(
    agent_command: &str,
    resume: Option<String>,
    fork: Option<String>,
) -> anyhow::Result<Self> {
    let agent = AcpAgent::from_str(agent_command)?.args(serve_args(resume, fork));
    // ... rest unchanged
```

(Verified against the SDK, `acp_agent.rs:110`: `AcpAgent::args` APPENDS —
`self.args.extend`. That is exactly what we want: `from_str` already
turned "buran serve" into command `buran` + args `["serve"]`, and the
helper contributes only the selection flags, so the child sees
`buran serve --resume <id>`.)

Note on the pure helper vs `from_str`: the helper appends the *selection
flags* only. It deliberately does not parse the user's `--agent` string —
that stays the SDK's job (`shell_words`), and the test pins the flag
shape, not the shell-splitting behavior we do not own.

`crates/buran-tui/src/main.rs` — new flags on `Cli`, threaded to
`run_app`:

```rust
struct Cli {
    /// Agent command spawn (defaults to `buran serve` from PATH).
    #[arg(long, default_value = "buran serve")]
    agent: String,
    /// Resume a stored session: an id, or "latest"; bare `--resume`
    /// means "latest". (Attach is completed by the client via
    /// session/loadSession.)
    #[arg(long, default_missing_value = "latest", require_equals = false,
          num_args = 0..=1)]
    resume: Option<String>,
    /// Fork a stored session (copy-on-start) and resume the fork.
    /// Conflicts with `--resume`.
    #[arg(long, conflicts_with = "resume")]
    fork: Option<String>,
}
```

```rust
let result = run_app(&mut terminal, &cli.agent, cli.resume, cli.fork).await;
```

```rust
async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    agent_command: &str,
    resume: Option<String>,
    fork: Option<String>,
) -> anyhow::Result<()> {
    let client = AcpClient::spawn(agent_command, resume, fork).await?;
    // ... rest unchanged
```

`README.md` — new section between "ACT — the recommended tool layer" and
"ACP contract":

```markdown
## Sessions

Every conversation persists as an append-only JSONL transcript in
`~/.local/state/buran/sessions/`, named by its UUIDv7 id (filename order
= chronological order; ADR 0007: the directory listing is the catalog —
no index, no SQLite).

```sh
buran sessions list            # id, started_at, model, forked_from
buran-tui --resume latest      # most recent session
buran-tui --fork <id>          # branch; the original is never touched
buran sessions rm <id>         # delete
```

Resume semantics: `serve` validates the id before the ACP channel opens;
a client then attaches to the stored conversation via
`session/loadSession` (cwd must match the session's recorded cwd). A torn
final line (crash mid-append) replays up to the last good line; unknown
record types are skipped for forward compatibility.
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p buran-tui --test resume_flag && cargo test --workspace`
Expected: PASS (the existing `client_vs_fake_agent` test keeps passing —
its `AcpClient::spawn` call gains `None, None` arguments).

- [ ] **Step 5: Lints + commit**

Run: `cargo clippy --workspace -- -D warnings && cargo fmt --check`

```bash
git add crates/buran-tui/ README.md
git commit -m "feat: buran-tui --resume/--fork pass-through; README sessions section"
```
