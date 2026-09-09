//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Build flavor shown in `--version`: full ships the download feature, lite
/// (built with --no-default-features) is the offline wrapper without it.
#[cfg(feature = "download")]
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (full)");
#[cfg(not(feature = "download"))]
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (lite)");

/// Hidden subcommand rsw spawns on itself to deliver a console ctrl event.
pub const DELIVER_CTRL_COMMAND: &str = "__deliver-ctrl";

#[derive(Parser, Debug)]
#[command(
    name = "rsw",
    version = VERSION,
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
    /// The explicit name pins the contract with `process_win::send_ctrl_event`
    /// (clap would otherwise derive `deliver-ctrl`).
    #[command(name = DELIVER_CTRL_COMMAND, hide = true)]
    DeliverCtrl {
        /// Target process id.
        pid: u32,
        /// Event: "c" (ctrl-c) or "b" (ctrl-break).
        event: char,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `process_win::send_ctrl_event` spawns rsw with this exact subcommand
    /// name; a mismatch silently disables graceful stops.
    #[test]
    fn deliver_ctrl_subcommand_name_matches_spawner() {
        let cli = Cli::try_parse_from(["rsw", DELIVER_CTRL_COMMAND, "4242", "b"])
            .expect("hidden helper subcommand must parse");
        assert!(matches!(
            cli.command,
            Command::DeliverCtrl {
                pid: 4242,
                event: 'b'
            }
        ));
    }
}
