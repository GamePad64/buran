//! Hook runner (ADR 0012): one shell command per harness event, JSON on
//! stdin, stdout back to the caller. Pure plumbing — the logic (rtk,
//! bell, notify-send) lives entirely outside the binary.
//!
//! Fail-open is the contract: a hook that fails to spawn, times out, or
//! is killed returns `None` and the caller passes the original value
//! through. A hook must never break a turn.
//!
//! Blocking by design: this runner is SYNC and must be called via
//! `tokio::task::block_in_place` (or from a plain thread) — it parks the
//! calling worker for at most `timeout`. All three I/O phases (stdin
//! write, exit wait, stdout drain) share ONE deadline; a hook that
//! ignores stdin or leaks a stdout-holding grandchild cannot hang the
//! caller past it.

use std::sync::Arc;
use std::time::Duration;

/// Runs `command` via `sh -c` with `event` JSON on stdin; returns the
/// hook's trimmed stdout, or `None` when the hook failed, timed out, or
/// was killed. Non-UTF8 output converts lossily (never a panic). The
/// timed-out process is killed (its own process group) AND reaped.
pub fn run_hook(command: &str, event: &serde_json::Value, timeout: Duration) -> Option<String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // Own process group: the timeout kill takes grandchildren (a
    // `sh -c 'x &'` leak) down with the shell, not just `sh`.
    // (Unix-only std API; Windows hooks are rare — the kill covers the
    // immediate child there.)
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let child = cmd.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(%e, %command, "hook spawn failed — fail-open");
            return None;
        }
    };

    let event_line = event.to_string();
    let mut stdin = child.stdin.take().expect("hook stdin piped");

    // Stdin feed on a detached thread under the SAME deadline: a hook
    // that never reads stdin would otherwise block this write past the
    // cap when the event exceeds the pipe buffer (~64 KB).
    let deadline = std::time::Instant::now() + timeout;
    {
        use std::io::Write as _;
        let writer = std::thread::spawn(move || {
            let _ = writeln!(stdin, "{event_line}");
            let _ = stdin.flush();
            // Drop closes the pipe: an stdin-ignoring hook still sees EOF.
        });
        // Never join unconditionally — a wedged writer dies at kill time.
        while !writer.is_finished() {
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // Stdout drain on a detached thread: `read_to_end` on the main path
    // can hang forever if the hook leaked a stdout-holding grandchild
    // (the exit-0-with-background-child shape). The drainer's buffer is
    // what we read; the deadline governs the whole wait.
    let stdout = child.stdout.take().expect("hook stdout piped");
    let drained = Arc::new(std::sync::Mutex::new(Vec::new()));
    let drainer = {
        let drained = drained.clone();
        std::thread::spawn(move || {
            let mut stdout = stdout;
            use std::io::Read as _;
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            *drained.lock().expect("drain lock") = buf;
        })
    };

    let exited_ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap: no zombie
                    // DETACH the drainer: joining here can block for the
                    // grandchild's full lifetime (a leaked `sleep` in the
                    // process group may still hold the stdout pipe for
                    // seconds after `sh` died). The drainer ends at EOF
                    // and its buffer is discarded — we return None anyway.
                    tracing::warn!(%command, "hook timed out — fail-open");
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                tracing::warn!(%e, %command, "hook wait failed — fail-open");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };

    // Give the drainer a bounded grace to finish (pipe may still hold
    // buffered bytes after exit).
    while !drainer.is_finished() {
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let buf = drained.lock().expect("drain lock").clone();

    if !exited_ok {
        tracing::warn!(%command, "hook exited non-zero — fail-open");
        return None;
    }
    // Trim trailing newline: `echo`/`writeln` shells append it; the JSON
    // contract does not include it.
    Some(String::from_utf8_lossy(&buf).trim().to_string())
}
