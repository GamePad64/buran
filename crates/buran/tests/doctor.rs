//! Doctor + `mcp list` tests: structured findings against fixture dirs.
//!
//! The doctor logic is a pure function of (config dir, project dir) plus
//! one MCP probe pass; `doctor()` itself is only a renderer, so tests
//! assert on `DoctorReport` values, never on printed text.

use buran::doctor::{DoctorReport, run_doctor};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Holds the tempdir alive for the test's duration; `TempDir` deletes its
/// directory on drop, so returning a bare path would race the doctor.
struct Fixture {
    _keep: tempfile::TempDir,
    path: PathBuf,
}

/// A project dir with no config anywhere: fresh machine, first run.
fn empty_project() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    Fixture { _keep: dir, path }
}

fn write_mcp_json(project: &Path, body: &str) {
    std::fs::write(project.join(".mcp.json"), body).unwrap();
}

#[tokio::test]
async fn report_on_empty_project_has_no_servers_and_act_offer() {
    let project = empty_project();
    let report = run_doctor(None, Some(&project.path), Duration::from_secs(2)).await;

    assert!(
        report.config_errors.is_empty(),
        "{:?}",
        report.config_errors
    );
    assert!(report.servers.is_empty());
    assert!(report.act_offer, "zero configured servers must offer ACT");
}

#[tokio::test]
async fn act_offer_absent_when_mcp_json_has_a_server() {
    let project = empty_project();
    write_mcp_json(
        &project.path,
        r#"{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]}}}"#,
    );

    let report = run_doctor(None, Some(&project.path), Duration::from_secs(2)).await;
    assert!(!report.act_offer);
    assert_eq!(report.servers.len(), 1);
}

#[tokio::test]
async fn act_offer_absent_when_config_lists_a_server() {
    let home = tempfile::tempdir().unwrap();
    let project = empty_project();
    let config_dir = home.path().join(".config/buran");
    std::fs::create_dir_all(&config_dir).unwrap();
    // A provider (so the doctor reports one) and one MCP server.
    std::fs::write(
        config_dir.join("config.toml"),
        "[provider]\nbase_url = \"http://localhost:8000/v1\"\nmodel = \"test-model\"\n\n[[mcp_servers]]\nname = \"tools\"\ncommand = \"cat\"\n",
    )
    .unwrap();

    let report = run_doctor(
        Some(home.path()),
        Some(&project.path),
        Duration::from_secs(2),
    )
    .await;
    assert!(
        !report.act_offer,
        "config-declared server counts: {report:?}"
    );
    assert_eq!(report.servers.len(), 1);
}

#[tokio::test]
async fn broken_server_is_probed_as_unhealthy() {
    let project = empty_project();
    write_mcp_json(
        &project.path,
        r#"{"mcpServers": {"ghost": {"command": "/definitely/not/a/binary"}}}"#,
    );

    let report = run_doctor(None, Some(&project.path), Duration::from_secs(2)).await;
    assert_eq!(report.servers.len(), 1);
    let probe = &report.servers[0];
    assert_eq!(probe.name, "ghost");
    assert!(!probe.healthy, "spawn failure must surface as unhealthy");
    assert!(
        !probe.detail.is_empty(),
        "unhealthy probe must carry the failure reason"
    );
}

#[tokio::test]
async fn malformed_mcp_json_is_a_named_config_error_and_no_act_offer() {
    let project = empty_project();
    // The .mcp.json is broken JSON. The doctor must name the file and not
    // offer ACT (a broken config is not an empty config).
    write_mcp_json(&project.path, "{ not json");

    let report = run_doctor(None, Some(&project.path), Duration::from_secs(2)).await;
    assert_eq!(report.config_errors.len(), 1, "{report:?}");
    assert!(
        report.config_errors[0].contains(".mcp.json"),
        "error must name the file: {:?}",
        report.config_errors
    );
    // The probe list stays empty (no servers parsed), but the offer is
    // suppressed: we cannot claim "no servers configured" when the
    // config is unparseable.
    assert!(!report.act_offer);
}

#[tokio::test]
async fn malformed_config_toml_is_a_named_config_error() {
    let home = tempfile::tempdir().unwrap();
    let project = empty_project();
    let config_dir = home.path().join(".config/buran");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "[provider\nbase_url = ").unwrap();

    let report = run_doctor(
        Some(home.path()),
        Some(&project.path),
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(report.config_errors.len(), 1, "{report:?}");
    assert!(
        report.config_errors[0].contains("config.toml"),
        "error must name the file: {:?}",
        report.config_errors
    );
}

#[tokio::test]
async fn skills_are_counted_and_named() {
    let project = empty_project();
    let skills_dir = project.path.join(".buran/skills");
    std::fs::create_dir_all(skills_dir.join("greet")).unwrap();
    std::fs::write(
        skills_dir.join("greet/SKILL.md"),
        "---\nname: greet\ndescription: Say hello\n---\n\nHELLO!\n",
    )
    .unwrap();

    // Point the doctor at the skills dir via project config. skills_dirs
    // are used verbatim (no project-dir relativization in v1), so the
    // fixture registers the absolute path.
    let skills_dir = skills_dir.canonicalize().unwrap();
    std::fs::write(
        project.path.join("buran.config.toml"),
        format!("skills_dirs = [{}]\n", serde_quote_path(&skills_dir)),
    )
    .unwrap();

    let report = run_doctor(None, Some(&project.path), Duration::from_secs(2)).await;
    assert_eq!(report.skills.len(), 1);
    assert_eq!(report.skills[0], "greet");
}

#[tokio::test]
async fn report_is_renderable() {
    // The renderer must accept a report end-to-end; this pins the
    // `doctor()` -> `render_report` seam without asserting on text.
    let report = DoctorReport::default();
    let text = buran::doctor::render_report(&report);
    assert!(text.contains("provider"), "renderer names every section");
}

// mcp_server/ServerProbe stay in scope as the report-shape helpers this
// file's tests build on (mcp_list wiring assertions live in run.rs tests).

/// TOML string literal for a path.
fn serde_quote_path(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('\\', "\\\\"))
}
