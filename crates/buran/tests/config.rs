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
    assert!(cfg.bundled_fs_server); // default true: graceful degradation per spec
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
