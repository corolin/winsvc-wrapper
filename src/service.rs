//! SCM integration: the `run-service` entry point installed into the service
//! binPath, the control handler, and status reporting.

use std::ffi::OsString;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};

use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};

use crate::config::Resolved;
use crate::logging::{Clock, LogSink};
use crate::process_win;
use crate::supervisor::{self, ControlEvent, ServiceExit, StatusUpdate};

static SERVICE: OnceLock<Arc<Resolved>> = OnceLock::new();

define_windows_service!(ffi_service_main, rsw_service_main);

/// Entry point used by `main` when the SCM launches the binary.
pub fn run_service(resolved: Arc<Resolved>) -> windows_service::Result<()> {
    let _ = SERVICE.set(resolved);
    let name = SERVICE.get().unwrap().service_id().to_string();
    windows_service::service_dispatcher::start(name, ffi_service_main)
}

fn rsw_service_main(_arguments: Vec<OsString>) {
    let (exit_code, notes) = std::panic::catch_unwind(run).unwrap_or_else(|panic| {
        let note = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown panic".into());
        (
            ServiceExitCode::Win32(1067),
            vec![format!("service panicked: {note}")],
        )
    });
    // Report the final STOPPED status with the mapped exit code. If the
    // handler was never registered (very early failure) this is best effort.
    if let Some(handle) = HANDLE.get() {
        let accepted = accept_mask();
        let _ = handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: accepted,
            exit_code,
            checkpoint: 0,
            wait_hint: std::time::Duration::default(),
            process_id: None,
        });
    }
    let _ = notes;
}

static HANDLE: OnceLock<ServiceStatusHandle> = OnceLock::new();

fn accept_mask() -> ServiceControlAccept {
    let cfg = SERVICE
        .get()
        .map(|r| r.cfg.service.preshutdown)
        .unwrap_or(false);
    let mut mask = ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN;
    if cfg {
        mask |= ServiceControlAccept::PRESHUTDOWN;
    }
    mask
}

fn run() -> (ServiceExitCode, Vec<String>) {
    let resolved = Arc::clone(SERVICE.get().unwrap());
    let notes: Vec<String> = Vec::new();

    let (tx, rx) = mpsc::channel::<ControlEvent>();
    let handle =
        match service_control_handler::register(resolved.service_id(), move |event| match event {
            ServiceControl::Stop => {
                let _ = tx.send(ControlEvent::Stop);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Shutdown | ServiceControl::Preshutdown => {
                let _ = tx.send(ControlEvent::Shutdown);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("rsw: RegisterServiceCtrlHandlerEx failed: {e}");
                return (ServiceExitCode::Win32(1067), notes);
            }
        };
    let _ = HANDLE.set(handle);

    let set_status = |state, checkpoint, wait_hint_secs, exit_code| {
        let _ = HANDLE.get().map(|h| {
            h.set_service_status(ServiceStatus {
                service_type: ServiceType::OWN_PROCESS,
                current_state: state,
                controls_accepted: accept_mask(),
                exit_code,
                checkpoint,
                wait_hint: std::time::Duration::from_secs(wait_hint_secs),
                process_id: None,
            })
        });
    };

    set_status(ServiceState::StartPending, 1, 30, ServiceExitCode::NO_ERROR);

    // Services run in session 0 without a console; we need one to deliver
    // ctrl events to the child later.
    process_win::ensure_console();
    process_win::install_ctrl_swallow_handler();

    let sink = match LogSink::open(
        &resolved.cfg.logging,
        resolved.log_dir(),
        &resolved.log_basename(),
        false,
        Clock::system(),
    ) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("rsw: cannot open logs: {e}");
            set_status(ServiceState::Stopped, 0, 0, ServiceExitCode::Win32(5));
            return (ServiceExitCode::Win32(5), notes);
        }
    };
    sink.info(&format!(
        "rsw {} service starting (id: {}, config: {})",
        env!("CARGO_PKG_VERSION"),
        resolved.service_id(),
        resolved.config_path.display()
    ));

    let status_handle = HANDLE.get().unwrap();
    let exit = supervisor::supervise(
        &resolved,
        &sink,
        &rx,
        &|update: StatusUpdate| match update {
            StatusUpdate::Running => {
                set_status(ServiceState::Running, 0, 0, ServiceExitCode::NO_ERROR)
            }
            StatusUpdate::StopPending {
                elapsed_ms,
                wait_hint_ms,
            } => set_status(
                ServiceState::StopPending,
                elapsed_ms / 1000 + 1,
                wait_hint_ms as u64 / 1000 + 1,
                ServiceExitCode::NO_ERROR,
            ),
        },
        false, // service mode: console ctrl events raised by the child are not stop requests
    );
    sink.info(&format!("service stopping: {exit:?}"));
    let _ = status_handle;

    let exit_code = map_service_exit(&exit, &resolved);
    (exit_code, notes)
}

/// Maps the supervisor outcome onto the service exit code the SCM sees
/// (and which drives configured failure actions).
fn map_service_exit(exit: &ServiceExit, resolved: &Resolved) -> ServiceExitCode {
    match exit {
        ServiceExit::Stopped | ServiceExit::StoppedDuringDelay(_) => ServiceExitCode::NO_ERROR,
        ServiceExit::ChildExited(code) => {
            if resolved.cfg.process.success_exit_codes.contains(code) {
                ServiceExitCode::NO_ERROR
            } else {
                ServiceExitCode::ServiceSpecific(*code)
            }
        }
        ServiceExit::StartFailed(os) => match os {
            Some(code) => ServiceExitCode::Win32(*code),
            None => ServiceExitCode::Win32(1067), // ERROR_PROCESS_ABORTED
        },
    }
}
