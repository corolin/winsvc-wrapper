//! The graceful stop ladder: stop helper command → console ctrl event →
//! close-window → timeout → TerminateProcess → job-object tree kill.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::ProcessConfig;
use crate::logging::LogSink;
use crate::process_win::{self, ProcessJob};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// The child was already gone.
    AlreadyExited,
    /// The configured stop helper command completed and the child exited.
    StopCommand,
    /// The child exited after receiving the ctrl event / window close.
    AfterSignal,
    /// The child had to be terminated after the timeout.
    Killed,
}

#[derive(Debug, Clone, Copy)]
pub struct StopResult {
    pub code: Option<u32>,
    pub kind: StopKind,
}

/// Runs the full ladder against `child`.
///
/// `on_progress` is called about once a second with `(elapsed, total)` so the
/// SCM layer can feed STOP_PENDING checkpoints.
///
/// WinSW semantics: `stoparguments` without `stopexecutable` means "run the
/// main executable with those arguments" — rsw keeps that compatibility.
pub fn stop_child(
    child: &mut Child,
    proc: &ProcessConfig,
    base_dir: &Path,
    working_dir: &Path,
    sink: &LogSink,
    job: &ProcessJob,
    mut on_progress: impl FnMut(Duration, Duration),
) -> StopResult {
    let timeout = Duration::from_secs(proc.stop_timeout_secs.max(1));
    let deadline = Instant::now() + timeout;
    let pid = child.id();

    if let Ok(Some(status)) = child.try_wait() {
        return StopResult {
            code: status.code().map(|c| c as u32),
            kind: StopKind::AlreadyExited,
        };
    }

    let has_stop_args = !proc.stop_arguments.is_empty();
    let has_stop_exe = proc
        .stop_executable
        .as_deref()
        .is_some_and(|s| !s.is_empty());

    // 1) stop_signal = "kill": skip graceful signals AND the timeout —
    //    stop means TerminateProcess + tree kill, immediately.
    if proc.stop_signal == crate::config::StopSignal::Kill && !has_stop_exe && !has_stop_args {
        sink.info(&format!(
            "stop_signal = kill: terminating child {pid} and its tree now"
        ));
        job.terminate_tree();
        let _ = child.kill();
        let code = child
            .wait()
            .ok()
            .and_then(|s| s.code())
            .map(|c| c as u32)
            .or(Some(1));
        return StopResult {
            code,
            kind: StopKind::Killed,
        };
    }

    // 2) A configured stop helper command replaces the signal-based path.
    if has_stop_exe || has_stop_args {
        let raw = if has_stop_exe {
            proc.stop_executable.as_deref().unwrap()
        } else {
            &proc.executable
        };
        let stop_exe = crate::config::resolve_program(raw, base_dir);
        sink.info(&format!(
            "running stop command: {stop_exe} {:?}",
            proc.stop_arguments
        ));
        run_stop_command(
            &stop_exe,
            proc,
            working_dir,
            sink,
            deadline,
            &mut on_progress,
        );
        if let Some(result) = wait_for_exit(child, deadline, &mut on_progress) {
            return StopResult {
                code: Some(result),
                kind: StopKind::StopCommand,
            };
        }
    } else {
        // 3) Signal (graceful policies only).
        match proc.stop_signal {
            crate::config::StopSignal::Kill => {}
            signal => match process_win::send_ctrl_event(signal, pid) {
                Ok(()) => sink.info(&format!("sent {:?} to child {pid}", signal)),
                Err(e) => {
                    // 4) No console to signal — try closing its windows instead.
                    let closed = process_win::close_main_windows_of(pid);
                    sink.warn(&format!(
                        "could not deliver ctrl event to child {pid} ({e}); closed {closed} window(s)"
                    ));
                }
            },
        }
        if let Some(result) = wait_for_exit(child, deadline, &mut on_progress) {
            return StopResult {
                code: Some(result),
                kind: StopKind::AfterSignal,
            };
        }
    }

    // 5) Timeout — sweep the whole tree NOW (grandchildren must not outlive
    //    the kill moment, e.g. while the post-stop hook runs), then reap the
    //    direct child.
    sink.warn(&format!(
        "child {pid} did not exit within {}s; killing the process tree",
        proc.stop_timeout_secs
    ));
    job.terminate_tree();
    let _ = child.kill();
    let code = child
        .wait()
        .ok()
        .and_then(|s| s.code())
        .map(|c| c as u32)
        .or(Some(1));
    StopResult {
        code,
        kind: StopKind::Killed,
    }
}

/// Runs the stop helper bounded by the remaining stop budget. WinSW waits
/// indefinitely here; rsw treats `stop_timeout_secs` as the budget for the
/// whole ladder so a hanging stop command cannot wedge the SCM stop forever.
fn run_stop_command(
    stop_exe: &str,
    proc: &ProcessConfig,
    working_dir: &Path,
    sink: &LogSink,
    deadline: Instant,
    on_progress: &mut dyn FnMut(Duration, Duration),
) {
    let spawn = Command::new(stop_exe)
        .args(&proc.stop_arguments)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut stopper = match spawn {
        Ok(c) => c,
        Err(e) => {
            sink.error(&format!("stop command failed to launch: {e}"));
            return;
        }
    };
    let total = deadline.saturating_duration_since(Instant::now());
    let mut last_tick = Instant::now();
    loop {
        match stopper.try_wait() {
            Ok(Some(status)) => {
                sink.info(&format!(
                    "stop command exited with {}",
                    status.code().unwrap_or(-1)
                ));
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                if last_tick.elapsed() >= Duration::from_secs(1) {
                    last_tick = Instant::now();
                    on_progress(total.saturating_sub(deadline - Instant::now()), total);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                sink.warn(&format!(
                    "stop command exceeded its {}s budget; killing it",
                    total.as_secs()
                ));
                let _ = stopper.kill();
                let _ = stopper.wait();
                return;
            }
        }
    }
}

fn wait_for_exit(
    child: &mut Child,
    deadline: Instant,
    on_progress: &mut impl FnMut(Duration, Duration),
) -> Option<u32> {
    let total = deadline - Instant::now();
    let mut last_tick = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status.code().unwrap_or(0) as u32);
        }
        if Instant::now() >= deadline {
            return None;
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = Instant::now();
            on_progress(total.saturating_sub(deadline - Instant::now()), total);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
