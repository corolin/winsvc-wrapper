//! Child process supervision: spawn → monitor → restart with backoff →
//! graceful stop. Shared by the SCM service entry and foreground `rsw run`.

use std::io::{BufRead as _, Read};
use std::os::windows::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::config::{Resolved, RestartPolicy};
use crate::download::run_downloads;
use crate::hooks::run_hook;
use crate::logging::{Channel, LogSink};
use crate::mapping::map_drives;
use crate::process_win::{self, CONSOLE_STOP, ProcessJob};
use crate::stop::{StopKind, stop_child};

/// Seconds of uptime after which the backoff/retry counters reset.
const STABLE_AFTER_SECS: u64 = 60;
/// Backoff ceiling.
const MAX_DELAY_SECS: u64 = 60;
const POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlEvent {
    Stop,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusUpdate {
    Running,
    StopPending { elapsed_ms: u32, wait_hint_ms: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceExit {
    /// A deliberate stop (SCM STOP/SHUTDOWN, or Ctrl+C in `rsw run`) finished.
    Stopped,
    /// The child exited and the restart policy said "give up".
    ChildExited(u32),
    /// A deliberate stop arrived while waiting out the restart delay.
    StoppedDuringDelay(u32),
    /// The child could not be spawned (or the pre-start hook failed).
    StartFailed(Option<u32>),
}

pub fn supervise(
    r: &Resolved,
    sink: &Arc<LogSink>,
    control_rx: &Receiver<ControlEvent>,
    status: &dyn Fn(StatusUpdate),
    interactive: bool,
) -> ServiceExit {
    let proc = &r.cfg.process;
    let working_dir = r.working_dir().to_path_buf();

    if let Some(code) = run_hook(
        r.cfg.hooks.pre_start.as_ref(),
        "pre-start",
        &r.base_dir,
        &working_dir,
        sink,
    ) && code != 0
    {
        sink.error("pre-start hook failed; not starting the child");
        return ServiceExit::StartFailed(None);
    }

    if let Err(e) = run_downloads(&r.cfg.download, &working_dir, sink) {
        sink.error(&format!("{e:#}"));
        return ServiceExit::StartFailed(None);
    }
    map_drives(&r.cfg.map_drive, sink);

    let job = match ProcessJob::create_kill_on_close() {
        Ok(job) => job,
        Err(e) => {
            sink.error(&format!("could not create job object: {e}"));
            return ServiceExit::StartFailed(Some(e.code().0 as u32));
        }
    };

    let restart = &proc.restart;
    let mut consecutive_restarts: u32 = 0;

    loop {
        let mut child = match spawn_child(r, &job, sink) {
            Ok(child) => child,
            Err(e) => {
                let code = e.raw_os_error();
                sink.error(&format!("failed to spawn {}: {e}", proc.executable));
                run_hook(
                    r.cfg.hooks.post_stop.as_ref(),
                    "post-stop",
                    &r.base_dir,
                    &working_dir,
                    sink,
                );
                return ServiceExit::StartFailed(code.map(|c| c as u32));
            }
        };
        let pid = child.id();
        sink.info(&format!(
            "started child {pid}: {} {:?}",
            proc.executable,
            r.start_args()
        ));
        pump_output(&mut child, Arc::clone(sink));
        status(StatusUpdate::Running);
        let started = Instant::now();
        run_hook(
            r.cfg.hooks.post_start.as_ref(),
            "post-start",
            &r.base_dir,
            &working_dir,
            sink,
        );

        // -- wait for exit or stop request ---------------------------------
        // Stop requests are checked FIRST: a child that exits *in response*
        // to the stop signal (fast interpreters like node are gone within one
        // poll interval) must be recorded as a deliberate stop, not as a
        // spontaneous exit that would trigger the restart policy — the JVM,
        // for instance, exits 130 on a console ctrl event.
        //
        // CONSOLE_STOP (a ctrl event raised on our own console) counts as a
        // stop request only in interactive `rsw run` mode, mirroring shawl's
        // proven default: in service mode a child raising ctrl-c on itself
        // must not take the whole service down.
        let exit = loop {
            let channel_stop = match control_rx.try_recv() {
                Ok(ControlEvent::Stop | ControlEvent::Shutdown) => true,
                Err(mpsc::TryRecvError::Disconnected) => true, // caller dropped
                _ => false,
            };
            let console_stop = CONSOLE_STOP.swap(false, std::sync::atomic::Ordering::SeqCst);
            if channel_stop || (interactive && console_stop) {
                break i32::MIN; // sentinel: stop requested
            }
            if let Ok(Some(status)) = child.try_wait() {
                // A code-less termination (e.g. job-tree kill) maps to
                // ERROR_PROCESS_ABORTED like shawl, never to "success".
                break status.code().unwrap_or(1067);
            }
            std::thread::sleep(POLL);
        };

        if exit != i32::MIN {
            let code = exit as u32;
            let uptime = started.elapsed().as_secs();
            sink.info(&format!("child {pid} exited with {code} after {uptime}s"));

            if uptime >= STABLE_AFTER_SECS {
                consecutive_restarts = 0;
            }

            let again = should_restart(&restart.policy, &proc.success_exit_codes, code, restart);
            if !again {
                run_hook(
                    r.cfg.hooks.post_stop.as_ref(),
                    "post-stop",
                    &r.base_dir,
                    &working_dir,
                    sink,
                );
                return ServiceExit::ChildExited(code);
            }
            if restart.max_retries > 0 && consecutive_restarts >= restart.max_retries {
                sink.warn(&format!(
                    "giving up after {consecutive_restarts} consecutive restarts (max_retries); handing over to SCM failure actions"
                ));
                run_hook(
                    r.cfg.hooks.post_stop.as_ref(),
                    "post-stop",
                    &r.base_dir,
                    &working_dir,
                    sink,
                );
                return ServiceExit::ChildExited(code);
            }
            consecutive_restarts += 1;
            let delay = if restart.backoff {
                restart
                    .delay_secs
                    .saturating_mul(1u64 << (consecutive_restarts - 1).min(6))
                    .min(MAX_DELAY_SECS)
            } else {
                restart.delay_secs
            };
            sink.info(&format!(
                "restarting child in {delay}s (restart #{consecutive_restarts}{})",
                if restart.backoff {
                    ", exponential backoff"
                } else {
                    ""
                }
            ));
            if sleep_cancellable(delay, control_rx, interactive) {
                sink.info("stop requested during restart delay");
                run_hook(
                    r.cfg.hooks.post_stop.as_ref(),
                    "post-stop",
                    &r.base_dir,
                    &working_dir,
                    sink,
                );
                return ServiceExit::StoppedDuringDelay(code);
            }
            continue; // respawn
        }

        // -- deliberate stop ------------------------------------------------
        // The hint must cover the pre-stop hook AND the stop ladder itself.
        let pre_stop_budget = r
            .cfg
            .hooks
            .pre_stop
            .as_ref()
            .map(|h| h.timeout_secs.max(1))
            .unwrap_or(0);
        let hint = (proc.stop_timeout_secs.max(1) + pre_stop_budget + 2) * 1000;
        status(StatusUpdate::StopPending {
            elapsed_ms: 0,
            wait_hint_ms: hint as u32,
        });
        sink.info("stop requested");
        run_hook(
            r.cfg.hooks.pre_stop.as_ref(),
            "pre-stop",
            &r.base_dir,
            &working_dir,
            sink,
        );
        let result = stop_child(
            &mut child,
            proc,
            &r.base_dir,
            &working_dir,
            sink,
            &job,
            |elapsed, total| {
                status(StatusUpdate::StopPending {
                    elapsed_ms: elapsed.as_millis() as u32,
                    wait_hint_ms: total.as_millis() as u32,
                });
            },
        );
        let note = match result.kind {
            StopKind::AlreadyExited => "child had already exited",
            StopKind::StopCommand => "stopped via stop command",
            StopKind::AfterSignal => "child exited after signal",
            StopKind::Killed => "child was force-killed after timeout",
        };
        sink.info(&format!("{note} (code {:?})", result.code));
        run_hook(
            r.cfg.hooks.post_stop.as_ref(),
            "post-stop",
            &r.base_dir,
            &working_dir,
            sink,
        );
        return ServiceExit::Stopped;
    }
}

fn spawn_child(r: &Resolved, job: &ProcessJob, sink: &LogSink) -> std::io::Result<Child> {
    let p = &r.cfg.process;
    let program = crate::config::resolve_program(&p.executable, &r.base_dir);
    let mut cmd = Command::new(program);
    cmd.args(r.start_args())
        .current_dir(r.working_dir())
        .envs(r.child_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(process_win::creation_flags(
            p.priority,
            p.stop_signal,
            p.hide_window,
        ));
    let child = cmd.spawn()?;
    if let Err(e) = job.assign(&child) {
        sink.warn(&format!(
            "could not add child to job object: {e} (grandchildren may survive a crash)"
        ));
    }
    Ok(child)
}

fn should_restart(
    policy: &RestartPolicy,
    success_codes: &[u32],
    code: u32,
    restart: &crate::config::RestartConfig,
) -> bool {
    match policy {
        RestartPolicy::Never => false,
        RestartPolicy::Always => true,
        RestartPolicy::OnFailure => !success_codes.contains(&code),
        RestartPolicy::Custom => {
            if !restart.restart_if.is_empty() {
                restart.restart_if.contains(&code)
            } else if !restart.restart_if_not.is_empty() {
                !restart.restart_if_not.contains(&code)
            } else {
                false
            }
        }
    }
}

/// Sleeps in cancellable slices; returns true when a stop was requested.
/// Console ctrl events count only in interactive (`rsw run`) mode.
fn sleep_cancellable(secs: u64, control_rx: &Receiver<ControlEvent>, interactive: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        let channel_stop = match control_rx.try_recv() {
            Ok(ControlEvent::Stop | ControlEvent::Shutdown) => true,
            Err(mpsc::TryRecvError::Disconnected) => true, // caller dropped
            _ => false,
        };
        let console_stop = CONSOLE_STOP.swap(false, std::sync::atomic::Ordering::SeqCst);
        if channel_stop || (interactive && console_stop) {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// Wires the child's stdout/stderr pipes into the log sink.
fn pump_output(child: &mut Child, sink: Arc<LogSink>) {
    if let Some(stdout) = child.stdout.take() {
        let sink_out = Arc::clone(&sink);
        std::thread::spawn(move || pump_stream(stdout, sink_out, Channel::Out));
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || pump_stream(stderr, sink, Channel::Err));
    }
}

fn pump_stream<R: Read>(reader: R, sink: Arc<LogSink>, channel: Channel) {
    let mut reader = std::io::BufReader::new(reader);
    let mut buf = Vec::with_capacity(512);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                    if buf.last() == Some(&b'\r') {
                        buf.pop();
                    }
                }
                let line = String::from_utf8_lossy(&buf);
                sink.child_line(channel, &line);
            }
            Err(_) => break,
        }
    }
}
