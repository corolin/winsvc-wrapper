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

fn open_service(access: ServiceAccess) -> Option<windows_service::service::Service> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    manager.open_service(SERVICE, access).ok()
}

fn wait_state(want: ServiceState, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(s) = open_service(ServiceAccess::QUERY_STATUS)
            && let Ok(st) = s.query_status()
            && st.current_state == want
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
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

/// Real-account roundtrip: create a local user, install under it (rsw must
/// grant SeServiceLogonRight via LSA), verify the service actually STARTS
/// under that account, then verify refresh with an EMPTY password keeps the
/// password Windows already stores. Requires elevation; creates and deletes
/// a temporary local user `rsw_acct_test`.
#[test]
#[ignore = "requires an elevated prompt; creates a temporary local user"]
fn service_account_roundtrip() {
    use windows_service::service::ServiceAccess as SA;

    if ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).is_err() {
        eprintln!("skipped: not elevated (run from an administrator prompt)");
        return;
    }

    let user = "test";
    let pass = "test!";

    // Use the existing account if present; otherwise create it. Creation may
    // fail on machines with a password-complexity policy — the user is
    // expected to have created it beforehand in that case.
    let user_exists = Command::new("net")
        .args(["user", user])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !user_exists {
        let out = Command::new("net")
            .args(["user", user, pass, "/add"])
            .output()
            .expect("net user command failed to run");
        assert!(
            out.status.success(),
            "could not create local user `{user}` (password policy?): {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let dir = std::env::temp_dir().join(format!("rsw-scm-acct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("logs")).unwrap();

    let result = (|| -> anyhow::Result<()> {
        let child = TEST_CHILD.replace('\\', "/");
        let cfg = dir.join("acct.toml");
        std::fs::write(
            &cfg,
            format!(
                r#"
[service]
id = "rsw-scm-acct-test"

[service.account]
username = '.\{user}'
password = "{pass}"
allow_logon_as_service = true

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

        // create the local user (elevated test process can)
        let out = Command::new("net")
            .args(["user", user, pass, "/add"])
            .output()?;
        assert!(
            out.status.success(),
            "net user add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // install: must succeed AND grant the log-on-as-a-service right
        let out = rsw(&["install", cfg.to_str().unwrap()], &dir);
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
            let manager =
                ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
                    .unwrap();
            let svc = manager.open_service(SERVICE, SA::QUERY_CONFIG).unwrap();
            let config = svc.query_config().unwrap();
            let start_name = config
                .account_name
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            assert!(
                start_name.ends_with("rsw_acct_test"),
                "logon account is `{start_name}`, expected the test user"
            );
        }

        // the acid test: the service must actually START under that account
        let out = rsw(&["start", cfg.to_str().unwrap()], &dir);
        assert!(
            out.status.success(),
            "start failed (logon failure 1069 means the LSA grant did not work): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            wait_state(ServiceState::Running, Duration::from_secs(20)),
            "service did not reach Running"
        );

        // refresh with an EMPTY password: must keep the stored password
        let cfg2 = dir.join("acct-empty-pass.toml");
        std::fs::write(
            &cfg2,
            format!(
                r#"
[service]
id = "rsw-scm-acct-test"

[service.account]
username = '.\{user}'
password = ""

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
        let out = rsw(&["stop", cfg.to_str().unwrap()], &dir);
        assert!(out.status.success());
        assert!(wait_state(ServiceState::Stopped, Duration::from_secs(30)));
        let out = rsw(&["refresh", cfg2.to_str().unwrap()], &dir);
        assert!(
            out.status.success(),
            "refresh failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = rsw(&["start", cfg2.to_str().unwrap()], &dir);
        assert!(
            out.status.success(),
            "start after empty-password refresh failed — the stored password was lost: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(wait_state(ServiceState::Running, Duration::from_secs(20)));

        // cleanup
        let out = rsw(&["uninstall", cfg2.to_str().unwrap()], &dir);
        assert!(out.status.success(), "uninstall failed");
        Ok(())
    })();

    // best-effort cleanup of the service; the `test` user itself is kept
    if let Some(s) = open_service(SA::ALL_ACCESS) {
        let _ = s.stop();
        let _ = s.delete();
    }
    let _ = std::fs::remove_dir_all(&dir);
    result.unwrap();
}
