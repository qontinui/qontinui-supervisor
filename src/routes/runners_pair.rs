//! `POST /runners/pair-with-token` — supervisor-driven headless device pairing.
//!
//! Phase 2c of `plans/2026-05-22-mtc-iter3-remediation-web-dashboard.md`.
//! Shells out to the existing `qontinui_profile device pair --auth-token ...`
//! CLI (lives in the runner workspace, built alongside `qontinui-runner.exe`)
//! to drive the headless pair flow without opening a system browser.
//!
//! This unblocks `/manual-test-coord`'s autonomous test-runner pairing —
//! the CI machine mints a token via the web dashboard and posts it here.
//!
//! ## Where the paired state lands (and who reads it)
//!
//! The CLI writes through the runner lib's `persist_pairing`, which resolves
//! its output dir from `QONTINUI_SECURE_STORAGE_DIR` first and falls back to
//! the SHARED `{data_local_dir}/com.qontinui.runner/`. (The CLI's *read* path
//! historically ignored the env var; that is fixed on the qontinui-runner
//! branch `fix/profile-secure-storage-dir-readers` — writes were never split.)
//! That gives this route three provisioning shapes:
//!
//! - **Default (no `target_runner_id`)** — the child runs with no override and
//!   writes the shared dir, unchanged behavior. That dir is what the primary
//!   (user-started) runner reads; supervisor-spawned runners do NOT read it.
//! - **`target_runner_id`** — the supervisor points the child at that runner's
//!   per-instance dir (`process::instance_config_dir`, the same single source
//!   of truth `start_exe_mode_for_runner` exports to non-primary children as
//!   `QONTINUI_SECURE_STORAGE_DIR`), directly pairing an EXISTING/registered
//!   runner in place. The runner's heartbeat re-reads the token store from
//!   disk on each tick, so a running runner picks the credentials up without
//!   a restart.
//! - **`paired_profile_id` on `POST /runners/spawn-test`** — the established
//!   route for provisioning FUTURE spawns: pair into the shared dir here,
//!   snapshot it into `~/.qontinui/profiles/<id>/`, and spawn-test copies the
//!   snapshot into the new runner's instance dir before it starts.
//!
//! ## CLI invocation
//!
//! ```text
//! qontinui_profile device pair --auth-token <token> --tenant-id <uuid>
//! ```
//!
//! `web_base_url` (when provided) is forwarded to the child as the
//! `QONTINUI_WEB_BASE` env var. The CLI itself has no `--web-base` flag —
//! it reads from the active profile's `coord_url` (with the `QONTINUI_WEB_BASE`
//! override). See `qontinui-runner/src-tauri/src/pair.rs::pair_via_browser`
//! for the env-var contract.
//!
//! ## Response shape
//!
//! On success the CLI writes the device-JWT to `auth_tokens.enc` and the
//! `{user_id, tenant_id}` pair to `paired_user.json`, then prints a single
//! line: `device paired: user_id=<uuid> (device-token JWT saved to ...)`.
//! It does NOT print the full JSON pair-cli response, so we recover the
//! shape by reading back the on-disk artifacts:
//!
//! - `device_id` from `~/.qontinui/machine.json` (home-scoped — the SAME file
//!   regardless of `target_runner_id`)
//! - `user_id` from `paired_user.json` (cross-checked against the stdout line)
//! - `tenant_id` from `paired_user.json`
//!
//! `paired_user.json` is read back from the SAME dir the child wrote to: the
//! target runner's instance dir when `target_runner_id` was given, the shared
//! `{data_local_dir}/com.qontinui.runner/` otherwise.
//!
//! `expires_at` is NOT surfaced — the JWT exp claim lives in the
//! AES-encrypted `auth_tokens.enc` which we cannot decrypt from this layer.
//! Callers that need exp should fetch a fresh token from coord or decode
//! the JWT themselves after reading it from secure storage.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::log_capture::{LogLevel, LogSource};
use crate::process::claude_env::StripInheritedClaudeMarkers;
use crate::state::SharedState;

/// JSON body for `POST /runners/pair-with-token`.
#[derive(Debug, Clone, Deserialize)]
pub struct PairWithTokenRequest {
    /// Bearer token minted by the qontinui-web dashboard's `Auth Tokens` tab
    /// (long-lived runner token) or the future short-lived pair-code surface.
    /// Forwarded verbatim to `qontinui_profile device pair --auth-token`.
    pub token: String,

