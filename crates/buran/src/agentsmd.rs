//! AGENTS.md loader for system instructions.
//!
//! Gateway-style layering (ADR 0003): every ancestor directory from the repo
//! root down to the project directory may carry an `AGENTS.md`; all found
//! layers are concatenated root-first so the nearest (most specific) file
//! speaks last. A `CLAUDE.md` file is never read — only an actual
//! `AGENTS.md` filename counts (Claude Code's symlink to it resolves through
//! the filesystem, so the real file is what we see).

use std::path::Path;

/// Collects AGENTS.md instructions from `project_dir` and its ancestors.
/// Empty string when nothing is found. Unreadable layers are skipped with a
/// debug log — a broken instruction file must never crash the server.
pub fn load_system_instructions(project_dir: Option<&Path>) -> String {
    let Some(project_dir) = project_dir else {
        return String::new();
    };

    // Ancestors run leaf → root; reverse so root speaks first.
    let mut layers: Vec<(std::path::PathBuf, String)> = project_dir
        .ancestors()
        .map(|dir| dir.join("AGENTS.md"))
        .filter(|path| path.is_file())
        .filter_map(|path| {
            std::fs::read_to_string(&path)
                .map_err(|source| {
                    tracing::debug!(?path, %source, "skipping unreadable AGENTS.md");
                    source
                })
                .ok()
                .map(|text| (path, text))
        })
        .collect();
    layers.reverse();

    let joined = layers
        .into_iter()
        .map(|(_, text)| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    tracing::debug!(bytes = joined.len(), "loaded AGENTS.md instructions");
    joined
}
