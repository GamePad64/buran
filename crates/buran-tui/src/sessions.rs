//! Session catalog for the picker. The TUI crate has no dependency on the
//! buran store (the ACP stdio seam is the contract — same ruling as
//! `latest_session_id`), so this re-implements the two rules the picker
//! needs: the title is the first user message, and uuidv7 filenames sort
//! chronologically. It reads a bounded head of each file, never a full
//! transcript.
//!
//! `loadSession` only succeeds in the directory the session was created
//! in, so the picker splits the catalog: loadable rows (cwd matches) and
//! a count of the rest. A row you cannot open is a trap; a count is not.

use std::path::{Path, PathBuf};

/// Longest title, matching `buran::store::TITLE_MAX_CHARS`. The two copies
/// exist because this crate must not depend on that one; the cap is part
/// of the displayed contract, so a drift shows up as a truncated title,
/// not a crash.
const TITLE_MAX_CHARS: usize = 80;

/// One catalog row the picker can render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    /// First user message, whitespace-collapsed and ellipsis-capped.
    /// `None` while the session has no user message.
    pub title: Option<String>,
    /// RFC 3339 from the transcript head.
    pub started_at: String,
    pub cwd: PathBuf,
}

/// What the picker shows: loadable rows (newest first) and how many
/// sessions live in other directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerCatalog {
    pub rows: Vec<SessionRow>,
    pub hidden: usize,
}

/// First user message, one line, ellipsis-capped. Same rule as
/// `buran::store::session_title`.
pub fn session_title(raw: &str, max: usize) -> String {
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max - 1).collect();
    format!("{cut}…")
}

/// "12m", "3h", "2d", "4w" — coarse on purpose: the picker is for
/// recognition, not timestamps. Future or unparseable stamps render as
/// the raw string rather than a wrong age.
pub fn relative_age(started_at: &str, now: jiff::Timestamp) -> String {
    let Ok(then) = started_at.parse::<jiff::Timestamp>() else {
        return started_at.to_string();
    };
    if then > now {
        return started_at.to_string();
    }
    let secs = now.duration_since(then).as_secs() as u64;
    let (n, unit) = if secs < 60 {
        (secs, "s")
    } else if secs < 3600 {
        (secs / 60, "m")
    } else if secs < 86_400 {
        (secs / 3600, "h")
    } else if secs < 7 * 86_400 {
        (secs / 86_400, "d")
    } else {
        (secs / (7 * 86_400), "w")
    };
    format!("{n}{unit}")
}

/// Scans `root` for `*.jsonl`, keeps rows whose cwd equals `here`, counts
/// the rest. Newest first (uuidv7 stem order, reversed). A file whose head
/// is not metadata is skipped: it cannot be loaded, and listing it would
/// offer a row that errors.
pub fn catalog(root: &Path, here: &Path) -> PickerCatalog {
    let mut loadable = Vec::new();
    let mut hidden = 0usize;
    let Ok(entries) = std::fs::read_dir(root) else {
        return PickerCatalog {
            rows: loadable,
            hidden,
        };
    };
    let mut found: Vec<SessionRow> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".jsonl")?.to_string();
            read_row(&entry.path(), id)
        })
        .collect();
    // uuidv7: lexicographic == chronological. Newest first.
    found.sort_by(|a, b| b.id.cmp(&a.id));
    for row in found {
        if row.cwd == here {
            loadable.push(row);
        } else {
            hidden += 1;
        }
    }
    PickerCatalog {
        rows: loadable,
        hidden,
    }
}

/// Store root read from `$HOME` — the same XDG path the spawned `serve`
/// writes (see `acp_client::store_root` for the shared-HOME ruling; the
/// function is duplicated here because `main.rs`'s module tree is the
/// binary's, and the picker is driven from there).
pub fn store_root() -> std::path::PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::Path::new(&home).join(".local/state")
        })
        .join("buran/sessions")
}

