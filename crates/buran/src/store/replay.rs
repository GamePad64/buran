//! Reader rules (ADR 0007): unknown record types are skipped; a torn
//! final line is a warning + stop; a corrupt line anywhere else is fatal.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use super::StoreError;
use crate::session::{TranscriptMeta, TranscriptRecord};

/// All records from a well-formed prefix of the file.
pub(super) fn replay(path: &Path) -> Result<Vec<TranscriptRecord>, StoreError> {
    let (records, _) = replay_inner(path)?;
    Ok(records)
}

/// Byte length of the file's well-formed prefix (the append position).
pub(super) fn good_prefix_len(path: &Path) -> Result<u64, StoreError> {
    replay_inner(path).map(|(_, len)| len)
}

/// Extract the head metadata by reading exactly ONE line (for `Store::list`).
/// The catalog must not fail a whole session because of a corrupt line in
/// its body — only the head line is inspected here, so body damage stays
/// invisible to `list` (full validation remains `open`'s job).
///
/// A torn FIRST line (a crash before the head was fully written) cannot
/// yield metadata, so the session has no identity: reported as `Corrupt`,
/// which `list` propagates. That file is damage from a failed `create`
/// rather than a listable session; dropping it with a loud error beats
/// silently listing a session that cannot later be `open`ed.
pub(super) fn head_meta(path: &Path) -> Result<TranscriptMeta, StoreError> {
    let file = File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => StoreError::NotFound(
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ),
        _ => e.into(),
    })?;
    let mut line = String::new();
    BufReader::new(file)
        .read_line(&mut line)
        .map_err(StoreError::Io)?;
    let trimmed = line.trim_end_matches(['\n', '\r']);
    match serde_json::from_str::<TranscriptRecord>(trimmed) {
        Ok(TranscriptRecord::Metadata(meta)) => Ok(meta),
        Ok(_) => Err(StoreError::Corrupt {
            path: path.to_path_buf(),
            reason: "first line is not a metadata head".into(),
        }),
        Err(e) => Err(StoreError::Corrupt {
            path: path.to_path_buf(),
            reason: format!("head line: {e}"),
        }),
    }
}

/// Single pass: each line is judged as it is read, and a parse failure is
/// torn-tail-tolerated only when nothing follows the failed line — checked
/// by one more `read_line` (0 bytes back == clean EOF right after). This
/// avoids re-reading the file to count lines (the brief's sketch did that;
/// the brief's simplification note prefers this shape).
fn replay_inner(path: &Path) -> Result<(Vec<TranscriptRecord>, u64), StoreError> {
    let file = File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => StoreError::NotFound(
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
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
            Err(_) if is_unknown_type(trimmed) => {
                // Forward compat: skip, keep the bytes (never rewrite).
                tracing::debug!(?path, line_no, "skipping unknown record type");
            }
            Err(e) if !has_more_lines(&mut reader)? => {
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

/// True when parseable JSON carrying a `type` we didn't recognize —
/// a record type from a newer buran (ADR 0007 forward compat).
fn is_unknown_type(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .is_some()
}

/// Peek past the current line: `Ok(false)` means the file ends right here,
/// so a parse failure on that line is a torn tail, not mid-file corruption.
fn has_more_lines(reader: &mut BufReader<File>) -> Result<bool, StoreError> {
    let mut peek = String::new();
    Ok(reader.read_line(&mut peek).map_err(StoreError::Io)? > 0)
}
