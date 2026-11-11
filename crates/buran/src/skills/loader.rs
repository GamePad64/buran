//! Skill directory scanning and body loading.

use std::fs;
use std::path::{Path, PathBuf};

use super::model::{SkillBody, SkillDescription, SkillsError};

/// Scans skill directories in order. Each directory may hold skills flat
/// (`dir/my-skill/SKILL.md`) or one group level deep (`dir/group/my-skill/
/// SKILL.md`). A skill missing `name` or `description` frontmatter is skipped
/// with a warning — one broken file must not hide the others. First
/// directory wins on `name` collisions.
pub fn scan(dirs: &[PathBuf]) -> Vec<SkillDescription> {
    let mut found: Vec<SkillDescription> = Vec::new();
    for dir in dirs {
        for candidate in skill_files(dir) {
            let Some(skill) = parse_frontmatter(&candidate) else {
                continue;
            };
            if found.iter().any(|s| s.name == skill.name) {
                tracing::debug!(name = %skill.name, "duplicate skill name; first wins");
                continue;
            }
            found.push(skill);
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// `SKILL.md` files at `<dir>/*/SKILL.md` and `<dir>/*/*/SKILL.md`.
fn skill_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }
        let direct = skill_dir.join("SKILL.md");
        if direct.is_file() {
            files.push(direct);
        }
        // One group level deep.
        if let Ok(groups) = fs::read_dir(&skill_dir) {
            for group in groups.flatten() {
                let nested = group.path().join("SKILL.md");
                if nested.is_file() {
                    files.push(nested);
                }
            }
        }
    }
    files
}

/// Parses `name`/`description` from the frontmatter block, if valid.
/// Returns `None` (with a warning) when frontmatter or required keys are
/// missing. Values are single-line strings — YAML lists/maps are not
/// supported (recorded limitation, ADR 0004).
fn parse_frontmatter(path: &Path) -> Option<SkillDescription> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) => {
            tracing::warn!(?path, %source, "unreadable SKILL.md skipped");
            return None;
        }
    };
    let Some(frontmatter) = frontmatter_block(&text) else {
        tracing::warn!(?path, "SKILL.md without frontmatter skipped");
        return None;
    };
    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(value.trim().trim_matches('"').to_string());
        } else if let Some(value) = line.strip_prefix("description:") {
            description = Some(value.trim().trim_matches('"').to_string());
        }
    }
    match (
        name.filter(|n| !n.is_empty()),
        description.filter(|d| !d.is_empty()),
    ) {
        (Some(name), Some(description)) => Some(SkillDescription {
            name,
            description,
            source_path: path.to_path_buf(),
        }),
        _ => {
            tracing::warn!(?path, "SKILL.md missing name or description skipped");
            None
        }
    }
}

/// Content between the opening `---` and the closing `---` line.
fn frontmatter_block(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Loads the markdown body of a skill: full file, frontmatter stripped.
pub fn load_body(desc: &SkillDescription) -> Result<SkillBody, SkillsError> {
    let text = fs::read_to_string(&desc.source_path).map_err(|source| SkillsError::Io {
        path: desc.source_path.clone(),
        source,
    })?;
    let body = match strip_frontmatter(&text) {
        Some(body) => body.to_string(),
        None => {
            return Err(SkillsError::NoFrontmatter {
                path: desc.source_path.clone(),
            });
        }
    };
    Ok(SkillBody {
        body_markdown: body.trim().to_string(),
    })
}

fn strip_frontmatter(text: &str) -> Option<&str> {
    if !text.starts_with("---") {
        return None;
    }
    let after_open = &text[3..];
    let end = after_open.find("\n---")?;
    let body = &after_open[end + 4..];
    Some(body.strip_prefix('\n').unwrap_or(body))
}