    /// Optional web base URL override. When set, forwarded to the CLI as the
    /// `QONTINUI_WEB_BASE` env var. When unset, the CLI derives the web base
    /// from the active profile's `coord_url`.
    ///
    /// Example: `https://demo.staging.qontinui.io`.
    #[serde(default)]
    pub web_base_url: Option<String>,

    /// Tenant scope for the new device. The runner CLI requires this; the
    /// supervisor refuses to invoke without it (no silent fallback to "no
    /// tenant" — that would silently produce a device row with NULL tenant).
    pub tenant_id: String,

    /// Optional override for the `qontinui_profile` binary path. Used by
    /// tests to point at a stub script. When unset, the supervisor resolves
    /// the binary via [`resolve_qontinui_profile_path`].
    #[serde(default)]
    pub qontinui_profile_path_override: Option<PathBuf>,

    /// Optional runner to pair IN PLACE. When set, the paired state is
    /// written to (and read back from) that runner's per-instance
    /// secure-storage dir — `<config_dir>/com.qontinui.runner/instances/<id>`,
    /// resolved through [`crate::process::instance_config_dir`], the same
    /// single source of truth `start_exe_mode_for_runner` exports to
    /// non-primary children — instead of the shared `data_local_dir()` dir.
    /// The id must exist in the registry (404 `runner_not_found` otherwise)
    /// and must not be the primary (the primary reads the shared dir; omit
    /// this field to pair it).
    #[serde(default)]
    pub target_runner_id: Option<String>,
}

/// Default timeout for the spawned CLI. Pair-CLI is a single HTTPS round-trip
/// to coord — 30s is generous. After this, the child is killed and the caller
/// gets HTTP 504.
const PAIR_CLI_TIMEOUT: Duration = Duration::from_secs(30);

/// Name prefix of the private directories [`stage_private_cli_copy`] creates
/// under [`private_cli_copy_root`].
pub const PRIVATE_CLI_COPY_PREFIX: &str = "qontinui-supervisor-pair-cli-";

/// Where the pair route's private copies live: the supervisor's per-user data
/// dir (the one the temp-runner ledger uses), not the system temp dir — a
/// Linux `/tmp` mounted `noexec` would make every copy unrunnable. The system
/// temp dir is only the fallback for a box with no per-user data dir.
fn private_cli_copy_root() -> PathBuf {
    dirs::data_local_dir()
        .map(|d| d.join("com.qontinui.supervisor").join("pair-cli"))
        .unwrap_or_else(std::env::temp_dir)
}

/// A leftover private copy older than this is swept by the next pair call.
/// Far above [`PAIR_CLI_TIMEOUT`], so a copy still in use is never swept.
const PRIVATE_CLI_COPY_STALE_AFTER: Duration = Duration::from_secs(3600);

/// A private copy of the `qontinui_profile` binary the pair route executes.
/// Its directory is removed when this is dropped (best effort — a Windows
/// image still being torn down can keep it a moment longer; the next pair
/// call's sweep removes it then).
struct PrivateCliCopy {
    dir: PathBuf,
    exe: PathBuf,
}

