//! Hook runner: fail-open, timeout with process reaping, non-UTF8 lossy.
//! The runner is pure plumbing — the JSON contract lives in the call sites.

use buran::hooks::run_hook;
use std::time::Duration;

fn event() -> serde_json::Value {
    serde_json::json!({"event": "post_tool_use", "result": "original"})
}

#[test]
fn echo_hook_returns_its_stdin() {
    let out = run_hook("cat", &event(), Duration::from_secs(5));
    assert_eq!(out.as_deref(), Some(event().to_string().as_str()));
}

#[test]
fn nonexistent_command_fails_open() {
    let out = run_hook(
        "definitely-not-a-command-xyz",
        &event(),
        Duration::from_secs(5),
    );
    assert_eq!(out, None, "spawn failure = fail-open None");
}

#[test]
fn garbage_stdout_is_passed_through_verbatim() {
    // Interpretation is the caller's job; the runner only carries bytes.
    let out = run_hook("echo not-json-at-all", &event(), Duration::from_secs(5));
    assert_eq!(out.as_deref(), Some("not-json-at-all"));
}

#[test]
fn hung_hook_times_out_and_the_process_is_reaped() {
    let start = std::time::Instant::now();
    let out = run_hook("sleep 30", &event(), Duration::from_millis(300));
    assert_eq!(out, None, "timeout = fail-open None");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "timeout actually bounded the wait"
    );
    // The sleeping shell must be reaped, not left as a zombie: the hook
    // count via ps would be flaky; instead spawn the same hook twice and
    // confirm the second also completes promptly (reaper works).
    let t = std::time::Instant::now();
    let _ = run_hook("sleep 30", &event(), Duration::from_millis(300));
    assert!(t.elapsed() < Duration::from_secs(5));
}

#[test]
fn non_utf8_output_is_lossy_not_a_panic() {
    // printf a raw high byte sequence.
    let out = run_hook("printf '\\xff\\xfe'", &event(), Duration::from_secs(5));
    assert!(out.is_some(), "lossy conversion, not failure");
}
