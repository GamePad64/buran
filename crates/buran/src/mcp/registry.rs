//! Tool catalog: every healthy server's tools, keyed by qualified name.

use std::collections::BTreeMap;

use rmcp::model::Tool;

/// One tool offered by one server.
#[derive(Debug, Clone)]
pub struct ToolEntry {
    pub server: String,
    pub tool: Tool,
}

impl ToolEntry {
    /// Qualified name as the model sees it: `server-name/tool-name`.
    pub fn qualified_name(&self) -> String {
        format!("{}/{}", self.server, self.tool.name)
    }
}

/// All tools across healthy servers. Qualified names are unique: a server
/// name appears once in the config (first wins on collisions), and rmcp
/// servers list each tool name once.
pub struct ToolCatalog {
    by_qualified_name: BTreeMap<String, ToolEntry>,
}

impl ToolCatalog {
    pub fn new(entries: Vec<ToolEntry>) -> Self {
        let by_qualified_name = entries
            .into_iter()
            .map(|entry| (entry.qualified_name(), entry))
            .collect();
        Self { by_qualified_name }
    }

    pub fn lookup(&self, qualified: &str) -> Option<&ToolEntry> {
        self.by_qualified_name.get(qualified)
    }

    pub fn names(&self) -> Vec<String> {
        self.by_qualified_name.keys().cloned().collect()
    }

    pub fn entries(&self) -> impl Iterator<Item = &ToolEntry> {
        self.by_qualified_name.values()
    }

    pub fn is_empty(&self) -> bool {
        self.by_qualified_name.is_empty()
    }
}

impl std::fmt::Debug for ToolCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCatalog")
            .field("tools", &self.names())
            .finish()
    }
}
