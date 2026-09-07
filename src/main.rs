//! rsw — Rust Service Wrapper.

mod account;
mod binpath; // escape/join are used for elevation; parse is test-only
mod cli;
mod config;
mod control;
mod convert;
mod download;
mod elevation;
mod hooks;
mod logging;
mod mapping;
mod process_win;
mod sddl;
mod service;
mod stop;
mod supervisor;

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;

use anyhow::Context as _;
use clap::Parser as _;

use crate::cli::{Cli, Command};
use crate::config::resolve_config_path;
use crate::logging::{Clock, LogSink};
use crate::supervisor::{ServiceExit, StatusUpdate, supervise};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // The elevated child must redirect its output before printing anything.
    if let Some(redirect) = &cli.redirect {
        elevation::redirect_output_to(redirect)?;
    }

    // SCM control commands auto-elevate (one UAC prompt) when run from a
    // non-elevated terminal, mirroring WinSW; --no-elevate restores plain
    // failure. `run`/`validate`/`status` never need it, and the elevated
    // child itself must not recurse.
    let needs_elevation = matches!(
        cli.command,
        Command::Install { .. }
            | Command::Uninstall { .. }
            | Command::Start { .. }
            | Command::Stop { .. }
            | Command::Restart { .. }
            | Command::Refresh { .. }
    );
    if needs_elevation
        && !cli.elevated
        && !cli.no_elevate
        && !elevation::is_current_process_elevated()
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let code = elevation::relaunch_elevated(
            &std::env::current_exe()?,
            &args,
            &std::env::current_dir()?,
        )?;
        std::process::exit(code);
    }

    let code = match cli.command {
        Command::Run { config } => cmd_run(config.as_deref())?,
        Command::Validate { config } => {
            cmd_validate(config.as_deref())?;
            0
        }
        Command::RunService { config } => cmd_run_service(&config)?,
        Command::DeliverCtrl { pid, event } => {
            let signal = match event {
                'c' => config::StopSignal::CtrlC,
                'b' => config::StopSignal::CtrlBreak,
                _ => anyhow::bail!("unknown ctrl event: {event}"),
            };
            std::process::exit(process_win::deliver_ctrl_event_direct(pid, signal));
        }
        Command::Convert { winsw_xml, output } => {
            convert::run(&winsw_xml, output.as_deref())?;
            0
        }
        Command::Install { config } => with_config(config.as_deref(), control::install)?,
        Command::Uninstall { config } => with_config(config.as_deref(), control::uninstall)?,
        Command::Start { config } => with_config(config.as_deref(), control::start)?,
        Command::Stop { config } => with_config(config.as_deref(), control::stop)?,
        Command::Restart { config } => with_config(config.as_deref(), control::restart)?,
        Command::Status { config } => with_config(config.as_deref(), control::status)?,
        Command::Refresh { config } => with_config(config.as_deref(), control::refresh)?,
    };
    std::process::exit(code as i32);
}

fn with_config(
    explicit: Option<&Path>,
    run: impl Fn(&config::Resolved) -> anyhow::Result<u32>,
) -> anyhow::Result<u32> {
    let r = load_resolved(explicit)?;
    run(&r)
}

fn load_resolved(explicit: Option<&Path>) -> anyhow::Result<config::Resolved> {
    let path = resolve_config_path(explicit)?;
    config::Resolved::load(&path).map_err(|e| anyhow::anyhow!("{e}"))
}

fn cmd_validate(explicit: Option<&Path>) -> anyhow::Result<()> {
    let r = load_resolved(explicit)?;
    println!(
        "{}",
        toml::to_string_pretty(&r.cfg).context("serializing the parsed config")?
    );
    Ok(())
}

fn cmd_run(explicit: Option<&Path>) -> anyhow::Result<u32> {
    let r = load_resolved(explicit)?;
    let sink = Arc::new(LogSink::open(
        &r.cfg.logging,
        r.log_dir(),
        &r.log_basename(),
        true, // tee child output to the console
        Clock::system(),
    )?);
    sink.info(&format!(
        "rsw {} starting in foreground mode (service: {})",
        env!("CARGO_PKG_VERSION"),
        r.service_id()
    ));

    process_win::install_ctrl_swallow_handler();

    let (tx, rx) = mpsc::channel();
    let exit = supervise(
        &r,
        &sink,
        &rx,
        &|update: StatusUpdate| {
            if let StatusUpdate::StopPending { .. } = update {
                sink.info("stopping...");
            }
        },
        true, // interactive: ctrl-c in our console is a stop request
    );
    drop(tx);

    sink.info(&format!("rsw exiting: {exit:?}"));
    Ok(run_exit_code(&exit, &r))
}

fn cmd_run_service(config: &Path) -> anyhow::Result<u32> {
    let resolved = Arc::new(config::Resolved::load(config).map_err(|e| anyhow::anyhow!("{e}"))?);
    service::run_service(Arc::clone(&resolved)).map_err(|e| {
        if matches!(&e, windows_service::Error::Winapi(io) if io.raw_os_error() == Some(1063)) {
            anyhow::anyhow!(
                "run-service is launched by the Windows service manager; use `rsw run` to debug the wrapped process in the foreground"
            )
        } else {
            anyhow::anyhow!("service dispatcher failed: {e}")
        }
    })?;
    Ok(0)
}

/// Process exit code for foreground runs: mirror the child's fate.
fn run_exit_code(exit: &ServiceExit, r: &config::Resolved) -> u32 {
    match exit {
        ServiceExit::Stopped | ServiceExit::StoppedDuringDelay(_) => 0,
        ServiceExit::ChildExited(code) => {
            if r.cfg.process.success_exit_codes.contains(code) {
                0
            } else {
                *code
            }
        }
        ServiceExit::StartFailed(os) => os.unwrap_or(1067), // ERROR_PROCESS_ABORTED
    }
}
