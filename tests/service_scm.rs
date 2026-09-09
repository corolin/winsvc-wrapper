//! Real-machine SCM regression (admin required). Ignored by default:
//!   cargo test --test service_scm -- --ignored
//! Complements scripts\test-service.ps1 and additionally verifies that
//! refreshing a config whose [[on_failure]] was removed CLEARS the SCM
//! failure actions instead of leaving them behind.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const RSW: &str = env!("CARGO_BIN_EXE_rsw");
const TEST_CHILD: &str = env!("CARGO_BIN_EXE_test-child");
const SERVICE: &str = "rsw-scm-rust-test";

fn rsw(args: &[&str], dir: &std::path::Path) -> std::process::Output {
    Command::new(RSW)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run rsw")
}

use std::process::Command;

fn open_named_service(
    name: &str,
    access: ServiceAccess,
) -> Option<windows_service::service::Service> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    manager.open_service(name, access).ok()
}

fn open_service(access: ServiceAccess) -> Option<windows_service::service::Service> {
    open_named_service(SERVICE, access)
}

fn wait_named_state(name: &str, want: ServiceState, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(s) = open_named_service(name, ServiceAccess::QUERY_STATUS)
            && let Ok(st) = s.query_status()
            && st.current_state == want
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

fn wait_state(want: ServiceState, timeout: Duration) -> bool {
    wait_named_state(SERVICE, want, timeout)
}

fn failure_action_count() -> Option<usize> {
    let s = open_service(ServiceAccess::QUERY_CONFIG)?;
    let actions = s.get_failure_actions().ok()?.actions;
    Some(actions.map(|a| a.len()).unwrap_or(0))
}

fn write_config(dir: &std::path::Path, with_on_failure: bool) -> PathBuf {
    let child = TEST_CHILD.replace('\\', "/");
    let on_failure = if with_on_failure {
        "\n[[on_failure]]\naction = \"restart\"\ndelay = \"2 sec\"\n"
    } else {
        ""
    };
    let cfg = dir.join("scm.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[service]
id = "{SERVICE}"
start_type = "manual"

[process]
executable = "{child}"
arguments = ["graceful", "200"]
stop_signal = "ctrl-c"
stop_timeout_secs = 10
{on_failure}
[logging]
dir = "logs"
mode = "append"
"#
        ),
    )
    .unwrap();
    cfg
}

