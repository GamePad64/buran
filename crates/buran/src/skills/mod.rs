//! Skills: `SKILL.md` files with `name`/`description` frontmatter.
//!
//! Descriptions are injected into the system prompt; bodies load on demand
//! (ADR 0004). Scanning covers `<dir>/<skill>/SKILL.md` and one nested group
//! level `<dir>/<group>/<skill>/SKILL.md`. First directory wins on `name`
//! collisions (global dirs precede project dirs per config merge order).

mod loader;
mod model;

pub use loader::{load_body, scan};
pub use model::{SkillBody, SkillDescription, SkillsError};