impl Drop for PrivateCliCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Copy `cli` into a fresh private directory under `temp_root` and return the
/// copy, which is what the pair route executes — never the file it resolved.
///
/// Why: the resolved `qontinui_profile` lives in a build slot, the non-pool
/// `target/debug/` or the LKG dir. A running image locks its file on Windows,
/// and a build that lands in that directory meanwhile panics in tauri-build's
/// `remove_file().unwrap()` over it. Running a copy leaves every build
/// directory unlocked.
///
/// The copy keeps the source's file name (so a `.cmd` test stub stays one) and
/// its bytes are checked: a copy of ZERO bytes — the source rewritten with
/// tauri-build's placeholder since it was resolved — is refused.
fn stage_private_cli_copy(cli: &Path, temp_root: &Path) -> Result<PrivateCliCopy, String> {
    sweep_stale_private_cli_copies(temp_root, std::time::SystemTime::now());
    let file_name = cli
        .file_name()
        .ok_or_else(|| format!("{:?} has no file name to copy", cli))?;
    let dir = temp_root.join(format!(
        "{}{}",
        PRIVATE_CLI_COPY_PREFIX,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create private copy dir {:?}: {e}", dir))?;
    let copy = PrivateCliCopy {
        exe: dir.join(file_name),
        dir,
    };
    let bytes = std::fs::copy(cli, &copy.exe)
        .map_err(|e| format!("copy {:?} -> {:?}: {e}", cli, copy.exe))?;
    if bytes == 0 {
        return Err(format!(
            "{:?} was ZERO-LENGTH when copied (a build placeholder, not a binary) — refused \
             to run it",
            cli
        ));
    }
    Ok(copy)
}

/// Remove private copies older than [`PRIVATE_CLI_COPY_STALE_AFTER`] (as of
/// `now`) left under `temp_root` by earlier pair calls. Best effort.
fn sweep_stale_private_cli_copies(temp_root: &Path, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(temp_root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(PRIVATE_CLI_COPY_PREFIX)
        {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > PRIVATE_CLI_COPY_STALE_AFTER);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Locate a REAL `qontinui_profile` binary built from the runner workspace.
/// The pair route never executes the path returned here in place — it runs a
/// private copy ([`stage_private_cli_copy`]).
///
/// Preference order, taking the first candidate that is a NON-EMPTY file:
/// 1. `<runner_npm_dir>/target/debug/qontinui_profile[.exe]` — the non-pool
///    local build (matches `runner_exe_path` for the runner binary itself).
/// 2. Each `<runner_npm_dir>/target-pool/<dir>/debug/qontinui_profile[.exe]`,
///    in directory-name order (`slot-0`, `slot-1`, …). Pair-cli is a thin
///    client, so slot drift here is not load-bearing the way it is for the
///    runner.
/// 3. `<runner_npm_dir>/target-pool/lkg/qontinui_profile[.exe]` — the copy the
///    LKG promotion carries (`build_monitor::update_lkg_after_success`).
///
/// A ZERO-LENGTH candidate is skipped, never returned. The runner's `build.rs`
/// writes a 0-byte `binaries/qontinui_profile-<triple>` placeholder and
/// tauri-build copies it over `<target>/<profile>/qontinui_profile` on every
/// build-script run, so a 0-byte file is routinely what sits at candidate 1:
/// executing it spawns nothing useful and the pair would fail with an opaque
/// error rather than this function's actionable one.
///
/// A runner build does NOT produce this binary by itself: the supervisor's
/// pool build is `cargo build --bin qontinui-runner`, which builds no other
/// bin. The fail-open sidecar build that follows it
/// (`build_monitor::SIDECAR_BUILD_ARGS`) is what builds `qontinui_profile`
/// into the slot. So the remedy the error names is a supervisor rebuild
/// (`POST /runners/spawn-test {rebuild: true}`), whose sidecar step produces
/// a fresh one.
pub fn resolve_qontinui_profile_path(state: &SharedState) -> Result<PathBuf, String> {
    resolve_qontinui_profile_in(&state.config.runner_npm_dir())
}

/// [`resolve_qontinui_profile_path`] against an explicit runner workspace
/// root (`runner_npm_dir`), so the candidate order and the non-empty rule are
/// testable against a tempdir.
fn resolve_qontinui_profile_in(npm_dir: &Path) -> Result<PathBuf, String> {
    let exe_name = crate::build_monitor::PROFILE_CLI_EXE_FILENAME;
    let canonical = npm_dir.join("target").join("debug").join(exe_name);
    let pool_root = npm_dir.join("target-pool");

    let mut candidates = vec![canonical.clone()];
    if let Ok(entries) = std::fs::read_dir(&pool_root) {
        let mut pool_dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        pool_dirs.sort();
        candidates.extend(pool_dirs.iter().map(|d| d.join("debug").join(exe_name)));
    }
    candidates.push(pool_root.join("lkg").join(exe_name));

    let mut zero_length = Vec::new();
    for candidate in candidates {
        match crate::build_monitor::SidecarFile::inspect(&candidate) {
            crate::build_monitor::SidecarFile::Present { .. } => return Ok(candidate),
            crate::build_monitor::SidecarFile::Empty => zero_length.push(candidate),
            crate::build_monitor::SidecarFile::Missing => {}
        }
    }

    let skipped = if zero_length.is_empty() {
        String::new()
    } else {
        format!(
            " Skipped {} ZERO-LENGTH candidate(s) — build placeholders, not binaries: {:?}.",
            zero_length.len(),
            zero_length
        )
    };
    Err(format!(
        "qontinui_profile binary not found at {:?}, under {:?}/*/debug or in {:?}.{} \
         Run a supervisor runner build first (e.g. POST /runners/spawn-test {{\"rebuild\": true}}); \
         its sidecar step builds qontinui_profile into the slot.",
        canonical,
        pool_root,
        pool_root.join("lkg"),
        skipped
    ))
}

/// Path to `~/.qontinui/machine.json` — the persistent device-identity file
/// the runner CLI writes (and `qontinui_profile device init` mints).
pub(crate) fn machine_json_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qontinui").join("machine.json"))
}

/// Path to the paired-user JSON the CLI writes on successful pair.
///
/// With a `target_dir` (the target runner's instance secure-storage dir, i.e.
/// what the child saw as `QONTINUI_SECURE_STORAGE_DIR`) the file lives
/// DIRECTLY under it — mirroring the runner lib's `paired_user_path()`, which
/// joins `paired_user.json` onto the env-var dir without any subdirectory.
/// Without one, the shared-dir fallback is unchanged: the Tauri app's local
/// data dir, alongside the encrypted JWT cache.
pub(crate) fn paired_user_json_path(target_dir: Option<&Path>) -> Option<PathBuf> {
    match target_dir {
        Some(dir) => Some(dir.join("paired_user.json")),
        None => {
            dirs::data_local_dir().map(|d| d.join("com.qontinui.runner").join("paired_user.json"))
        }
    }
}

/// Resolve the per-instance secure-storage dir for `target_runner_id`.
///
/// Delegates to [`crate::process::instance_config_dir`] — the documented
/// single source of truth for that path (the spawn side, the profile-write
/// side, and the removal side all funnel through it) — so this route can
/// never diverge from the dir `start_exe_mode_for_runner` points the runner
/// itself at.
pub(crate) fn pair_target_instance_dir(runner_id: &str) -> Result<PathBuf, String> {
    crate::process::instance_config_dir(runner_id).ok_or_else(|| {
        format!(
            "could not resolve the per-instance config dir \
             <config_dir>/com.qontinui.runner/instances/{runner_id} for runner '{runner_id}': \
             dirs::config_dir() returned None or the runner id is degenerate"
        )
    })
}

/// `POST /runners/pair-with-token` handler.
///
/// Returns 200 with `{device_id, user_id, tenant_id, expires_at}` on
/// success. `expires_at` is always `null` — see the module docstring.
///
/// Error responses:
/// - 400 `validation_error` for missing / malformed body fields (including a
///   primary `target_runner_id` — the primary reads the shared dir).
/// - 404 `runner_not_found` when `target_runner_id` names no registered runner.
/// - 500 `qontinui_profile_not_found` when the runner binary isn't built.
/// - 500 `pair_cli_failed` with the captured stderr + exit code.
/// - 504 `pair_cli_timeout` when the CLI hangs beyond [`PAIR_CLI_TIMEOUT`].
pub async fn pair_with_token(
    State(state): State<SharedState>,
    Json(body): Json<PairWithTokenRequest>,
) -> impl IntoResponse {
    pair_with_token_copying_into(state, body, private_cli_copy_root()).await
}

/// [`pair_with_token`] with the directory its private CLI copies go under
/// passed in, so a caller — the integration tests — can keep them out of the
/// live supervisor's per-user data dir. Deliberately a function parameter and
/// not a request field: no HTTP caller chooses where the supervisor writes.
pub async fn pair_with_token_copying_into(
    state: SharedState,
    body: PairWithTokenRequest,
    copy_root: PathBuf,
) -> axum::response::Response {
    // Body validation. We surface 400 with a structured error body so
    // callers can branch on the `error` discriminator instead of parsing
    // freeform messages.
    if body.token.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "validation_error",
                "message": "token must be a non-empty string",
            })),
        )
            .into_response();
    }
    if let Err(e) = uuid::Uuid::parse_str(body.tenant_id.trim()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "validation_error",
                "message": format!("tenant_id is not a valid UUID: {e}"),
            })),
        )
            .into_response();
    }

    // Resolve the optional target runner's instance dir. Unknown ids are a
    // structured 404; only Temp/Named runners are valid targets. The primary
    // and External (user-started) runners are rejected because they do NOT
    // read a per-instance secure-storage dir: the primary reads the SHARED
    // `data_local_dir()`, and External runners are launched outside supervisor
    // control so they never receive the `QONTINUI_SECURE_STORAGE_DIR` export
    // that `start_exe_mode_for_runner` gives Temp/Named children. Pairing
    // either into an `instances/<id>` dir would report success while the
    // runner never sees the credentials — the exact divergence bug documented
    // on `process::instance_config_dir`.
    let target_dir: Option<PathBuf> = match body.target_runner_id.as_deref() {
        None => None,
        Some(runner_id) => {
            let Some(managed) = state.get_runner(runner_id).await else {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": "runner_not_found",
                        "message": format!(
                            "target_runner_id '{runner_id}' is not in the runner registry — \
                             cannot resolve its instance secure-storage dir"
                        ),
                    })),
                )
                    .into_response();
            };
            // Only supervisor-spawned runners (Temp/Named) are pointed at a
            // per-instance secure-storage dir; everything else reads the
            // shared dir it was launched with.
            let kind = managed.config.kind();
            if !(kind.is_temp() || kind.is_named()) {
                let which = if kind.is_primary() {
                    "the primary reads the shared secure-storage dir"
                } else {
                    "External (user-started) runners are launched outside supervisor \
                     control and never read a per-instance dir"
                };
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "validation_error",
                        "message": format!(
                            "target_runner_id '{runner_id}' is not a supervisor-spawned \
                             (test-*/named-*) runner — {which}. Only Temp/Named runners can \
                             be paired in place; omit target_runner_id to pair the shared dir"
                        ),
                    })),
                )
                    .into_response();
            }
            match pair_target_instance_dir(runner_id) {
                Ok(dir) => Some(dir),
                Err(message) => {
                    return server_error("instance_dir_unresolved", &message);
                }
            }
        }
    };

    // Resolve the CLI path. The override branch is for tests; production
    // callers always go through `resolve_qontinui_profile_path`.
    let cli_path = match body.qontinui_profile_path_override.clone() {
        Some(p) => {
            if !p.exists() {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "qontinui_profile_not_found",
                        "message": format!("override path does not exist: {:?}", p),
                    })),
                )
                    .into_response();
            }
            p
        }
        None => match resolve_qontinui_profile_path(&state) {
            Ok(p) => p,
            Err(message) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "qontinui_profile_not_found",
                        "message": message,
                    })),
                )
                    .into_response();
            }
        },
    };

    // Run a private copy, never the resolved file: a running image would lock
    // a build directory's file (see `stage_private_cli_copy`). Held until the
    // child has been reaped; its directory is removed on drop.
    let private_copy = match stage_private_cli_copy(&cli_path, &copy_root) {
        Ok(copy) => copy,
        Err(message) => {
            return server_error("qontinui_profile_copy_failed", &message);
        }
    };
    let exec_path = private_copy.exe.clone();

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "pair-with-token: invoking {:?} (private copy of {:?}) with tenant_id={} \
                 web_base_url={:?} target_runner_id={:?} secure_storage_dir={:?}",
                exec_path,
                cli_path,
                body.tenant_id,
                body.web_base_url,
                body.target_runner_id,
                target_dir
            ),
        )
        .await;

    // Spawn the CLI. Captured stdout/stderr feed the response body on
    // failure; on success we read the on-disk artifacts.
    let mut cmd = tokio::process::Command::new(&exec_path);
    cmd.arg("device")
        .arg("pair")
        .arg("--auth-token")
        .arg(&body.token)
        .arg("--tenant-id")
        .arg(&body.tenant_id)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        // A pair call that times out drops the child; without this it would
        // keep running, unbounded, after the 504 was answered.
        .kill_on_drop(true)
        // Don't let inherited Claude Code markers leak into the child;
        // mirrors what manager.rs does on runner spawn.
        .strip_inherited_claude_markers();
    if let Some(web_base) = &body.web_base_url {
        cmd.env("QONTINUI_WEB_BASE", web_base);
    }
    // Point the child at the target runner's instance dir. The runner lib's
    // `persist_pairing` (and `SecureStorage::new`) resolve their output dir
    // from this env var, so both credential halves — `auth_tokens.enc` and
    // `paired_user.json` — land in the dir the target runner actually reads.
    // Create it first: the target runner may never have started.
    if let Some(dir) = &target_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return server_error(
                "instance_dir_create_failed",
                &format!(
                    "failed to create instance secure-storage dir {}: {e}",
                    dir.display()
                ),
            );
        }
        cmd.env("QONTINUI_SECURE_STORAGE_DIR", dir);
    }

    let output_fut = async {
        // Name the program the Command actually holds, not a variable beside
        // it, so the message can never claim a different file than the one
        // that was spawned.
        let spawned = PathBuf::from(cmd.as_std().get_program());
        let child = cmd.spawn().map_err(|e| {
            format!(
                "failed to spawn {:?}: {e} (private copy of {:?}; path exists? {})",
                spawned,
                cli_path,
                spawned.exists()
            )
        })?;
        child
            .wait_with_output()
            .await
            .map_err(|e| format!("wait_with_output: {e}"))
    };

    let output = match tokio::time::timeout(PAIR_CLI_TIMEOUT, output_fut).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "pair_cli_spawn_failed",
                    "message": e,
                })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({
                    "error": "pair_cli_timeout",
                    "message": format!(
                        "qontinui_profile pair-cli exceeded {}s timeout",
                        PAIR_CLI_TIMEOUT.as_secs()
                    ),
                })),
            )
                .into_response();
        }
    };

    drop(private_copy);

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let exit_code = output.status.code();
        state
            .logs
            .emit(
                LogSource::Supervisor,
                LogLevel::Warn,
                format!(
                    "pair-with-token: qontinui_profile exit={:?} stderr={}",
                    exit_code,
                    stderr.lines().last().unwrap_or("")
                ),
            )
            .await;
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "pair_cli_failed",
                "exit_code": exit_code,
                "stderr": stderr,
                "stdout": stdout,
            })),
        )
            .into_response();
    }

    // CLI succeeded — recover the canonical response shape from the on-disk
    // artifacts. Anything missing here is a CLI contract violation; we
    // surface it as 500 so the caller knows the pair "succeeded" but the
    // supervisor couldn't read back the expected state.
    let machine_json = match machine_json_path() {
        Some(p) => p,
        None => {
            return server_error(
                "home_dir_unresolved",
                "could not resolve home directory to read machine.json",
            );
        }
    };
    // Read back from the SAME dir the child wrote to: the target runner's
    // instance dir when one was resolved, the shared dir otherwise.
    let paired_json = match paired_user_json_path(target_dir.as_deref()) {
        Some(p) => p,
        None => {
            return server_error(
                "data_local_dir_unresolved",
                "could not resolve data_local_dir to read paired_user.json",
            );
        }
    };

    let device_id = match read_machine_device_id(&machine_json) {
        Ok(id) => id,
        Err(e) => {
            return server_error("machine_json_read_failed", &e);
        }
    };
    let (user_id, persisted_tenant_id) = match read_paired_user(&paired_json) {
        Ok(p) => p,
        Err(e) => {
            return server_error("paired_user_read_failed", &e);
        }
    };

    let tenant_id_out = persisted_tenant_id.unwrap_or_else(|| body.tenant_id.clone());

    state
        .logs
        .emit(
            LogSource::Supervisor,
            LogLevel::Info,
            format!(
                "pair-with-token: success device_id={} user_id={} tenant_id={}",
                device_id, user_id, tenant_id_out
            ),
        )
        .await;

    (
        StatusCode::OK,
        Json(json!({
            "device_id": device_id,
            "user_id": user_id,
            "tenant_id": tenant_id_out,
            // CLI does not surface the JWT's exp claim — see module docs.
            "expires_at": serde_json::Value::Null,
        })),
    )
        .into_response()
}

