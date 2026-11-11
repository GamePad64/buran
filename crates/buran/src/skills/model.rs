//! Skill data types.

use std::path::PathBuf;

/// One discovered skill: enough to render into the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDescription {
    pub name: String,
    pub description: String,
    /// The `SKILL.md` file this skill was parsed from.
    pub source_path: PathBuf,
}

/// The markdown body of a skill, frontmatter stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillBody {
    pub body_markdown: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    #[error("failed reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} has no frontmatter block (--- delimiters); it is not a skill file")]
    NoFrontmatter { path: PathBuf },
}
