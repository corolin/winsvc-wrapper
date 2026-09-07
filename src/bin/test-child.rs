//! Test child process for rsw integration tests. Not shipped.
//!
//! Modes:
//!   test-child echo <text>...          print each arg as a line, then exit 0
//!   test-child spam <n> [--to-stderr]  print n numbered lines (~20 bytes each)
//!   test-child crash <code> [after-ms] exit with the given code
//!   test-child graceful [cleanup-ms]   install a ctrl handler; on ctrl event,
//!                                      wait cleanup-ms, print GRACEFUL-DONE, exit 0
//!   test-child sleep <ms>              sleep then exit 0
//!   test-child hang                    print STARTED then sleep forever
//!   test-child spawn-grandchild        spawn `test-child hang`, print its pid, then hang
//!   test-child ctrl-c <pid>            attach to <pid>'s console and broadcast ctrl-c
//!   test-child ctrl-break <pid>        attach to <pid>'s console and broadcast ctrl-break
//!   test-child alive <pid>             exit 0 if <pid> is still running, 1 otherwise

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
use windows::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
    SetConsoleCtrlHandler,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::BOOL;

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(ctrl_type: u32) -> BOOL {
    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            STOP.store(true, Ordering::SeqCst);
            BOOL(1)
        }
        _ => BOOL(0),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("hang");
    match mode {
        "echo" => {
            for line in &args[1..] {
                println!("{line}");
            }
        }
        "spam" => {
            let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let to_stderr = args.iter().any(|a| a == "--to-stderr");
            for i in 0..n {
                let line = format!("SPAM {:06} the quick brown fox", i);
                if to_stderr {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        "crash" => {
            let code: i32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1);
            let after: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            if after > 0 {
                std::thread::sleep(Duration::from_millis(after));
            }
            std::process::exit(code);
        }
        "graceful" => {
            let cleanup_ms: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200);
            unsafe {
                // Clear the "ignore ctrl-c" flag inherited from a
                // CREATE_NEW_PROCESS_GROUP parent, then install our handler.
                let _ = SetConsoleCtrlHandler(None, false);
                let _ = SetConsoleCtrlHandler(Some(on_ctrl), true);
            }
            println!("GRACEFUL-READY");
            let _ = std::io::stdout().flush();
            while !STOP.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(cleanup_ms));
            println!("GRACEFUL-DONE");
            let _ = std::io::stdout().flush();
        }
        "sleep" => {
            let ms: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1000);
            std::thread::sleep(Duration::from_millis(ms));
        }
        "hang" => {
            println!("STARTED");
            let _ = std::io::stdout().flush();
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        "hang-immune" => {
            // Like hang, but swallows console ctrl events — only a stop
            // command or TerminateProcess can end it.
            unsafe {
                let _ = SetConsoleCtrlHandler(Some(on_ctrl), true);
            }
            println!("STARTED");
            let _ = std::io::stdout().flush();
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        "spawn-grandchild" => {
            let exe = std::env::current_exe().unwrap();
            let grandchild = std::process::Command::new(exe)
                .arg("hang")
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn grandchild");
            println!("GRANDCHILD {}", grandchild.id());
            // The grandchild deliberately outlives this process; leak the handle.
            std::mem::forget(grandchild);
            let _ = std::io::stdout().flush();
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        "ctrl-c" | "ctrl-break" => {
            let pid: u32 = args.get(1).and_then(|s| s.parse().ok()).expect("pid");
            let event = if mode == "ctrl-c" {
                CTRL_C_EVENT
            } else {
                CTRL_BREAK_EVENT
            };
            unsafe {
                // A process can only attach to one console: detach from ours first.
                let _ = FreeConsole();
                if AttachConsole(pid).is_err() {
                    eprintln!("attach failed");
                    std::process::exit(1);
                }
                let result = GenerateConsoleCtrlEvent(event, 0);
                let _ = FreeConsole();
                if result.is_err() {
                    eprintln!("signal failed");
                    std::process::exit(1);
                }
            }
        }
        "alive" => {
            let pid: u32 = args.get(1).and_then(|s| s.parse().ok()).expect("pid");
            let alive = unsafe {
                match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    Ok(handle) => {
                        let mut code: u32 = 0;
                        let ok = GetExitCodeProcess(handle, &mut code).is_ok()
                            && code == STILL_ACTIVE.0 as u32;
                        let _ = CloseHandle(handle);
                        ok
                    }
                    Err(_) => false,
                }
            };
            std::process::exit(if alive { 0 } else { 1 });
        }
        other => {
            eprintln!("test-child: unknown mode {other}");
            std::process::exit(64);
        }
    }
}
