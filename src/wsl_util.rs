//! Shared constructor for `wsl` subprocesses.
//!
//! Every `wsl` invocation in the supervisor goes through [`wsl_command`] so
//! that on Windows the transient console window is suppressed
//! (`CREATE_NO_WINDOW`). Without this flag each `wsl` spawn flashes a console
//! window on screen — and the CI-runner probe loop (`ci_runner_probe.rs`)
//! fires several `wsl` calls every 30s, so the flashing is continuous.
//!
//! This mirrors the `CREATE_NO_WINDOW` handling already applied to the cargo
//! build, expo, and process-manager spawns elsewhere in the crate.

use std::process::Command;

/// Build a `Command` for `wsl`, suppressing the transient console window on
/// Windows. Add args/output handling at the call site as usual.
pub fn wsl_command() -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = Command::new("wsl");
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    {
        Command::new("wsl")
    }
}
