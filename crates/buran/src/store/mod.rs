//! Session store: one append-only JSONL file per session (ADR 0007).
//! The directory listing IS the catalog — no index file; any future
//! index is a derived cache (ADR 0007 policy).

mod replay;
mod writer;

pub use writer::{StoreCmd, WriterHandle};

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, Write};
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

/// Current UTC time as an RFC 3339 string. Hand-rolled civil-from-days
/// conversion (Howard Hinnant's algorithm) — no chrono/jiff dependency for
/// one timestamp; `pub` because the CLI's `run.rs` needs it across the
/// crate boundary.
pub fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        let head = serde_json::to_string(&TranscriptRecord::Metadata(meta))
            .expect("TranscriptRecord serialization cannot fail: no maps/floats");
        file.write_all(head.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(SessionFile { id, path, file })
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
        Ok((
            SessionFile {
                id: id.to_string(),
                path: path.clone(),
                file,
            },
            records,
        ))
    }

    pub fn list(&self) -> Result<Vec<SessionInfo>, StoreError> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let head = replay::head_meta(&entry.path())?;
            out.push(SessionInfo {
                id: stem.to_string(),
                meta: head,
            });
        }
        // UUIDv7: lexicographic == chronological.
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// The session's head metadata, read without opening (no truncation,
    /// no append handle): the pre-flight read for `--fork`, which must
    /// copy the source's model/provider/cwd into the fork head without
    /// mutating or repairing the source.
    pub fn head(&self, id: &str) -> Result<TranscriptMeta, StoreError> {
        replay::head_meta(&self.path_for(id))
    }

    /// Copies all records except the head; writes a fresh head carrying
    /// `forked_from`. The original file is never opened for writing —
    /// existence is checked on the path and replay runs on a read-only
    /// handle, so a torn tail on the source is NOT truncated as a side
    /// effect (a fork repairs nothing in the transcript it forks from).
    pub fn fork(&self, id: &str, mut fork_meta: TranscriptMeta) -> Result<SessionFile, StoreError> {
        let path = self.path_for(id);
        if !path.exists() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        fork_meta.forked_from = Some(id.to_string());
        // Replay BEFORE create: a corrupt mid-file source must fail the
        // fork without leaving an orphan fork file behind (a fresh head
        // alone in the catalog).
        let records = replay::replay(&path)?;
        let mut new_file = self.create(fork_meta)?;
        for record in records {
            if matches!(record, TranscriptRecord::Metadata(_)) {
                continue; // original head replaced by the fresh one
            }
            new_file.append(&record)?;
        }
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
    path: PathBuf,
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
            path: self.path.clone(),
            reason: format!("serialize: {e}"),
        })?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        Ok(())
    }
}
