//! UAC auto-elevation for SCM control commands (WinSW parity).
//!
//! `install`/`uninstall`/`start`/`stop`/`restart`/`refresh` need an elevated
//! token. Run from a non-elevated terminal, rsw relaunches itself via the
//! `runas` verb — the user confirms one UAC prompt and the command completes
//! in place. The elevated child redirects its stdout/stderr to a temp file
//! that the parent echoes back after it exits.

use std::env;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE};
use windows::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    IsUserAnAdmin, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::PCWSTR;

/// True when the current process token has admin privileges.
pub fn is_current_process_elevated() -> bool {
    unsafe { IsUserAnAdmin().as_bool() }
}

/// Relaunches the current executable with `runas`, appending the internal
/// elevation markers, waits for it, echoes its output, and returns its exit
/// code. The child inherits `cwd` so sidecar and relative config paths work.
pub fn relaunch_elevated(exe: &Path, args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    let redirect = temp_redirect_path();
    // Create the file NOW, exclusively, as the unelevated user: the elevated
    // child then only ever writes into a file we own, and a pre-planted file
    // or junction at a guessable name makes the run fail instead of being
    // followed with admin rights.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&redirect)
        .with_context(|| format!("creating redirect file {}", redirect.display()))?;
    let mut params = args.to_vec();
    params.push("--elevated".into());
    params.push("--redirect".into());
    params.push(redirect.to_string_lossy().into_owned());
    let cmdline = crate::binpath::join(&params);

    let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let cmdline_w: Vec<u16> = cmdline.encode_utf16().chain([0]).collect();
    let verb_w: Vec<u16> = "runas".encode_utf16().chain([0]).collect();
    let dir_w: Vec<u16> = cwd.as_os_str().encode_wide().chain([0]).collect();

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb_w.as_ptr()),
        lpFile: PCWSTR(exe_w.as_ptr()),
        lpParameters: PCWSTR(cmdline_w.as_ptr()),
        lpDirectory: PCWSTR(dir_w.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut sei).map_err(|e| {
            if e.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
                anyhow::anyhow!("elevation request was cancelled; nothing was changed")
            } else {
                anyhow::anyhow!("could not elevate: {e}")
            }
        })?;
        if sei.hProcess.is_invalid() {
            bail!("elevation did not produce a process handle");
        }
        WaitForSingleObject(sei.hProcess, INFINITE);
        let mut code: u32 = 0;
        GetExitCodeProcess(sei.hProcess, &mut code)
            .map_err(|e| anyhow::anyhow!("GetExitCodeProcess failed: {e}"))?;
        let _ = CloseHandle(sei.hProcess);

        // Echo whatever the elevated child printed (output is buffered in the
        // redirect file; SCM commands are short so this is effectively live).
        if let Ok(text) = std::fs::read_to_string(&redirect) {
            eprint!("{text}")
        }
        let _ = std::fs::remove_file(&redirect);
        Ok(code as i32)
    }
}

/// Called by the elevated child, before any output: points stdout/stderr at
/// the redirect file the parent will read back.
pub fn redirect_output_to(path: &Path) -> anyhow::Result<()> {
    use std::fs::OpenOptions;

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening redirect file {}", path.display()))?;
    let handle = HANDLE(file.as_raw_handle());
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, handle)
            .map_err(|e| anyhow::anyhow!("SetStdHandle(stdout) failed: {e}"))?;
        SetStdHandle(STD_ERROR_HANDLE, handle)
            .map_err(|e| anyhow::anyhow!("SetStdHandle(stderr) failed: {e}"))?;
    }
    // Keep the raw handle valid for the process lifetime.
    std::mem::forget(file);
    Ok(())
}

/// Per-run redirect file name: pid plus a nanosecond stamp, so the name is
/// not guessable ahead of time.
fn temp_redirect_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!("rsw-elevated-{}-{nanos:x}.log", std::process::id()))
}
