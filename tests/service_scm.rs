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
        # Build the SecureString via .NET directly: ConvertTo-SecureString
        # lives in Microsoft.PowerShell.Security, whose auto-loading is
        # broken on some machines (CouldNotAutoloadMatchingModule), while
        # ::new()/AppendChar are language/CLR features with no cmdlet
        # dependency. CreateService validates the logon synchronously, so
        # also pin the password policy (default must-change/expires policies
        # fail that check with os error 1057 on newer Windows builds).
        $sp = [System.Security.SecureString]::new()
        foreach ($c in $p.ToCharArray()) { [void]$sp.AppendChar($c) }
        New-LocalUser -Name $n -Password $sp -ErrorAction Stop | Out-Null
        Set-LocalUser -Name $n -PasswordNeverExpires $true -ErrorAction Stop
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

// ---------------------------------------------------------------------------
// allow_start_stop delegation (SDDL DACL) + apply
// ---------------------------------------------------------------------------

const DELG_SERVICE: &str = "rsw-scm-sddl-test";

fn current_account() -> String {
    format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    )
}

fn write_delegated_config(dir: &std::path::Path, account: &str) -> PathBuf {
    let child = TEST_CHILD.replace('\\', "/");
    // TOML basic-string escaping: account names carry backslashes
    // ("COROLIN-PC\dream") that must never reach the file raw.
    let account_toml = account.replace('\\', "\\\\").replace('"', "\\\"");
    let cfg = dir.join("sddl.toml");
    std::fs::write(
        &cfg,
        format!(
            r#"[service]
id = "{DELG_SERVICE}"
start_type = "manual"
allow_start_stop = ["{account_toml}"]

[process]
executable = "{child}"
arguments = ["sleep", "60000"]

[logging]
dir = "logs"
mode = "append"
"#
        ),
    )
    .unwrap();
    cfg
}

/// Reads the service DACL back as an SDDL string. The service SID string in
/// each ACE is compared byte-for-byte — no localized output is involved.
fn service_dacl_sddl(name: &str) -> anyhow::Result<String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};
    use windows::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceObjectSecurity,
        SC_MANAGER_CONNECT, SERVICE_ALL_ACCESS,
    };
    use windows::core::PCWSTR;

    let name_w: Vec<u16> = name.encode_utf16().chain([0]).collect();
    unsafe {
        let manager = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
            .map_err(|e| anyhow::anyhow!("OpenSCManager: {e}"))?;
        let svc = OpenServiceW(manager, PCWSTR(name_w.as_ptr()), SERVICE_ALL_ACCESS)
            .map_err(|e| anyhow::anyhow!("OpenService: {e}"));
        let _ = CloseServiceHandle(manager);
        let svc = svc?;

        let mut buf = [0u8; 8192];
        let mut needed = 0u32;
        let q = QueryServiceObjectSecurity(
            svc,
            DACL_SECURITY_INFORMATION.0,
            Some(PSECURITY_DESCRIPTOR(buf.as_mut_ptr().cast())),
            buf.len() as u32,
            &mut needed,
        );
        let _ = CloseServiceHandle(svc);
        q.map_err(|e| anyhow::anyhow!("QueryServiceObjectSecurity: {e}"))?;

        let sd = PSECURITY_DESCRIPTOR(buf.as_mut_ptr().cast());
        let mut sddl = windows::core::PWSTR::null();
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut sddl,
            None,
        )
        .map_err(|e| anyhow::anyhow!("converting the DACL to SDDL failed: {e}"))?;
        let out = sddl.to_string().map_err(|e| anyhow::anyhow!("{e}"));
        let _ = LocalFree(Some(HLOCAL(sddl.as_ptr().cast())));
        out
    }
}

/// Rights field of the allow-ACE granted to `sid` in an SDDL DACL string.
/// ACEs are `(type;flags;rights;o;i;sid)`; rights are returned verbatim —
/// callers must check membership, not equality (SCM reorders bits on
/// read-back and expands generic rights).
fn ace_rights(dacl: &str, sid: &str) -> Option<String> {
    dacl.split('(').find_map(|ace| {
        let f: Vec<&str> = ace.trim_end_matches(')').split(';').collect();
        (f.len() == 6 && f[0] == "A" && f[5] == sid).then(|| f[2].to_string())
    })
}

