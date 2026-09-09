//! Windows process plumbing: job objects (kill-on-close), console allocation,
//! ctrl-event delivery, window closing, and creation flags.

use std::env;
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Console::{
    AllocConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, GetConsoleProcessList, GetConsoleWindow,
    SetConsoleCtrlHandler,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, PostMessageW, SW_HIDE, ShowWindow, WM_CLOSE, WNDENUMPROC,
};
use windows::core::{BOOL, PCWSTR};

use crate::config::{Priority, StopSignal};

// ---------------------------------------------------------------------------
// Console ctrl swallow handler
// ---------------------------------------------------------------------------

/// Set by our own `PHANDLER_ROUTINE` when the user (or we ourselves) raises a
/// console ctrl event. The supervisor treats this as a stop request.
pub static CONSOLE_STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn swallow_ctrl_handler(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        // Swallow ctrl-c / ctrl-break so we stay alive to orchestrate the
        // stop ladder; the child decides on its own how to react.
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            CONSOLE_STOP.store(true, Ordering::SeqCst);
            BOOL(1)
        }
        _ => BOOL(0),
    }
}

/// Registers the swallow handler for the current console.
///
/// Also re-enables ctrl-c delivery: a process created with
/// `CREATE_NEW_PROCESS_GROUP` starts with ctrl-c ignored (and passes that
/// state to its children). `SetConsoleCtrlHandler(None, false)` clears the
/// ignore flag; our handler runs first and swallows the event, so rsw stays
/// alive while the child reacts on its own.
pub fn install_ctrl_swallow_handler() {
    unsafe {
        let _ = SetConsoleCtrlHandler(None, false);
        let _ = SetConsoleCtrlHandler(Some(swallow_ctrl_handler), true);
    }
}

