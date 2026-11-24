//! AGENTS.md loader for system instructions.
//!
//! Gateway-style layering (ADR 0003 + configurable names): the searched
//! FILE NAMES come from config (default `["AGENTS.md"]`; adding
//! `CLAUDE.md` lets buran absorb Claude Code projects). Three phases,
//! order preserved end-to-end:
//!
//! 1. **Collect** — walk ancestors leaf → root; per directory, the
//!    configured names in config order.
//! 2. **Canonicalize** — resolve symlinks; broken links drop out
//!    (fail-open, like unreadable layers).
//! 3. **Dedup, first occurrence wins** — a `CLAUDE.md -> AGENTS.md`
//!    symlink twin loads once, at the SYMLINK's position (the nearest
//!    layer speaks for its directory).
//!
//! Unreadable layers are skipped with a debug log — a broken instruction
//! file must never crash the server.

use std::path::{Path, PathBuf};

use itertools::Itertools as _;

/// Default searched names (ADR 0003).
pub const DEFAULT_FILES: &[&str] = &["AGENTS.md"];

/// Collects instruction-file instructions from `project_dir` and its
/// ancestors, searching `names` (in config order). Empty string when
/// nothing is found.
pub fn load_system_instructions_with(project_dir: Option<&Path>, names: &[String]) -> String {
    let Some(project_dir) = project_dir else {
        return String::new();
    };
    if names.is_empty() {
        return String::new();
    }

    // Phase 1: collect candidate paths — ROOT → leaf (general rules speak
    // before specific ones, ADR 0003); within a directory, config order.
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = project_dir.ancestors().map(Path::to_path_buf).collect();
    dirs.reverse(); // ancestors() is leaf → root; root must speak first
    for dir in dirs {
        for name in names {
            let path = dir.join(name);
            if path.is_file() {
                candidates.push(path);
            }
        }
    }

    // Phase 2: canonicalize (resolves symlinks + normalizes). Broken
    // links fail-open: dropped here, logged.
    let mut canonical: Vec<PathBuf> = Vec::new();
    for path in candidates {
        match path.canonicalize() {
            Ok(real) => canonical.push(real),
            Err(source) => {
                tracing::debug!(?path, %source, "skipping broken instruction-file link");
            }
        }
    }

    // Phase 3: dedup keeping FIRST occurrence — a symlink twin loads once,
    // at the symlink's position. `unique()` is global first-wins, same
    // contract the HashSet version had, without the manual insert-filter.
    let texts: Vec<String> = canonical
        .into_iter()
        .unique()
        .filter_map(|real| {
            std::fs::read_to_string(&real)
                .map_err(|source| {
                    tracing::debug!(?real, %source, "skipping unreadable instruction file");
                    source
                })
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        })
        .collect();

    let joined = texts.join("\n\n");
    tracing::debug!(
        files = texts.len(),
        bytes = joined.len(),
        "loaded instruction files"
    );
    joined
}

/// Backward-compatible entry: the default names only.
pub fn load_system_instructions(project_dir: Option<&Path>) -> String {
    let names: Vec<String> = DEFAULT_FILES.iter().map(|s| s.to_string()).collect();
    load_system_instructions_with(project_dir, &names)
}
