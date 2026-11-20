use buran::config::{ApiKey, Config, PermissionPolicy};
use std::path::PathBuf;

#[test]
fn parses_minimal_project_config_with_provider() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        r#"
[provider]
name = "openrouter"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
model = "anthropic/claude-sonnet-5.5"
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert_eq!(
        cfg.provider.as_ref().unwrap().base_url,
        "https://openrouter.ai/api/v1"
    );
    assert_eq!(
        cfg.provider.as_ref().unwrap().model,
        "anthropic/claude-sonnet-5.5"
    );
    assert_eq!(
        cfg.provider.as_ref().unwrap().api_key,
        ApiKey::FromEnv("OPENROUTER_API_KEY".into())
    );
}

#[test]
fn explicit_api_key_beats_env() {
    // api_key = "sk-..." parses as ApiKey::Explicit and resolve() returns it as-is.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://localhost\"\napi_key = \"sk-test\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert_eq!(
        cfg.provider.as_ref().unwrap().api_key.resolve().unwrap(),
        "sk-test"
    );
}

#[test]
fn parses_mcp_json_format() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".mcp.json"),
        r#"
{
  "mcpServers": {
    "filesystem": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"] },
    "git": { "command": "uvx", "args": ["mcp-server-git"] }
  }
}
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    let names: Vec<&str> = cfg.mcp_servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["filesystem", "git"]);
    let fs = &cfg.mcp_servers[0];
    assert_eq!(fs.command, "npx");
    assert_eq!(
        fs.args,
        vec!["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    );
}

#[test]
fn malformed_mcp_json_is_named_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".mcp.json"), "{ not json").unwrap();
    let err = Config::load(Some(dir.path()), |_| None).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains(".mcp.json"), "error must name the file: {msg}");
}

#[test]
fn missing_everything_loads_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert!(cfg.provider.is_none());
    assert!(cfg.mcp_servers.is_empty());
    assert_eq!(cfg.permission, PermissionPolicy::Ask);
}

#[test]
fn global_config_is_overridden_by_project() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
    std::fs::write(
        home.path().join(".config/buran/config.toml"),
        "[provider]\nbase_url = \"https://global.example.com\"\napi_key = \"sk-global\"\nmodel = \"global-model\"\n\n[permissions]\npolicy = \"allow_all\"\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"https://project.example.com\"\nmodel = \"project-model\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(project.path()), |name| {
        if name == "HOME" {
            Some(home.path().to_string_lossy().into_owned())
        } else {
            None
        }
    })
    .unwrap();
    let p = cfg.provider.as_ref().unwrap();
    // project fields override, unspecified fields fall back to global
    assert_eq!(p.base_url, "https://project.example.com");
    assert_eq!(p.model, "project-model");
    assert_eq!(p.api_key, ApiKey::Explicit("sk-global".into()));
    assert_eq!(cfg.permission, PermissionPolicy::AllowAll);
}

#[test]
fn project_mcp_server_wins_name_collision() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
    std::fs::write(
        home.path().join(".config/buran/config.toml"),
        "[[mcp_servers]]\nname = \"fs\"\ncommand = \"global-fs\"\nargs = []\n\n[[mcp_servers]]\nname = \"git\"\ncommand = \"global-git\"\nargs = []\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join(".mcp.json"),
        r#"{ "mcpServers": { "fs": { "command": "project-fs" } } }"#,
    )
    .unwrap();
    let cfg = Config::load(Some(project.path()), |name| {
        (name == "HOME").then(|| home.path().to_string_lossy().into_owned())
    })
    .unwrap();
    let names: Vec<&str> = cfg.mcp_servers.iter().map(|s| s.name.as_str()).collect();
    // First-seen order kept: global fs position keeps its slot, project replaces in place.
    assert_eq!(names, ["fs", "git"]);
    let fs = cfg.mcp_servers.iter().find(|s| s.name == "fs").unwrap();
    assert_eq!(fs.command, "project-fs");
}

#[test]
fn buran_api_key_env_overrides_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://localhost\"\napi_key = \"sk-config\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let cfg = Config::load_with_env(
        Some(dir.path()),
        |_| None,
        |name: &str| (name == "BURAN_API_KEY").then(|| "sk-env-override".to_string()),
    )
    .unwrap();
    assert_eq!(
        cfg.provider.as_ref().unwrap().api_key,
        ApiKey::FromEnv("BURAN_API_KEY".into())
    );
    // resolve uses the injected env reader, so no process env is touched
    assert_eq!(
        cfg.provider
            .as_ref()
            .unwrap()
            .api_key
            .resolve_with(
                |name: &str| (name == "BURAN_API_KEY").then(|| "sk-env-override".to_string())
            )
            .unwrap(),
        "sk-env-override"
    );
}

#[test]
fn missing_api_key_env_var_is_named_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://localhost\"\napi_key_env = \"DEFINITELY_NOT_SET_XYZ\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    let err = cfg
        .provider
        .as_ref()
        .unwrap()
        .api_key
        .resolve()
        .unwrap_err();
    assert!(
        err.to_string().contains("DEFINITELY_NOT_SET_XYZ"),
        "error must name the env var: {err}"
    );
}

#[test]
fn malformed_toml_names_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("buran.config.toml"), "[provider\nbroken").unwrap();
    let err = Config::load(Some(dir.path()), |_| None).unwrap_err();
    assert!(
        err.to_string().contains("buran.config.toml"),
        "error must name the file: {err}"
    );
}

