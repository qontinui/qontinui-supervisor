//! CI runner lifecycle management (Phase 4b of self-hosted CI runners).
//!
//! Handles install, configure, start, stop, update, and remove of the
//! GitHub Actions runner binary inside WSL. All WSL interactions are
//! synchronous `std::process::Command` calls wrapped in
//! `tokio::task::spawn_blocking` at the call-site (or by the async
//! convenience functions in this module).
//!
//! The runner binary lives at `~/actions-runner/` inside the default WSL
//! distribution. Configuration state is persisted by the runner itself in
//! `~/actions-runner/.runner` (a JSON file written by `config.sh`).

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use crate::wsl_util::wsl_command;

// ---------------------------------------------------------------------------
// WSL helper
// ---------------------------------------------------------------------------

/// Execute a command inside the default WSL distribution via
/// `wsl -e bash -c "..."`. Returns trimmed stdout on success.
fn wsl_exec(cmd: &str) -> Result<String> {
    let output = wsl_command()
        .args(["-e", "bash", "-c", cmd])
        .output()
        .context("failed to spawn wsl process")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "WSL command failed (exit {}): {}",
            output.status,
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Sync primitives (run inside spawn_blocking)
// ---------------------------------------------------------------------------

/// Check whether a runner is already configured (`.runner` sentinel exists).
pub fn is_runner_installed() -> bool {
    wsl_exec("test -f ~/actions-runner/.runner && echo yes || echo no")
        .map(|s| s == "yes")
        .unwrap_or(false)
}

/// Check whether the runner binary directory exists at all.
#[allow(dead_code)]
fn is_runner_dir_present() -> bool {
    wsl_exec("test -d ~/actions-runner && echo yes || echo no")
        .map(|s| s == "yes")
        .unwrap_or(false)
}

/// Download and extract the latest GitHub Actions runner binary into
/// `~/actions-runner/`. Idempotent — overwrites existing binaries.
fn download_runner_binary() -> Result<()> {
    let cmd = r#"mkdir -p ~/actions-runner && cd ~/actions-runner && \
        LATEST=$(curl -sL https://api.github.com/repos/actions/runner/releases/latest \
            | grep -oP '"tag_name":\s*"v\K[^"]+') && \
        if [ -z "$LATEST" ]; then echo "ERR: could not determine latest runner version" >&2; exit 1; fi && \
        curl -sL "https://github.com/actions/runner/releases/download/v${LATEST}/actions-runner-linux-x64-${LATEST}.tar.gz" \
            | tar xz"#;
    let out = wsl_exec(cmd)?;
    if !out.is_empty() {
        info!("download_runner_binary: {out}");
    }
    Ok(())
}

/// Run `config.sh` to register this runner with GitHub.
fn configure_runner(token: &str, org: &str, labels: &[String], name: &str) -> Result<()> {
    // Validate inputs to prevent command injection.
    for part in [token, org, name] {
        if part.contains([';', '|', '&', '$', '`']) {
            bail!("input contains suspicious characters: {part}");
        }
    }
    for label in labels {
        if label.contains([';', '|', '&', '$', '`']) {
            bail!("label contains suspicious characters: {label}");
        }
    }

    let labels_str = labels.join(",");
    let cmd = format!(
        r#"cd ~/actions-runner && ./config.sh \
            --url "https://github.com/{org}" \
            --token "{token}" \
            --name "{name}" \
            --labels "{labels_str}" \
            --work _work \
            --runnergroup default \
            --unattended \
            --replace"#
    );
    let out = wsl_exec(&cmd)?;
    info!("configure_runner: {out}");
    Ok(())
}

/// Install the runner as a systemd service and start it.
fn install_and_start_service() -> Result<()> {
    let out = wsl_exec("cd ~/actions-runner && sudo ./svc.sh install && sudo ./svc.sh start")?;
    info!("install_and_start_service: {out}");
    Ok(())
}

/// Stop the runner systemd service.
fn stop_service() -> Result<()> {
    wsl_exec("cd ~/actions-runner && sudo ./svc.sh stop")?;
    Ok(())
}

/// Start the runner systemd service.
fn start_service() -> Result<()> {
    wsl_exec("cd ~/actions-runner && sudo ./svc.sh start")?;
    Ok(())
}

/// Unconfigure the runner (deregister from GitHub).
fn unconfigure_runner(token: &str) -> Result<()> {
    if token.contains([';', '|', '&', '$', '`']) {
        bail!("token contains suspicious characters");
    }
    wsl_exec(&format!(
        r#"cd ~/actions-runner && ./config.sh remove --token "{token}""#
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Async public API
// ---------------------------------------------------------------------------

/// Install and configure the GitHub Actions runner. Idempotent — if the
/// runner is already configured (`.runner` exists), this is a no-op.
pub async fn install_runner(token: &str, org: &str, labels: &[String], name: &str) -> Result<()> {
    // 1. Check if already configured.
    let already = tokio::task::spawn_blocking(is_runner_installed).await?;
    if already {
        info!("install_runner: runner already configured, skipping install");
        return Ok(());
    }

    // 2. Download binary.
    info!("install_runner: downloading runner binary...");
    tokio::task::spawn_blocking(download_runner_binary).await??;

    // 3. Configure.
    let token = token.to_string();
    let org = org.to_string();
    let labels = labels.to_vec();
    let name = name.to_string();
    info!("install_runner: configuring runner (org={org}, name={name})...");
    tokio::task::spawn_blocking(move || configure_runner(&token, &org, &labels, &name)).await??;

    // 4. Install and start the service.
    info!("install_runner: installing and starting systemd service...");
    tokio::task::spawn_blocking(install_and_start_service).await??;

    info!("install_runner: complete");
    Ok(())
}

/// Stop the runner service.
pub async fn stop_runner() -> Result<()> {
    info!("stop_runner: stopping CI runner service...");
    tokio::task::spawn_blocking(stop_service).await??;
    info!("stop_runner: stopped");
    Ok(())
}

/// Start the runner service.
pub async fn start_runner() -> Result<()> {
    info!("start_runner: starting CI runner service...");
    tokio::task::spawn_blocking(start_service).await??;
    info!("start_runner: started");
    Ok(())
}

/// Update the runner binary to the latest version. Stops the service,
/// downloads the new binary, then starts the service again.
pub async fn update_runner() -> Result<()> {
    info!("update_runner: stopping service for update...");
    stop_runner().await?;

    info!("update_runner: downloading latest runner binary...");
    tokio::task::spawn_blocking(download_runner_binary).await??;

    info!("update_runner: restarting service...");
    start_runner().await?;

    info!("update_runner: complete");
    Ok(())
}

/// Remove the runner completely: stop the service (best-effort), then
/// unconfigure (deregister from GitHub).
pub async fn remove_runner(token: &str) -> Result<()> {
    info!("remove_runner: stopping service (best-effort)...");
    if let Err(e) = stop_runner().await {
        warn!("remove_runner: stop failed (continuing): {e}");
    }

    info!("remove_runner: unconfiguring runner...");
    let token = token.to_string();
    tokio::task::spawn_blocking(move || unconfigure_runner(&token)).await??;

    info!("remove_runner: complete");
    Ok(())
}

/// Return the currently installed runner version string, or `None` if the
/// runner binary is not present.
#[allow(dead_code)]
pub async fn installed_version() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        wsl_exec("cd ~/actions-runner && ./config.sh --version 2>/dev/null || true")
            .ok()
            .filter(|s| !s.is_empty())
    })
    .await
    .ok()
    .flatten()
}