fn server_error(error_kind: &str, message: &str) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": error_kind,
            "message": message,
        })),
    )
        .into_response()
}

/// Read `device_id` from `~/.qontinui/machine.json`. Tolerates the legacy
/// `machine_id` key via the same serde alias the runner uses.
pub(crate) fn read_machine_device_id(path: &Path) -> Result<String, String> {
    #[derive(serde::Deserialize)]
    struct MachineFile {
        #[serde(alias = "machine_id")]
        device_id: String,
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let parsed: MachineFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(parsed.device_id)
}

/// Read `(user_id, tenant_id)` from `paired_user.json`. `tenant_id` may be
/// absent on legacy files written before the 2026-05-22 schema bump — we
/// fall back to the request body's tenant_id in that case (see caller).
pub(crate) fn read_paired_user(path: &Path) -> Result<(String, Option<String>), String> {
    #[derive(serde::Deserialize)]
    struct PairedUserFile {
        user_id: String,
        #[serde(default)]
        tenant_id: Option<String>,
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let parsed: PairedUserFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok((parsed.user_id, parsed.tenant_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // `stage_private_cli_copy` — the pair route runs a private copy of the
    // resolved binary, never the build directory's file.

    #[test]
    fn private_cli_copy_is_a_byte_identical_copy_outside_the_source_dir() {
        let root = tempdir().unwrap();
        let src_dir = root.path().join("target-pool").join("slot-0").join("debug");
        fs::create_dir_all(&src_dir).unwrap();
        let src = src_dir.join(crate::build_monitor::PROFILE_CLI_EXE_FILENAME);
        fs::write(&src, b"real-profile").unwrap();
        let temp_root = root.path().join("tmp");
        fs::create_dir_all(&temp_root).unwrap();

        let copy = stage_private_cli_copy(&src, &temp_root).expect("staged");
        assert!(copy.exe.starts_with(&temp_root));
        assert_eq!(copy.exe.file_name(), src.file_name());
        assert_eq!(fs::read(&copy.exe).unwrap(), b"real-profile");
        let dir = copy.dir.clone();
        drop(copy);
        assert!(!dir.exists(), "the private copy is removed on drop");
        assert!(src.exists(), "the source is never touched");
    }

    #[test]
    fn private_cli_copy_refuses_zero_bytes() {
        let root = tempdir().unwrap();
        let src = root
            .path()
            .join(crate::build_monitor::PROFILE_CLI_EXE_FILENAME);
        fs::write(&src, b"").unwrap();
        let temp_root = root.path().join("tmp");
        fs::create_dir_all(&temp_root).unwrap();

        let err = match stage_private_cli_copy(&src, &temp_root) {
            Ok(_) => panic!("a zero-byte copy must be refused"),
            Err(e) => e,
        };
        assert!(err.contains("ZERO-LENGTH"), "{err}");
        assert_eq!(
            fs::read_dir(&temp_root).unwrap().count(),
            0,
            "a refused copy leaves no directory behind"
        );
    }

    #[test]
    fn private_cli_copy_sweep_removes_only_its_own_stale_copies() {
        let root = tempdir().unwrap();
        let temp_root = root.path();
        let stale = temp_root.join(format!("{PRIVATE_CLI_COPY_PREFIX}stale"));
        let fresh = temp_root.join(format!("{PRIVATE_CLI_COPY_PREFIX}fresh"));
        let foreign = temp_root.join("someone-elses-dir");
        for d in [&stale, &fresh, &foreign] {
            fs::create_dir_all(d).unwrap();
        }
        // As of now, every copy is fresh: nothing is swept.
        sweep_stale_private_cli_copies(temp_root, std::time::SystemTime::now());
        assert!(
            stale.exists() && fresh.exists(),
            "a copy that may be in use is kept"
        );

        // Two stale-windows later, both prefixed copies are stale.
        let later = std::time::SystemTime::now() + PRIVATE_CLI_COPY_STALE_AFTER * 2;
        sweep_stale_private_cli_copies(temp_root, later);
        assert!(
            !stale.exists() && !fresh.exists(),
            "stale private copies are swept"
        );
        assert!(foreign.exists(), "nothing without the prefix is touched");
    }

    // `resolve_qontinui_profile_in` — a zero-length `qontinui_profile` (the
    // runner build.rs placeholder that tauri-build copies into
    // `target/debug/`) is skipped, never handed to the pair CLI spawn.

    fn write_profile(dir: &Path, bytes: &[u8]) -> PathBuf {
        fs::create_dir_all(dir).expect("mkdir");
        let p = dir.join(crate::build_monitor::PROFILE_CLI_EXE_FILENAME);
        fs::write(&p, bytes).expect("write qontinui_profile");
        p
    }

    #[test]
    fn profile_resolution_skips_a_zero_length_canonical_for_a_nonempty_slot() {
        let root = tempdir().unwrap();
        let npm = root.path();
        write_profile(&npm.join("target").join("debug"), b"");
        write_profile(&npm.join("target-pool").join("slot-0").join("debug"), b"");
        let real = write_profile(
            &npm.join("target-pool").join("slot-1").join("debug"),
            b"real-profile",
        );

        assert_eq!(resolve_qontinui_profile_in(npm).unwrap(), real);
    }

    #[test]
    fn profile_resolution_prefers_a_nonempty_canonical_and_falls_back_to_lkg() {
        let root = tempdir().unwrap();
        let npm = root.path();
        let lkg = write_profile(&npm.join("target-pool").join("lkg"), b"lkg-profile");
        assert_eq!(
            resolve_qontinui_profile_in(npm).unwrap(),
            lkg,
            "the LKG copy is the last candidate"
        );

        let canonical = write_profile(&npm.join("target").join("debug"), b"local-profile");
        assert_eq!(resolve_qontinui_profile_in(npm).unwrap(), canonical);
    }

    #[test]
    fn profile_resolution_errors_when_every_candidate_is_zero_length() {
        let root = tempdir().unwrap();
        let npm = root.path();
        write_profile(&npm.join("target").join("debug"), b"");
        write_profile(&npm.join("target-pool").join("slot-0").join("debug"), b"");
        write_profile(&npm.join("target-pool").join("lkg"), b"");

        let err = resolve_qontinui_profile_in(npm).expect_err("all candidates are 0-byte");
        assert!(
            err.contains("Skipped 3 ZERO-LENGTH candidate(s)"),
            "the error must say why the files that exist were refused: {err}"
        );
    }

    #[test]
    fn reads_machine_device_id_canonical() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("machine.json");
        fs::write(
            &path,
            r#"{"device_id":"11111111-1111-4111-8111-111111111111","hostname":"x"}"#,
        )
        .unwrap();
        let id = read_machine_device_id(&path).expect("read device_id");
        assert_eq!(id, "11111111-1111-4111-8111-111111111111");
    }

    #[test]
    fn reads_machine_device_id_legacy_alias() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("machine.json");
        // Pre-rename file uses `machine_id` instead of `device_id`.
        fs::write(
            &path,
            r#"{"machine_id":"22222222-2222-4222-8222-222222222222","hostname":"x"}"#,
        )
        .unwrap();
        let id = read_machine_device_id(&path).expect("read device_id");
        assert_eq!(id, "22222222-2222-4222-8222-222222222222");
    }

    #[test]
    fn reads_paired_user_with_tenant() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("paired_user.json");
        fs::write(
            &path,
            r#"{"user_id":"33333333-3333-4333-8333-333333333333","tenant_id":"44444444-4444-4444-8444-444444444444"}"#,
        )
        .unwrap();
        let (user_id, tenant_id) = read_paired_user(&path).expect("read");
        assert_eq!(user_id, "33333333-3333-4333-8333-333333333333");
        assert_eq!(
            tenant_id.as_deref(),
            Some("44444444-4444-4444-8444-444444444444")
        );
    }

    /// A known (well-formed) runner id resolves to the SAME per-instance dir
    /// `start_exe_mode_for_runner` exports to the runner itself —
    /// `<config_dir>/com.qontinui.runner/instances/<id>` — because both sides
    /// funnel through `process::instance_config_dir`. If this shape drifts,
    /// pair-with-token writes credentials into a dir the target never reads.
    #[test]
    fn pair_target_instance_dir_matches_spawn_side_shape() {
        if dirs::config_dir().is_none() {
            return;
        }
        let dir = pair_target_instance_dir("test-9877").expect("normal id must resolve");
        assert_eq!(
            Some(dir.as_path()),
            crate::process::instance_config_dir("test-9877").as_deref(),
            "pair side and spawn side must agree byte-for-byte on the instance dir"
        );
        assert!(
            dir.ends_with(std::path::Path::new(
                "com.qontinui.runner/instances/test-9877"
            )),
            "unexpected instance-dir shape: {dir:?}"
        );
    }

    /// Degenerate ids (empty / traversal) must error, never resolve to the
    /// shared `instances/` parent.
    #[test]
    fn pair_target_instance_dir_rejects_degenerate_ids() {
        for bad in ["", "a/b", "a\\b", ".."] {
            assert!(
                pair_target_instance_dir(bad).is_err(),
                "pair_target_instance_dir({bad:?}) must be an error"
            );
        }
    }

    /// With a target dir, `paired_user.json` is read DIRECTLY under it —
    /// mirroring the runner lib's `paired_user_path()`, which joins the file
    /// name onto `QONTINUI_SECURE_STORAGE_DIR` with no subdirectory.
    #[test]
    fn paired_user_json_path_prefers_target_dir() {
        let dir = tempdir().unwrap();
        let path = paired_user_json_path(Some(dir.path())).expect("target dir always resolves");
        assert_eq!(path, dir.path().join("paired_user.json"));
    }

    /// Without a target dir, the shared-dir fallback is unchanged:
    /// `{data_local_dir}/com.qontinui.runner/paired_user.json`.
    #[test]
    fn paired_user_json_path_falls_back_to_shared_dir() {
        let Some(expected) =
            dirs::data_local_dir().map(|d| d.join("com.qontinui.runner").join("paired_user.json"))
        else {
            return;
        };
        assert_eq!(paired_user_json_path(None), Some(expected));
    }

    #[test]
    fn reads_paired_user_legacy_no_tenant() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("paired_user.json");
        fs::write(
            &path,
            r#"{"user_id":"33333333-3333-4333-8333-333333333333"}"#,
        )
        .unwrap();
        let (user_id, tenant_id) = read_paired_user(&path).expect("read");
        assert_eq!(user_id, "33333333-3333-4333-8333-333333333333");
        assert!(tenant_id.is_none());
    }
}
