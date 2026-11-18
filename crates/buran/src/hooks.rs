//! Hook runner (ADR 0012): one shell command per harness event, JSON on
//! stdin, stdout back to the caller. Pure plumbing — the logic (rtk,
//! bell, notify-send) lives entirely outside the binary.
//!
//! Fail-open is the contract: a hook that fails to spawn, times out, or
//! is killed returns `None` and the caller passes the original value
//! through. A hook must never break a turn.

use std::time::Duration;

/// Runs `command` via `sh -c` with `event` JSON on stdin; returns the
/// hook's stdout, or `None` when the hook failed, timed out, or was
/// killed. Non-UTF8 output converts lossily (never a panic). The timed-
/// out process is killed AND reaped — no zombies.
pub fn run_hook(command: &str, event: &serde_json::Value, timeout: Duration) -> Option<String> {
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(%e, %command, "hook spawn failed — fail-open");
            return None;
        }
    };

    // Feed the event; a write failure means the hook died early — fail
    // open with the original.
    let event_line = event.to_string();
    {
        let mut stdin = child.stdin.take().expect("hook stdin piped");
        use std::io::Write as _;
        if writeln!(stdin, "{event_line}").is_err() {
            let _ = child.kill();
            let _ = child.wait();
            tracing::warn!(%command, "hook stdin write failed — fail-open");
            return None;
        }
        // Dropping stdin closes the pipe: an `on_turn_end`-style hook
        // that ignores stdin must still see EOF, not hang.
    }

    // Bounded wait: kill + reap on timeout (reaping matters — a killed
    // process that is never waited on stays a zombie).
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = child.stdout.take().expect("hook stdout piped");
                use std::io::Read as _;
                let mut buf = Vec::new();
                // Status is set; the pipe is closed — a blocking read to
                // EOF cannot hang.
                let _ = stdout.read_to_end(&mut buf);
                if !status.success() {
                    tracing::warn!(%command, %status, "hook exited non-zero — fail-open");
                    return None;
                }
                // Trim trailing newline: `echo`/`writeln` shells append it;
                // the JSON contract does not include it.
                return Some(String::from_utf8_lossy(&buf).trim().to_string());
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap: no zombie
                    tracing::warn!(%command, "hook timed out — fail-open");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                tracing::warn!(%e, %command, "hook wait failed — fail-open");
                return None;
            }
        }
    }
}
