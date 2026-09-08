//! End-to-end tests for `rsw run` (foreground mode). No admin required.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RSW: &str = env!("CARGO_BIN_EXE_rsw");
const TEST_CHILD: &str = env!("CARGO_BIN_EXE_test-child");
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

struct Setup {
    dir: PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn setup(config_body: &str) -> Setup {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("rsw-e2e-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    let child_exe = TEST_CHILD.replace('\\', "/");
    let config = format!(
        r#"
[service]
id = "e2e-test"

[process]
executable = "{child_exe}"
{config_body}

[logging]
dir = "logs"
mode = "append"
"#
    );
    std::fs::write(dir.join("app.toml"), config).unwrap();
    Setup { dir }
}

fn spawn_rsw(setup: &Setup, creation: u32) -> std::process::Child {
    Command::new(RSW)
        .arg("run")
        .arg("app.toml")
        .current_dir(&setup.dir)
        .creation_flags(creation)
        // stdin must NOT be inherited: under parallel CREATE_NEW_CONSOLE churn
        // an inherited pipe stdin combined with a fresh console can stall the
        // child's early process init (seen as 1-thread/0-CPU rsw instances).
        // SCM-started services have no stdin either — this matches production.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rsw")
}

use std::os::windows::process::CommandExt as _;

fn dump_logs(setup: &Setup) {
    for name in ["app.wrapper.log", "app.out.log", "app.err.log"] {
        let content = read_log(setup, name);
        if !content.is_empty() {
            eprintln!(
                "--- {name} ---
{content}"
            );
        }
    }
}

fn wait_exit(child: &mut std::process::Child, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.code().unwrap_or(-1)),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

fn read_log(setup: &Setup, name: &str) -> String {
    std::fs::read_to_string(setup.dir.join("logs").join(name)).unwrap_or_default()
}

fn wait_for_log(setup: &Setup, name: &str, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if read_log(setup, name).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn run_captures_output_and_exits_cleanly() {
    let setup = setup("arguments = [\"echo\", \"HELLO-OUT\", \"SECOND-LINE\"]");
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(code, 0);
    let out = read_log(&setup, "app.out.log");
    assert!(out.contains("HELLO-OUT"), "stdout capture missing: {out}");
    assert!(out.contains("SECOND-LINE"), "stdout capture missing: {out}");
    let wrapper = read_log(&setup, "app.wrapper.log");
    assert!(
        wrapper.contains("started child"),
        "wrapper log missing start: {wrapper}"
    );
    assert!(
        wrapper.contains("exited with 0"),
        "wrapper log missing exit: {wrapper}"
    );
}

#[test]
fn stderr_goes_to_its_own_log() {
    let setup = setup("arguments = [\"spam\", \"3\", \"--to-stderr\"]");
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(code, 0);
    assert!(
        read_log(&setup, "app.err.log").contains("SPAM 000"),
        "stderr log empty"
    );
    assert!(
        !read_log(&setup, "app.out.log").contains("SPAM"),
        "stdout log should stay clean"
    );
}

#[test]
fn restart_on_failure_with_max_retries_then_mapped_exit_code() {
    let setup = setup(concat!(
        "arguments = [\"crash\", \"7\"]\n",
        "[process.restart]\n",
        "policy = \"on-failure\"\n",
        "delay_secs = 0\n",
        "max_retries = 2\n"
    ));
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(
        code, 7,
        "child exit code should propagate when retries are exhausted"
    );
    let wrapper = read_log(&setup, "app.wrapper.log");
    assert_eq!(
        wrapper.matches("started child").count(),
        3,
        "initial run + 2 restarts expected: {wrapper}"
    );
    assert!(
        wrapper.contains("giving up after 2"),
        "missing give-up message: {wrapper}"
    );
}

#[test]
fn no_restart_on_success_exit() {
    let setup = setup(concat!(
        "arguments = [\"crash\", \"0\"]\n",
        "[process.restart]\n",
        "policy = \"on-failure\"\n"
    ));
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(code, 0);
    let wrapper = read_log(&setup, "app.wrapper.log");
    assert_eq!(
        wrapper.matches("started child").count(),
        1,
        "no restarts expected: {wrapper}"
    );
}

#[test]
fn graceful_stop_via_ctrl_c() {
    let setup = setup(concat!(
        "arguments = [\"graceful\", \"300\"]\n",
        "stop_signal = \"ctrl-c\"\n",
        "stop_timeout_secs = 5\n"
    ));
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP);
    assert!(
        wait_for_log(
            &setup,
            "app.out.log",
            "GRACEFUL-READY",
            Duration::from_secs(15)
        ),
        "child did not start"
    );

    // The signaler may be terminated by the ctrl event it broadcasts; ignore its exit.
    let _ = Command::new(TEST_CHILD)
        .args(["ctrl-c", &rsw.id().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .expect("run signaler");

    let started = Instant::now();
    let code = wait_exit(&mut rsw, Duration::from_secs(30));
    if code.is_none() {
        dump_logs(&setup);
        panic!("rsw did not exit");
    }
    let code = code.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(code, 0);
    assert!(
        elapsed < Duration::from_secs(5),
        "graceful stop took {elapsed:?}"
    );
    assert!(
        read_log(&setup, "app.out.log").contains("GRACEFUL-DONE"),
        "child skipped graceful cleanup"
    );
    let wrapper = read_log(&setup, "app.wrapper.log");
    assert!(
        wrapper.contains("child exited after signal") || wrapper.contains("already exited"),
        "unexpected stop path: {wrapper}"
    );
}

#[test]
fn stop_kills_the_whole_process_tree() {
    let setup = setup(concat!(
        "arguments = [\"spawn-grandchild\"]\n",
        "stop_timeout_secs = 3\n"
    ));
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP);
    assert!(
        wait_for_log(&setup, "app.out.log", "GRANDCHILD", Duration::from_secs(15)),
        "child did not report grandchild"
    );
    let out = read_log(&setup, "app.out.log");
    let grandchild_pid: u32 = out
        .lines()
        .find_map(|l| l.strip_prefix("GRANDCHILD ").and_then(|p| p.parse().ok()))
        .expect("grandchild pid in log");

    let _ = Command::new(TEST_CHILD)
        .args(["ctrl-c", &rsw.id().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();

    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(code, 0);

    // The grandchild must not survive: the job object sweeps it away.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut alive = true;
    while Instant::now() < deadline {
        let status = Command::new(TEST_CHILD)
            .args(["alive", &grandchild_pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .unwrap();
        alive = status.success();
        if !alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !alive,
        "grandchild {grandchild_pid} survived the service stop"
    );
}

/// Default stop policy (kill): the tree must be gone well under a second
/// after the stop request, with no graceful-signal grace period.
#[test]
fn default_kill_stops_immediately() {
    let setup = setup(concat!(
        "arguments = [\"hang-immune\"]
",
        "stop_timeout_secs = 30
" // would mask a regression if the ladder waited
    ));
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    assert!(
        wait_for_log(&setup, "app.out.log", "STARTED", Duration::from_secs(15)),
        "child did not start"
    );
    let started = Instant::now();
    let _ = Command::new(TEST_CHILD)
        .args(["ctrl-c", &rsw.id().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    let elapsed = started.elapsed();
    assert_eq!(code, 0);
    assert!(
        elapsed < Duration::from_secs(3),
        "kill must be immediate, took {elapsed:?}"
    );
    let wrapper = read_log(&setup, "app.wrapper.log");
    assert!(
        wrapper.contains("terminating child") && wrapper.contains("now"),
        "kill path not taken: {wrapper}"
    );
}

/// Default semantics (no [process.restart] section): a failing child exits
/// the service immediately with the child's exit code — reported to the SCM,
/// no in-wrapper retries (WinSW parity, see the two-layers README section).
#[test]
fn default_policy_reports_child_failure_immediately() {
    let setup = setup("arguments = [\"crash\", \"7\"]");
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(
        code, 7,
        "child exit code must surface as the service exit code"
    );
    let wrapper = read_log(&setup, "app.wrapper.log");
    assert_eq!(
        wrapper.matches("started child").count(),
        1,
        "default policy must not restart: {wrapper}"
    );
}

#[test]
fn validate_prints_resolved_config() {
    let setup = setup("arguments = [\"echo\", \"x\"]");
    let output = Command::new(RSW)
        .arg("validate")
        .arg("app.toml")
        .current_dir(&setup.dir)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("e2e-test"),
        "validate output missing service id: {stdout}"
    );
}

#[test]
fn validate_rejects_broken_config() {
    let dir = std::env::temp_dir().join(format!("rsw-e2e-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("bad.toml"),
        "[service]\nid = \"x\"\n[process]\nexecutable = \"\"\n",
    )
    .unwrap();
    let output = Command::new(RSW)
        .arg("validate")
        .arg("bad.toml")
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("executable"),
        "error should explain the problem: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hooks_run_with_resolved_paths_and_expanded_args() {
    // Regression (shawl-audit): hooks were spawned with the raw config string,
    // so relative paths and %BASE% in hook arguments never worked.
    let setup = setup(concat!(
        "arguments = [\"echo\", \"x\"]\n",
        "[hooks]\n",
        "pre_start = { executable = \"test-child-helper.cmd\", arguments = [\"pre\", \"%BASE%\"], stdout = \"logs/hook.log\", stderr = \"NUL\" }\n",
        "post_stop = { executable = \"test-child-helper.cmd\", arguments = [\"post\", \"%BASE%\"], stdout = \"logs/hook.log\", stderr = \"NUL\" }\n",
    ));
    // A helper batch next to the config, referenced by bare name: resolving it
    // must prefer the config directory.
    std::fs::write(
        setup.dir.join("test-child-helper.cmd"),
        "@echo HOOK-%1-%2\n",
    )
    .unwrap();
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(code, 0);
    let hook_log = read_log(&setup, "hook.log");
    // rsw canonicalizes %BASE% (dunce), so on runners whose %TEMP% is an 8.3
    // short path (RUNNER~1) the test's raw temp_dir won't match — compare
    // against the canonical form.
    let base = dunce::canonicalize(&setup.dir)
        .unwrap_or_else(|_| setup.dir.clone())
        .to_string_lossy()
        .into_owned();
    assert!(
        hook_log.contains(&format!("HOOK-pre-{base}")),
        "pre-start hook (relative path + %BASE% arg) did not run: {hook_log}"
    );
    assert!(
        hook_log.contains(&format!("HOOK-post-{base}")),
        "post-stop hook did not run: {hook_log}"
    );
}

/// The stop-helper path: the child ignores signals ("hang"), so the only way
/// down is the configured stop command + the timeout kill.
#[test]
fn stop_executable_command_runs_before_timeout_kill() {
    let setup = setup(concat!(
        "arguments = [\"hang-immune\"]\n",
        "stop_executable = \"helper.cmd\"\n",
        "stop_timeout_secs = 2\n"
    ));
    // The stop helper just records that it ran.
    std::fs::write(
        setup.dir.join("helper.cmd"),
        "@echo STOPPER-RAN > logs\\stopcmd.log\n",
    )
    .unwrap();
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    assert!(
        wait_for_log(&setup, "app.out.log", "STARTED", Duration::from_secs(15)),
        "child did not start"
    );
    let _ = Command::new(TEST_CHILD)
        .args(["ctrl-c", &rsw.id().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(code, 0);
    let wrapper = read_log(&setup, "app.wrapper.log");
    assert!(
        wrapper.contains("running stop command"),
        "stop command path not taken: {wrapper}"
    );
    assert!(
        read_log(&setup, "stopcmd.log").contains("STOPPER-RAN"),
        "stop command did not execute"
    );
    assert!(
        wrapper.contains("killing the process tree"),
        "timeout kill missing: {wrapper}"
    );
}

/// WinSW compat: `stoparguments` WITHOUT `stopexecutable` means "run the main
/// executable with those arguments".
#[test]
fn stop_arguments_without_stop_executable_use_main_exe() {
    let setup = setup(concat!(
        "arguments = [\"hang-immune\"]\n",
        "stop_arguments = [\"stop-note\"]\n",
        "stop_timeout_secs = 3\n"
    ));
    let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
    assert!(
        wait_for_log(&setup, "app.out.log", "STARTED", Duration::from_secs(15)),
        "child did not start"
    );
    let _ = Command::new(TEST_CHILD)
        .args(["ctrl-c", &rsw.id().to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
    assert_eq!(code, 0);
    // The wrapper log must show the MAIN executable being re-run with the
    // stop arguments, proving the stop-arguments-only path was taken.
    let wrapper = read_log(&setup, "app.wrapper.log");
    assert!(
        wrapper.contains("running stop command") && wrapper.contains("stop-note"),
        "stop_arguments-only path not used with the main executable: {wrapper}"
    );
}

// Silence unused warnings for helper path used conditionally by future tests.
#[allow(dead_code)]
fn _touch(_: &Path) {}

// --- lite build ([[download]] degradation) --------------------------------
// These compile only in the --no-default-features test run (CI), where the
// binary has no download support and the degradation paths are live.
#[cfg(not(feature = "download"))]
mod lite_download {
    use super::*;

    #[test]
    fn lite_validate_warns_but_exits_zero() {
        let setup = setup(concat!(
            "arguments = [\"echo\", \"x\"]\n\n",
            "[[download]]\nfrom = \"https://example.com/app.jar\"\nto = \"app.jar\"\n"
        ));
        let output = Command::new(RSW)
            .arg("validate")
            .arg("app.toml")
            .current_dir(&setup.dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "validate must not fail on a lite build"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no download support"),
            "validate must warn about [[download]]: {stderr}"
        );
    }

    #[test]
    fn lite_run_skips_download_and_starts_child() {
        let setup = setup(concat!(
            "arguments = [\"echo\", \"LITE-OK\"]\n\n",
            "[[download]]\nfrom = \"https://example.com/app.jar\"\nto = \"app.jar\"\n"
        ));
        let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
        let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
        assert_eq!(code, 0);
        let wrapper = read_log(&setup, "app.wrapper.log");
        assert!(
            wrapper.contains("skipping download https://example.com/app.jar"),
            "wrapper log must note the skipped download: {wrapper}"
        );
        let out = read_log(&setup, "app.out.log");
        assert!(out.contains("LITE-OK"), "child must still run: {out}");
    }

    #[test]
    fn lite_run_aborts_when_fail_on_error() {
        let setup = setup(concat!(
            "arguments = [\"echo\", \"MUST-NOT-RUN\"]\n\n",
            "[[download]]\nfrom = \"https://example.com/app.jar\"\nto = \"app.jar\"\nfail_on_error = true\n"
        ));
        let mut rsw = spawn_rsw(&setup, CREATE_NEW_CONSOLE);
        let code = wait_exit(&mut rsw, Duration::from_secs(30)).expect("rsw did not exit");
        assert_ne!(code, 0, "fail_on_error must abort the start");
        let wrapper = read_log(&setup, "app.wrapper.log");
        assert!(
            wrapper.contains("not supported in this lite build"),
            "wrapper log must explain the abort: {wrapper}"
        );
        let out = read_log(&setup, "app.out.log");
        assert!(!out.contains("MUST-NOT-RUN"), "child must not start: {out}");
    }
}