/// Ensures the current process has a console (services run in session 0 with
/// none). Allocates one and hides its window.
pub fn ensure_console() {
    if have_console() {
        return;
    }
    unsafe {
        let _ = AllocConsole();
        let hwnd = GetConsoleWindow();
        if !hwnd.is_invalid() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// True when this process is attached to a console.
pub fn have_console() -> bool {
    unsafe { GetConsoleProcessList(&mut [0u32; 1]) > 0 }
}

// ---------------------------------------------------------------------------
// Job object
// ---------------------------------------------------------------------------

/// A job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: every process
/// assigned to it (including grandchildren spawned later) is terminated by the
/// kernel when the last handle closes — even if rsw itself crashes.
pub struct ProcessJob(HANDLE);

impl ProcessJob {
    pub fn create_kill_on_close() -> windows::core::Result<ProcessJob> {
        unsafe {
            let job = CreateJobObjectW(None, PCWSTR::null())?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )?;
            Ok(ProcessJob(job))
        }
    }

    /// Assigns a freshly spawned child. Must happen right after spawn, before
    /// the child can fork grandchildren.
    pub fn assign(&self, child: &Child) -> windows::core::Result<()> {
        let handle = HANDLE(child.as_raw_handle());
        unsafe { AssignProcessToJobObject(self.0, handle) }
    }

    /// Terminates the whole tree right now. Used in the force-kill path so
    /// grandchildren die at kill time instead of when the job handle closes.
    pub fn terminate_tree(&self) {
        unsafe {
            let _ = windows::Win32::System::JobObjects::TerminateJobObject(self.0, 1);
        }
    }

    /// Closing the handle is enough thanks to KILL_ON_JOB_CLOSE.
    pub fn terminate(&self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Drop for ProcessJob {
    fn drop(&mut self) {
        self.terminate();
    }
}

// ---------------------------------------------------------------------------
// Ctrl-event delivery
// ---------------------------------------------------------------------------

/// Delivers a console ctrl event to the child, wherever its console lives.
///
/// The broadcast is performed by a short-lived helper process (rsw itself,
/// hidden) rather than by rsw directly: an in-process AttachConsole would
/// expose rsw to its own CTRL_C broadcast, and under some console states the
/// swallow handler loses the race against the default terminator
/// (STATUS_CONTROL_C_EXIT). The helper installs a swallow handler of its own,
/// so only the target hears the event.
pub fn send_ctrl_event(signal: StopSignal, child_pid: u32) -> std::io::Result<()> {
    let event_arg = match signal {
        StopSignal::CtrlC => "c",
        StopSignal::CtrlBreak => "b",
        StopSignal::Kill => unreachable!("kill signal must not be delivered as a ctrl event"),
    };
    use std::os::windows::process::CommandExt as _;
    let exe = env::current_exe().map_err(std::io::Error::other)?;
    let out = Command::new(exe)
        .args([
            crate::cli::DELIVER_CTRL_COMMAND,
            &child_pid.to_string(),
            event_arg,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(std::io::Error::other)?;
    match out.code() {
        Some(DELIVER_OK) => Ok(()),
        // The helper broadcast the event and was then torn down by that same
        // event before its swallow handler ran (the default handler exits with
        // STATUS_CONTROL_C_EXIT). Delivery happened; treat it as success.
        Some(code) if code as u32 == STATUS_CONTROL_C_EXIT => Ok(()),
        Some(DELIVER_NO_CONSOLE) => Err(std::io::Error::other(
            "target process has no console to receive the event",
        )),
        Some(DELIVER_GENERATE_FAILED) => Err(std::io::Error::other(
            "GenerateConsoleCtrlEvent failed in the delivery helper",
        )),
        Some(code) => Err(std::io::Error::other(format!(
            "ctrl delivery helper exited with {code}"
        ))),
        None => Err(std::io::Error::other(
            "ctrl delivery helper terminated without an exit code",
        )),
    }
}

/// Exit codes of the `__deliver-ctrl` helper.
pub const DELIVER_OK: i32 = 0;
pub const DELIVER_NO_CONSOLE: i32 = 1;
pub const DELIVER_GENERATE_FAILED: i32 = 2;
pub const DELIVER_BAD_SIGNAL: i32 = 3;
/// NTSTATUS a process exits with when the default ctrl handler terminates it.
pub const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// Helper entry for `__deliver-ctrl`: attaches to the target console,
/// broadcasts the event, swallowing it in this process.
///
/// Two details keep the helper alive through its own broadcast (measured:
/// without them it dies with STATUS_CONTROL_C_EXIT most of the time):
/// the swallow handler is installed *after* `AttachConsole`, and the helper
/// lingers briefly before `FreeConsole` so the ctrl dispatch thread reaches
/// our handler instead of the default terminator. `send_ctrl_event` still
/// tolerates STATUS_CONTROL_C_EXIT as "delivered" for belt and braces.
pub fn deliver_ctrl_event_direct(target_pid: u32, signal: StopSignal) -> i32 {
    use windows::Win32::System::Console::{AttachConsole, FreeConsole, GenerateConsoleCtrlEvent};
    let event = match signal {
        StopSignal::CtrlC => CTRL_C_EVENT,
        StopSignal::CtrlBreak => CTRL_BREAK_EVENT,
        StopSignal::Kill => return DELIVER_BAD_SIGNAL,
    };
    unsafe {
        let _ = FreeConsole(); // detach from the inherited console, if any
        if AttachConsole(target_pid).is_err() {
            return DELIVER_NO_CONSOLE; // target has no console
        }
        let _ = SetConsoleCtrlHandler(None, false);
        let _ = SetConsoleCtrlHandler(Some(swallow_ctrl_handler), true);
        let result = GenerateConsoleCtrlEvent(event, 0);
        // Let the ctrl-handler thread reach our swallow handler before we
        // detach; detaching mid-dispatch lets the default terminator win.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = FreeConsole();
        match result {
            Ok(()) => DELIVER_OK,
            Err(_) => DELIVER_GENERATE_FAILED,
        }
    }
}

// ---------------------------------------------------------------------------
// CloseMainWindow fallback
// ---------------------------------------------------------------------------

unsafe extern "system" fn enum_windows_of_pid(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let collect = unsafe { &mut *(lparam.0 as *mut Vec<(HWND, u32)>) };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    collect.push((hwnd, pid));
    BOOL(1) // keep enumerating
}

/// Posts `WM_CLOSE` to every visible top-level window owned by `pid`
/// (the moral equivalent of .NET's `Process.CloseMainWindow`).
pub fn close_main_windows_of(pid: u32) -> usize {
    let mut all: Vec<(HWND, u32)> = Vec::new();
    let callback: WNDENUMPROC = Some(enum_windows_of_pid);
    unsafe {
        let _ = EnumWindows(callback, LPARAM(&mut all as *mut _ as isize));
        let mut sent = 0usize;
        for (hwnd, owner) in all {
            if owner == pid && PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok() {
                sent += 1;
            }
        }
        sent
    }
}

// ---------------------------------------------------------------------------
// Creation flags
// ---------------------------------------------------------------------------

pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
const NORMAL_PRIORITY_CLASS: u32 = 0x0000_0020;
const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;
const HIGH_PRIORITY_CLASS: u32 = 0x0000_0080;
const REALTIME_PRIORITY_CLASS: u32 = 0x0000_0100;

/// Computes `CREATE_PROCESS` flags from config.
///
/// Note: `CREATE_NEW_PROCESS_GROUP` is only set for `ctrl_break` stops — a new
/// process group ignores ctrl-c until it opts back in, so reserving the group
/// lets us target ctrl-break precisely.
pub fn creation_flags(priority: Priority, stop_signal: StopSignal, hide_window: bool) -> u32 {
    let _ = hide_window; // see note below: implemented by console inheritance
    let mut flags = match priority {
        Priority::Idle => IDLE_PRIORITY_CLASS,
        Priority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        Priority::Normal => NORMAL_PRIORITY_CLASS,
        Priority::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        Priority::High => HIGH_PRIORITY_CLASS,
        Priority::Realtime => REALTIME_PRIORITY_CLASS,
    };
    if stop_signal == StopSignal::CtrlBreak {
        flags |= CREATE_NEW_PROCESS_GROUP;
    }
    // NOTE: deliberately NO `CREATE_NO_WINDOW` for hide_window. Children
    // inherit rsw's own console, which is already hidden in service mode
    // (AllocConsole + SW_HIDE). CREATE_NO_WINDOW would put the child in a
    // separate invisible console where console ctrl events can never reach
    // it, silently breaking graceful stops.
    flags
}