#[test]
fn skills_dirs_merge_global_then_project() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".config/buran")).unwrap();
    std::fs::write(
        home.path().join(".config/buran/config.toml"),
        "skills_dirs = [\"/global/skills\"]\n",
    )
    .unwrap();
    std::fs::write(
        project.path().join("buran.config.toml"),
        "skills_dirs = [\"/project/skills\"]\n",
    )
    .unwrap();
    let cfg = Config::load(Some(project.path()), |name| {
        (name == "HOME").then(|| home.path().to_string_lossy().into_owned())
    })
    .unwrap();
    assert_eq!(
        cfg.skills_dirs,
        vec![
            PathBuf::from("/global/skills"),
            PathBuf::from("/project/skills")
        ]
    );
}

#[test]
fn compaction_defaults_and_toml_fields() {
    // Defaults: auto ON (real work = not thinking about it), reserve 8192.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://x/v1\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert!(cfg.compaction.auto, "auto defaults ON");
    assert_eq!(cfg.compaction.reserve, 8192);
    assert_eq!(
        cfg.provider.as_ref().unwrap().context_window,
        None,
        "context_window unset = None (128k hard default at use site)"
    );

    // TOML fields override.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        r#"
[provider]
base_url = "http://x/v1"
model = "m"
[compaction]
auto = false
reserve = 4096
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    let comp = cfg.compaction;
    assert!(!comp.auto, "auto = false in toml");
    assert_eq!(comp.reserve, 4096);
}

#[test]
fn provider_context_window_parses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://x/v1\"\nmodel = \"m\"\ncontext_window = 200000\n",
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert_eq!(cfg.provider.as_ref().unwrap().context_window, Some(200_000));
}

#[test]
fn mcp_servers_toml_table_parses() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        r#"
[provider]
base_url = "http://x/v1"
model = "m"
[[mcp_servers]]
name = "fs"
command = "/bin/true"
args = ["a", "b"]
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(cfg.mcp_servers[0].name, "fs");
    assert_eq!(cfg.mcp_servers[0].args, vec!["a", "b"]);
}

#[test]
fn compaction_summarizer_section_parses_and_defaults_absent() {
    // Absent: summarizer = None → summaries use the session provider.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://x/v1\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert!(
        cfg.compaction.summarizer.is_none(),
        "no section, no override"
    );

    // Present: the full inline provider shape.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        r#"
[provider]
base_url = "http://x/v1"
model = "m"
[compaction.summarizer]
base_url = "http://cheap.local/v1"
model = "cheap-model"
api_key_env = "CHEAP_KEY"
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    let s = cfg.compaction.summarizer.expect("section parsed");
    assert_eq!(s.base_url, "http://cheap.local/v1");
    assert_eq!(s.model, "cheap-model");
    assert_eq!(s.api_key_env, "CHEAP_KEY");
}

#[test]
fn subagents_section_parse_and_defaults() {
    // Absent: defaults with enabled=false (opt-in server).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://x/v1\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    let s = &cfg.subagents;
    assert!(!s.enabled, "absent section = disabled (opt-in)");
    assert_eq!(s.max_children, 4);
    assert_eq!(s.timeout_secs, 600);
    assert!(s.provider.is_none(), "no override = inherit");

    // Present: fields parse.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        r#"
[provider]
base_url = "http://x/v1"
model = "m"
[subagents]
enabled = true
max_children = 2
timeout_secs = 120
[subagents.provider]
base_url = "http://cheap.local/v1"
model = "cheap-model"
api_key_env = "CHEAP_KEY"
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    let s = &cfg.subagents;
    assert!(s.enabled);
    assert_eq!(s.max_children, 2);
    assert_eq!(s.timeout_secs, 120);
    let p = s.provider.as_ref().expect("override parsed");
    assert_eq!(p.model, "cheap-model");
}

#[test]
fn filter_for_subagent_drops_the_subagents_entry_only() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        r#"
[provider]
base_url = "http://x/v1"
model = "m"
[[mcp_servers]]
name = "subagents"
command = "buran"
args = ["subagents"]
[[mcp_servers]]
name = "fs"
command = "/bin/true"
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert_eq!(cfg.mcp_servers.len(), 2);

    let filtered = buran::config::filter_for_subagent(cfg);
    assert_eq!(
        filtered
            .mcp_servers
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        vec!["fs"],
        "the subagents entry is dropped (depth ≤ 2, mechanically)"
    );
}

#[test]
fn hooks_section_parse_and_defaults() {
    // Absent: no hooks, standard timeout default.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://x/v1\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert!(cfg.hooks.post_tool_use.is_none());
    assert!(cfg.hooks.on_turn_end.is_none());
    assert_eq!(cfg.hooks.timeout_secs, 10);

    // Present: commands parse.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        r#"
[provider]
base_url = "http://x/v1"
model = "m"
[hooks]
post_tool_use = "rtk hook post-tool"
on_turn_end = "my-bell"
timeout_secs = 3
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert_eq!(
        cfg.hooks.post_tool_use.as_deref(),
        Some("rtk hook post-tool")
    );
    assert_eq!(cfg.hooks.on_turn_end.as_deref(), Some("my-bell"));
    assert_eq!(cfg.hooks.timeout_secs, 3);
}

#[test]
fn agentsmd_files_parse_and_default() {
    // Default: AGENTS.md only.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        "[provider]\nbase_url = \"http://x/v1\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert_eq!(cfg.agentsmd.files, vec!["AGENTS.md"]);

    // Configured: CLAUDE.md added.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("buran.config.toml"),
        r#"
[provider]
base_url = "http://x/v1"
model = "m"
[agentsmd]
files = ["AGENTS.md", "CLAUDE.md"]
"#,
    )
    .unwrap();
    let cfg = Config::load(Some(dir.path()), |_| None).unwrap();
    assert_eq!(cfg.agentsmd.files, vec!["AGENTS.md", "CLAUDE.md"]);
}
