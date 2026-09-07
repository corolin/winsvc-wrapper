//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rsw",
    version,
    about = "rsw — Rust Service Wrapper: run any executable as a Windows service",
    after_help = "The config path is optional when rsw.exe is renamed (e.g. app.exe picks up app.toml)."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Don't auto-elevate via UAC for admin commands; fail with a hint instead.
    #[arg(long, global = true)]
    pub no_elevate: bool,

    /// Internal: this process was relaunched elevated by a parent rsw.
    #[arg(long, global = true, hide = true)]
    pub elevated: bool,

    /// Internal: write stdout/stderr to this file (used by the elevated child).
    #[arg(long, global = true, hide = true)]
    pub redirect: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Install the service described by the config (triggers one UAC prompt when needed).
    Install {
        /// Path to the .toml/.yaml config file.
        config: Option<PathBuf>,
    },
    /// Stop (if running) and delete the service.
    Uninstall { config: Option<PathBuf> },
    /// Start the service.
    Start { config: Option<PathBuf> },
    /// Stop the service gracefully.
    Stop { config: Option<PathBuf> },
    /// Restart the service.
    Restart { config: Option<PathBuf> },
    /// Print the service state (exit 0 running/stopped/paused, 1 transitional, 1060 not installed).
    Status { config: Option<PathBuf> },
    /// Re-read the config and update service properties without reinstall.
    Refresh { config: Option<PathBuf> },
    /// Run the wrapped process in the foreground for debugging (no SCM, no admin).
    Run { config: Option<PathBuf> },
    /// Parse and validate the config, then print the resolved result.
    Validate { config: Option<PathBuf> },
    /// Convert a WinSW XML service definition into an rsw TOML file.
    Convert {
        /// Path to the WinSW .xml service definition.
        winsw_xml: PathBuf,
        /// Output TOML path (default: <winsw_xml stem>.toml next to it).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Internal: SCM entry point baked into the service binPath.
    #[command(hide = true)]
    RunService {
        #[arg(long)]
        config: PathBuf,
    },
    /// Internal: one-shot ctrl-event delivery helper (spawned by rsw itself).
    #[command(hide = true)]
    DeliverCtrl {
        /// Target process id.
        pid: u32,
        /// Event: "c" (ctrl-c) or "b" (ctrl-break).
        event: char,
    },
}
