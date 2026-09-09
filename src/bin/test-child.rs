//! Test child process for rsw integration tests. Not shipped.
//!
//! Modes:
//!   test-child echo <text>...          print each arg as a line, then exit 0
//!   test-child echo-raw <text>...       print each arg WITHOUT a trailing
//!                                      newline, then exit 0 (exit-race probe)
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
//!   test-child flood <total-mb> [--line-bytes N] [--chunk-kb N] [--warmup-ms N]
//!                    [--stderr] [--no-newline]
//!                                      preload a chunk of log lines, warm up, then
//!                                      dump total-mb to stdout (or stderr) at full
//!                                      speed; prints FLOOD-ARMED/FLOOD-DONE markers
//!                                      with exact byte counts and elapsed ms

use std::io::Write as _;
use std::os::windows::io::FromRawHandle as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
use windows::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent,
    GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
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
        "echo-raw" => {
            let mut out = std::io::stdout().lock();
            for line in &args[1..] {
                let _ = out.write_all(line.as_bytes());
            }
            let _ = out.flush();
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
        "flood" => {
            let flag = |name: &str| {
                args.iter()
                    .position(|a| a == name)
                    .and_then(|i| args.get(i + 1))
                    .and_then(|v| v.parse::<u64>().ok())
            };
            let total_mb: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
            let line_bytes: usize = flag("--line-bytes").unwrap_or(120) as usize;
            let chunk_kb: usize = flag("--chunk-kb").unwrap_or(256) as usize;
            let warmup_ms: u64 = flag("--warmup-ms").unwrap_or(1000);
            let to_stderr = args.iter().any(|a| a == "--stderr");
            let no_newline = args.iter().any(|a| a == "--no-newline");
            let numbered = args.iter().any(|a| a == "--numbered");

            // Preload the magazine: one chunk of whole log lines. With
            // --numbered each line carries a global sequence number, so log
            // ordering/continuity can be verified after the flood.
            let mut line = Vec::with_capacity(line_bytes);
            let text = b"2026-09-09T12:00:00.000 INFO  [flood] simulated log line payload";
            while line.len() + 1 < line_bytes {
                let need = (line_bytes - 1) - line.len();
                let take = need.min(text.len());
                line.extend_from_slice(&text[..take]);
            }
            if !no_newline {
                line.push(b'\n');
            }
            let mut chunk = Vec::with_capacity(chunk_kb * 1024);
            while chunk.len() + line.len() <= chunk_kb * 1024 {
                chunk.extend_from_slice(&line);
            }
            let target = total_mb * 1024 * 1024;

            eprintln!(
                "FLOOD-ARMED chunk={}B line={}B target={}B{}{}",
                chunk.len(),
                line.len(),
                target,
                if no_newline { " NO-NEWLINE" } else { "" },
                if numbered { " NUMBERED" } else { "" }
            );
            std::thread::sleep(Duration::from_millis(warmup_ms));

            // Write through the raw std handle: no line buffering, full pipe speed.
            let which = if to_stderr {
                STD_ERROR_HANDLE
            } else {
                STD_OUTPUT_HANDLE
            };
            let handle = unsafe { GetStdHandle(which) }.expect("std handle");
            let mut out = unsafe {
                std::fs::File::from_raw_handle(handle.0 as std::os::windows::io::RawHandle)
            };
            let t0 = std::time::Instant::now();
            let mut written: u64 = 0;
            let mut lineno: u64 = 0;
            let mut staging = Vec::with_capacity(chunk_kb * 1024);
            while written < target {
                staging.clear();
                if numbered {
                    let body_len = if no_newline {
                        line_bytes
                    } else {
                        line_bytes - 1
                    };
                    while staging.len() + 10 + body_len <= chunk_kb * 1024 {
                        write!(&mut staging, "{lineno:09} ").unwrap();
                        staging.extend_from_slice(&line[..body_len - 10]);
                        if !no_newline {
                            staging.push(b'\n');
                        }
                        lineno += 1;
                    }
                } else {
                    staging.extend_from_slice(&chunk);
                }
                out.write_all(&staging).expect("write");
                written += staging.len() as u64;
            }
            let _ = out.flush();
            std::mem::forget(out); // closing the std handle is not ours to do
            eprintln!(
                "FLOOD-DONE bytes={} lines={} elapsed_ms={}",
                written,
                lineno,
                t0.elapsed().as_millis()
            );
        }
        other => {
            eprintln!("test-child: unknown mode {other}");
            std::process::exit(64);
        }
    }
}