/// Head metadata plus the first user message, scanning at most a few
/// hundred lines. Body damage ends the scan; a title found before it
/// still stands.
fn read_row(path: &Path, id: String) -> Option<SessionRow> {
    const MAX_LINES: usize = 200;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut started_at = None;
    let mut cwd = None;
    let mut title = None;
    for i in 0..MAX_LINES {
        let mut line = String::new();
        let n = std::io::BufRead::read_line(&mut reader, &mut line).ok()?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            break;
        };
        if i == 0 {
            if value.get("type").and_then(|t| t.as_str()) != Some("metadata") {
                return None;
            }
            started_at = value
                .get("started_at")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            cwd = value.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);
        } else if title.is_none()
            && value.get("type").and_then(|t| t.as_str()) == Some("message")
            && value.get("role").and_then(|r| r.as_str()) == Some("user")
            && let Some(content) = value.get("content").and_then(|c| c.as_str())
            && content.split_whitespace().next().is_some()
        {
            title = Some(session_title(content, TITLE_MAX_CHARS));
            break;
        }
    }
    Some(SessionRow {
        id,
        title,
        started_at: started_at?,
        cwd: cwd?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path, id: &str, cwd: &str, ask: Option<&str>) {
        let mut body = format!(
            "{}\n",
            serde_json::json!({
                "type": "metadata", "model": "m", "provider": "p",
                "started_at": "2026-11-12T00:00:00Z", "cwd": cwd
            })
        );
        if let Some(ask) = ask {
            body.push_str(&format!(
                "{}\n",
                serde_json::json!({"type": "message", "role": "user", "content": ask})
            ));
        }
        std::fs::write(dir.join(format!("{id}.jsonl")), body).unwrap();
    }

    #[test]
    fn catalog_keeps_matching_cwd_newest_first_and_counts_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        write_session(
            dir.path(),
            "01940000-0000-7000-8000-000000000001",
            "/proj",
            Some("older"),
        );
        write_session(
            dir.path(),
            "01940000-0000-7000-8000-000000000003",
            "/proj",
            Some("newer"),
        );
        write_session(
            dir.path(),
            "01940000-0000-7000-8000-000000000002",
            "/other",
            Some("elsewhere"),
        );

        let cat = catalog(dir.path(), Path::new("/proj"));
        assert_eq!(cat.rows.len(), 2);
        assert_eq!(cat.rows[0].id, "01940000-0000-7000-8000-000000000003");
        assert_eq!(cat.rows[0].title.as_deref(), Some("newer"));
        assert_eq!(cat.rows[1].title.as_deref(), Some("older"));
        assert_eq!(cat.hidden, 1);
    }

    #[test]
    fn empty_session_has_no_title_but_still_lists() {
        let dir = tempfile::tempdir().unwrap();
        write_session(
            dir.path(),
            "01940000-0000-7000-8000-000000000001",
            "/proj",
            None,
        );
        let cat = catalog(dir.path(), Path::new("/proj"));
        assert_eq!(cat.rows[0].title, None);
    }

    #[test]
    fn missing_store_is_an_empty_catalog() {
        let cat = catalog(Path::new("/no/such/store"), Path::new("/proj"));
        assert!(cat.rows.is_empty());
        assert_eq!(cat.hidden, 0);
    }

    #[test]
    fn relative_age_buckets() {
        // 2026-11-12T00:00:00Z == a fixed instant; ages measured from it.
        let t: jiff::Timestamp = "2026-11-12T00:00:00Z".parse().unwrap();
        let at = |secs: i64| t + jiff::SignedDuration::from_secs(secs);
        assert_eq!(relative_age("2026-11-12T00:00:00Z", at(90)), "1m");
        assert_eq!(relative_age("2026-11-12T00:00:00Z", at(3 * 3600)), "3h");
        assert_eq!(relative_age("2026-11-12T00:00:00Z", at(2 * 86_400)), "2d");
        assert_eq!(relative_age("not a date", t), "not a date");
    }
}