#[test]
fn ace_rights_parses_normalized_sddl() {
    // Verbatim read-back from a real QueryServiceObjectSecurity call: the
    // written GA came back expanded to concrete rights and the delegated
    // RPWPLC was reordered to LCRPWP — substring matching cannot work.
    let dacl = "D:P(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;SY)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)(A;;LCRPWP;;;S-1-5-21-294617185-3689988605-2414484505-1001)";
    assert_eq!(
        ace_rights(dacl, "BA").as_deref(),
        Some("CCDCLCSWRPWPDTLOCRSDRCWDWO")
    );
    let r = ace_rights(dacl, "S-1-5-21-294617185-3689988605-2414484505-1001").unwrap();
    for right in ["LC", "RP", "WP"] {
        assert!(r.contains(right), "`{r}` must contain {right}");
    }
    assert_eq!(ace_rights(dacl, "S-1-1-0"), None);
}

#[test]
#[ignore = "requires an elevated prompt; run with --ignored"]
fn allow_start_stop_dacl_and_unelevated_start_stop() {
    if ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).is_err() {
        eprintln!("skipped: not elevated (run from an administrator prompt)");
        return;
    }

    let dir = std::env::temp_dir().join(format!("rsw-scm-sddl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    let account = current_account();
    let cfg = write_delegated_config(&dir, &account);

    let result = std::panic::catch_unwind(|| delegation_check(&dir, &cfg, &account));

    if let Some(s) = open_named_service(DELG_SERVICE, ServiceAccess::ALL_ACCESS) {
        let _ = s.stop();
        let _ = s.delete();
    }
    let _ = std::fs::remove_dir_all(&dir);

    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn delegation_check(dir: &std::path::Path, cfg: &std::path::Path, account: &str) {
    // validate must resolve the account to a SID before anything touches the SCM
    let out = rsw(&["validate", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "validate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let prefix = format!("allow_start_stop: {account} -> ");
    let sid = stdout
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("validate must print `{prefix}<SID>`: {stdout}"))
        .to_string();
    assert!(sid.starts_with("S-1-"), "unexpected SID `{sid}`");

    let out = rsw(&["install", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // the persisted DACL must carry the delegated ACE. Windows normalizes
    // SDDL on read-back (rights bits are reordered, e.g. RPWPLC -> LCRPWP,
    // and GA expands to the concrete service rights), so assert on parsed
    // rights membership — never on raw substrings.
    let dacl = service_dacl_sddl(DELG_SERVICE).expect("reading the service DACL");
    let rights = ace_rights(&dacl, &sid)
        .unwrap_or_else(|| panic!("DACL `{dacl}` has no ACE for the delegated {sid}"));
    for right in ["LC", "RP", "WP"] {
        assert!(
            rights.contains(right),
            "delegated rights `{rights}` must include {right} (DACL: {dacl})"
        );
    }
    for well_known in ["BA", "SY"] {
        let r = ace_rights(&dacl, well_known)
            .unwrap_or_else(|| panic!("DACL `{dacl}` has no {well_known} ACE"));
        for right in ["DC", "RP", "WP", "WD"] {
            assert!(
                r.contains(right),
                "{well_known} rights `{r}` must include {right} (DACL: {dacl})"
            );
        }
    }

    // the acid test: a restricted (basic-user, non-elevated) token must be
    // able to start AND stop with --no-elevate — the proof that a delegated
    // user needs no UAC prompt at all. runas launches the child in its own
    // console; the service state is the authoritative signal.
    for verb in ["start", "stop"] {
        let want = if verb == "start" {
            ServiceState::Running
        } else {
            ServiceState::Stopped
        };
        let out = Command::new("runas")
            .args([
                "/trustlevel:0x20000",
                &format!(
                    "\"{}\" {verb} --no-elevate \"{}\"",
                    RSW,
                    cfg.to_str().unwrap()
                ),
            ])
            .current_dir(dir)
            .output()
            .expect("runas failed to launch");
        assert!(
            wait_named_state(DELG_SERVICE, want, Duration::from_secs(30)),
            "unelevated `rsw {verb}` did not reach {want:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // apply on a stopped service: refresh + start in one command
    let out = rsw(&["apply", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_named_state(DELG_SERVICE, ServiceState::Running, Duration::from_secs(20)),
        "apply must start a stopped service"
    );

    // apply on a running service: config is refreshed, the service is left
    // running (no restart)
    let out = rsw(&["apply", cfg.to_str().unwrap()], dir);
    assert!(
        out.status.success(),
        "second apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("config takes effect on next start"),
        "apply must leave a running service alone: {stdout}"
    );
    assert!(
        wait_named_state(DELG_SERVICE, ServiceState::Running, Duration::from_secs(5)),
        "service must stay running"
    );

    let out = rsw(&["uninstall", cfg.to_str().unwrap()], dir);
    assert!(out.status.success(), "uninstall failed");
}