#[test]
#[ignore = "requires an elevated prompt; run with --ignored"]
fn scm_roundtrip_and_failure_action_clearing() {
    // Admin gate: creating a service without elevation is access-denied.
    if ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).is_err() {
        eprintln!("skipped: not elevated (run from an administrator prompt)");
        return;
    }

    let dir = std::env::temp_dir().join(format!("rsw-scm-rust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("logs")).unwrap();

    let result = check(&dir);

    // cleanup even on failure
    if let Some(s) = open_service(ServiceAccess::ALL_ACCESS) {
        let _ = s.stop();
        let _ = s.delete();
    }
    let _ = std::fs::remove_dir_all(&dir);
    result.unwrap();
}

#[allow(dead_code)]
fn check(dir: &std::path::Path) -> anyhow::Result<()> {
    // install (with failure actions)
    let cfg = write_config(dir, true);
    let out = rsw(&["install", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        failure_action_count(),
        Some(1),
        "restart action should be persisted"
    );

    // start / running / stop / stopped
    let out = rsw(&["start", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_state(ServiceState::Running, Duration::from_secs(20)),
        "service did not reach Running"
    );

    let out = rsw(&["stop", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_state(ServiceState::Stopped, Duration::from_secs(30)),
        "service did not reach Stopped"
    );
    // Under the SCM nobody else broadcasts console events: a graceful exit
    // proves rsw's own ctrl delivery helper works in session 0.
    let out_log = std::fs::read_to_string(dir.join("logs").join("scm.out.log")).unwrap_or_default();
    assert!(
        out_log.contains("GRACEFUL-DONE"),
        "child must be stopped gracefully by rsw's ctrl-c: {out_log}"
    );
    let wrapper =
        std::fs::read_to_string(dir.join("logs").join("scm.wrapper.log")).unwrap_or_default();
    assert!(
        wrapper.contains("sent CtrlC to child"),
        "rsw must deliver the ctrl event itself: {wrapper}"
    );

    // refresh with [[on_failure]] REMOVED must clear the SCM actions
    let cfg2 = write_config(dir, false);
    let out = rsw(&["refresh", cfg2.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "refresh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        failure_action_count(),
        Some(0),
        "refresh must clear failure actions removed from the config"
    );

    // status + uninstall
    let out = rsw(&["status", cfg2.to_str().unwrap()], dir);
    assert!(out.status.success(), "status failed");
    let out = rsw(&["uninstall", cfg2.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        open_service(ServiceAccess::QUERY_STATUS).is_none(),
        "service still present after uninstall"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Real service account
// ---------------------------------------------------------------------------

const ACCT_SERVICE: &str = "rsw-scm-acct-test";
const ACCT_USER: &str = "rsw_acct_test";

/// A password that satisfies the default complexity policy and differs per
/// run, so the throwaway account never carries a well-known credential.
fn throwaway_password() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("Rsw!{}x{:08x}Zq", std::process::id(), nanos)
}

/// Creates / deletes the throwaway user via PowerShell's local-account
/// cmdlets — invoked as `-File <script> -args`, never `-Command`: endpoint
/// security products flag `ConvertTo-SecureString -AsPlainText` appearing on
/// the command line (measured on this repo's CI machine: the process hangs
/// ~30 s and dies silently, while the identical script from a file runs in
/// half a second; `net user <name> <pw> /add` is blocked the same way).
const LOCAL_USER_PS1: &str = r#"
param([string]$op, [string]$n, [string]$p)
switch ($op) {
    'exists' { if (Get-LocalUser -Name $n -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 } }
    'create' {
        $sp = ConvertTo-SecureString $p -AsPlainText -Force
        New-LocalUser -Name $n -Password $sp -ErrorAction Stop | Out-Null
    }
    'delete' { Remove-LocalUser -Name $n -ErrorAction SilentlyContinue }
}
"#;

fn local_user_op(op: &str, name: &str) -> std::io::Result<std::process::Output> {
    let script = std::env::temp_dir().join("rsw-scm-user.ps1");
    std::fs::write(&script, LOCAL_USER_PS1).expect("write helper script");
    Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .args(["-op", op, "-n", name])
        .output()
}

fn local_user_exists(name: &str) -> bool {
    local_user_op("exists", name)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn create_local_user(name: &str, password: &str) -> std::io::Result<std::process::Output> {
    let script = std::env::temp_dir().join("rsw-scm-user.ps1");
    std::fs::write(&script, LOCAL_USER_PS1).expect("write helper script");
    Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .args(["-op", "create", "-n", name, "-p", password])
        .output()
}

fn delete_local_user(name: &str) -> bool {
    local_user_op("delete", name)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `password = None` omits the whole `[service.account]` section (the
/// documented way to keep whatever account/password the SCM already stores).
fn write_account_config(dir: &std::path::Path, name: &str, password: Option<&str>) -> PathBuf {
    let child = TEST_CHILD.replace('\\', "/");
    let cfg = dir.join(name);
    let account = password
        .map(|pw| {
            format!(
                "\n[service.account]\nusername = '.\\{ACCT_USER}'\npassword = \"{pw}\"\nallow_logon_as_service = true\n"
            )
        })
        .unwrap_or_default();
    std::fs::write(
        &cfg,
        format!(
            r#"
[service]
id = "{ACCT_SERVICE}"
start_type = "manual"
{account}
[process]
executable = "{child}"
arguments = ["sleep", "500"]

[logging]
dir = "logs"
mode = "append"
"#
        ),
    )
    .unwrap();
    cfg
}

/// Real-account roundtrip: create a local user, install under it (rsw must
/// grant SeServiceLogonRight via LSA), verify the service actually STARTS
/// under that account, then verify a refresh WITHOUT `[service.account]`
/// keeps the account and password Windows already stores. Requires elevation; creates and deletes
/// a temporary local user `rsw_acct_test` (an account that already exists
/// with that name is left alone, and the test fails since its password is
/// unknown — remove it first).
#[test]
#[ignore = "requires an elevated prompt; creates a temporary local user"]
fn service_account_roundtrip() {
    if ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).is_err() {
        eprintln!("skipped: not elevated (run from an administrator prompt)");
        return;
    }

    let password = throwaway_password();
    let existed = local_user_exists(ACCT_USER);
    assert!(
        !existed,
        "local user `{ACCT_USER}` already exists; remove it (Remove-LocalUser {ACCT_USER}) and rerun"
    );
    let out = create_local_user(ACCT_USER, &password).expect("New-LocalUser failed to run");
    assert!(
        out.status.success(),
        "could not create local user `{ACCT_USER}`: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let dir = std::env::temp_dir().join(format!("rsw-scm-acct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    // The per-user temp dir is unreadable by other accounts; the service
    // runs as the throwaway user and must at least read its config/logs.
    let _ = Command::new("icacls")
        .arg(&dir)
        .args(["/grant", &format!("{ACCT_USER}:(OI)(CI)M")])
        .output();

    let result = std::panic::catch_unwind(|| account_check(&dir, &password));

    // Cleanup runs even when an assertion failed: service, temp dir, user.
    if let Some(s) = open_named_service(ACCT_SERVICE, ServiceAccess::ALL_ACCESS) {
        let _ = s.stop();
        let _ = wait_named_state(ACCT_SERVICE, ServiceState::Stopped, Duration::from_secs(30));
        let _ = s.delete();
    }
    let _ = std::fs::remove_dir_all(&dir);
    let deleted = delete_local_user(ACCT_USER);

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(deleted, "could not delete the temporary user `{ACCT_USER}`");
}

fn account_check(dir: &std::path::Path, password: &str) {
    let cfg = write_account_config(dir, "acct.toml", Some(password));

    // install: must succeed AND grant the log-on-as-a-service right
    let out = rsw(&["install", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("granted SeServiceLogonRight"),
        "LSA grant missing from install output: {stdout}"
    );

    // the SCM must now show the account as the logon identity
    {
        let svc = open_named_service(ACCT_SERVICE, ServiceAccess::QUERY_CONFIG)
            .expect("service must exist after install");
        let config = svc.query_config().unwrap();
        let start_name = config
            .account_name
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        assert!(
            start_name.ends_with(&ACCT_USER.to_lowercase()),
            "logon account is `{start_name}`, expected the test user"
        );
    }

    // the acid test: the service must actually START under that account
    let out = rsw(&["start", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "start failed (logon failure 1069 means the LSA grant did not work): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_named_state(ACCT_SERVICE, ServiceState::Running, Duration::from_secs(20)),
        "service did not reach Running"
    );

    // refresh WITHOUT [service.account]: must keep the stored account+password
    let cfg2 = write_account_config(dir, "acct-no-section.toml", None);
    let out = rsw(&["stop", cfg.to_str().unwrap()], dir);
    assert!(out.status.success());
    assert!(wait_named_state(
        ACCT_SERVICE,
        ServiceState::Stopped,
        Duration::from_secs(30)
    ));
    let out = rsw(&["refresh", cfg2.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "refresh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = rsw(&["start", cfg2.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "start after account-less refresh failed — the stored account/password was lost: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(wait_named_state(
        ACCT_SERVICE,
        ServiceState::Running,
        Duration::from_secs(20)
    ));

    let out = rsw(&["uninstall", cfg2.to_str().unwrap()], dir);
    assert!(out.status.success(), "uninstall failed");
}
