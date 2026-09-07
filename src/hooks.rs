//! Pre/post lifecycle hooks (WinSW `prestart`/`poststart`/`prestop`/`poststop`).

use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::Hook;
use crate::logging::LogSink;

/// Runs a hook to completion (bounded by its `timeout_secs`).
/// Returns `None` when the hook is not configured, `Some(exit_code)` when it
/// ran. A failure to launch produces `Some(code)` with a logged error.
pub fn run_hook(
    hook: Option<&Hook>,
    phase: &str,
    base_dir: &Path,
    working_dir: &Path,
    sink: &LogSink,
) -> Option<u32> {
    let hook = hook?;
    if hook.executable.trim().is_empty() {
        return None;
    }
    let executable = crate::config::resolve_program(&hook.executable, base_dir);
    sink.info(&format!("{phase} hook: {executable} {:?}", hook.arguments));

    let open = |path: &str| -> Stdio {
        if path.eq_ignore_ascii_case("NUL") {
            Stdio::null()
        } else {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map(Stdio::from)
                .unwrap_or_else(|e| {
                    sink.warn(&format!(
                        "{phase} hook: cannot open {path} ({e}); discarding output"
                    ));
                    Stdio::null()
                })
        }
    };

    let child = Command::new(&executable)
        .args(&hook.arguments)
        .current_dir(working_dir)
        .stdout(open(&hook.stdout))
        .stderr(open(&hook.stderr))
        .stdin(Stdio::null())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            sink.error(&format!("{phase} hook failed to launch: {e}"));
            return Some(0xFFFF_FFFF);
        }
    };

    // Put the hook under a kill-on-close job so grandchildren it spawns are
    // swept on timeout (or on normal exit) instead of leaking.
    let job = crate::process_win::ProcessJob::create_kill_on_close().ok();
    if let Some(job) = &job
        && let Err(e) = job.assign(&child)
    {
        sink.warn(&format!("{phase} hook: job-object assignment failed: {e}"));
    }

    let deadline = Instant::now() + Duration::from_secs(hook.timeout_secs.max(1));
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                if code == 0 {
                    sink.info(&format!("{phase} hook exited with 0"));
                } else {
                    sink.warn(&format!("{phase} hook exited with {code}"));
                }
                return Some(code as u32);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                sink.warn(&format!(
                    "{phase} hook exceeded {}s timeout; killing it and its children",
                    hook.timeout_secs
                ));
                if let Some(job) = &job {
                    job.terminate_tree();
                }
                let _ = child.kill();
                let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                return Some(code as u32);
            }
        }
    }
}
