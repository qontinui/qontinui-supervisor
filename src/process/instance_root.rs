//! The **instance root** of a supervisor-spawned (temp / named) runner — Phase 4
//! of plan `2026-10-04-a-subject-runner-must-be-fully-isolated-from-the-harness-runner`.
//!
//! A runner launched with `QONTINUI_INSTANCE_ROOT=<dir>` is a SUBJECT: every
//! runner-owned location resolves under that one directory, and every
//! root-confined override it is handed must lie inside it or it refuses to
//! start (exit 78, `<root>/startup-refusal.log`). The contract is qontinui-runner
//! `src-tauri/src/instance_env.rs`; this module is the supervisor's half of it.
//!
//! The root is [`super::instance_config_dir`] — the per-instance dir the
//! supervisor already owned, so the existing reaper ([`super::remove_instance_config_dir`])
//! removes everything the subject wrote with one `remove_dir_all`.
//!
//! # Every location is still exported explicitly
//!
//! A runner that honours the root derives these defaults itself, so exporting
//! them is redundant for it. They are exported anyway, because the supervisor
//! can launch an LKG or otherwise older binary that predates the root: such a
//! binary ignores `QONTINUI_INSTANCE_ROOT`, and without the explicit vars it
//! would fall back to the harness's shared stores — the original incident. Each
//! value lies inside the root, which is exactly what the contract requires.
//!
//! # Secure storage and config ARE the root (deviation from the plan text)
//!
//! The plan's Phase 4 names `<root>/secure` as the destination. A runner given
//! no override would default to `<root>/secure` and `<root>/config`; the
//! supervisor deliberately keeps exporting `QONTINUI_SECURE_STORAGE_DIR` and
//! `QONTINUI_CONFIG_DIR` as the root itself (the contract allows a confined key
//! to BE the root):
//!
//! 1. an older binary reads exactly the dir it always read, and
//! 2. a persistent NAMED runner keeps its existing `settings.json` and
//!    `paired_user.json` without a migration.
//!
//! What the plan meant — "the paired-profile writer writes into the subject's
//! own secure dir" — holds by construction: the env block and every writer
//! resolve that dir through [`InstanceRoot::secure_storage_dir`] (via
//! [`instance_secure_storage_dir`]).

use std::path::{Path, PathBuf};

/// The env var naming a subject runner's instance root.
pub const INSTANCE_ROOT_ENV: &str = "QONTINUI_INSTANCE_ROOT";

/// Mirror of qontinui-runner `instance_env::ROOT_CONFINED_ENV_KEYS`: under an
/// instance root, each of these that is set non-blank must lie inside the root,
/// or the runner refuses to start.
///
/// Used by the `extra_env` override warning and by the tests that assert the
/// whole non-primary spawn block honours the contract.
pub const ROOT_CONFINED_ENV_KEYS: [&str; 10] = [
    "QONTINUI_CAPABILITY_STATE_DIR",
    "QONTINUI_CONFIG_DIR",
    "QONTINUI_EMBEDDED_PG_DIR",
    "QONTINUI_HOME",
    "QONTINUI_PANIC_LOG_DIR",
    "QONTINUI_PROMPTS_DIR",
    "QONTINUI_RUNNER_LOG_DIR",
    "QONTINUI_SECURE_STORAGE_DIR",
    "QONTINUI_SESSION_NAMES_DIR",
    "WEBVIEW2_USER_DATA_FOLDER",
];

/// The files copied from the harness's `~/.qontinui` into a subject's
/// `<root>/home` before it starts — explicit provisioning INTO the root, which
/// D2 of the plan allows (implicit reads outside it are what it forbids).
///
/// - `machine.json` — the box's device identity (`device_id`, hostname, active
///   tenant). Without it a subject cannot mint a coord device JWT from the
///   pairing the spawn copies in, and coord-facing features read as "not
///   enrolled". It names the MACHINE, which the subject genuinely shares.
/// - `profiles.json` — the box's backend profile (coord URL, API URL). Without
///   it a subject's coord base falls to its tier default instead of the box's
///   configured coordinator.
///
/// Deliberately NOT copied: every credential the subject can mint for itself
/// (`coord-device-jwt`, agent JWTs, loopback handshake keys — those are
/// per-port and the subject writes its own under `<root>/home`), and every
/// ledger or registry (session names, capability state, touch ledger) — those
/// are the harness's history, not the subject's.
pub const PROVISIONED_HOME_FILES: [&str; 2] = ["machine.json", "profiles.json"];

/// One supervisor-spawned runner's instance root and the locations under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRoot {
    root: PathBuf,
}

impl InstanceRoot {
    /// The instance root of the runner `runner_id`:
    /// [`super::instance_config_dir`]. `None` exactly when that is `None` (no
    /// resolvable config dir, or a degenerate id) — callers must treat it as a
    /// hard failure, never fall back to a shared dir.
    pub fn for_runner(runner_id: &str) -> Option<Self> {
        super::instance_config_dir(runner_id).map(Self::at)
    }

    /// An instance root at an explicit path (tests).
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The root itself — the value exported as `QONTINUI_INSTANCE_ROOT`.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// `QONTINUI_SECURE_STORAGE_DIR`: the root itself (see the module docs).
    /// Where the pairing (`paired_user.json`) and token cache
    /// (`auth_tokens.enc`) live — the ONE answer for the env block, the
    /// spawn-test paired-state writer and `POST /runners/pair-with-token`.
    pub fn secure_storage_dir(&self) -> PathBuf {
        self.root.clone()
    }

    /// `QONTINUI_CONFIG_DIR`: the root itself (see the module docs).
    pub fn config_dir(&self) -> PathBuf {
        self.root.clone()
    }

    /// `QONTINUI_HOME` — the subject's `~/.qontinui`: `<root>/home`.
    pub fn home_dir(&self) -> PathBuf {
        self.root.join("home")
    }

    /// `QONTINUI_RUNNER_LOG_DIR` and `QONTINUI_PANIC_LOG_DIR`: `<root>/logs`.
    /// The supervisor reads the startup panic log back from here.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `QONTINUI_EMBEDDED_PG_DIR`: `<root>/embedded-pg` — a private cluster,
    /// never the machine-shared one.
    pub fn embedded_pg_dir(&self) -> PathBuf {
        self.root.join("embedded-pg")
    }

    /// `WEBVIEW2_USER_DATA_FOLDER`: `<root>/webview`. Exported on every OS —
    /// only WebView2 reads the variable, and a root-honouring runner uses the
    /// same location for its own WebView data on every platform.
    pub fn webview_dir(&self) -> PathBuf {
        self.root.join("webview")
    }

    /// Every directory-valued variable the supervisor exports for this root, in
    /// one list — what the spawn sets and the tests check against
    /// [`ROOT_CONFINED_ENV_KEYS`].
    pub fn confined_exports(&self) -> [(&'static str, PathBuf); 7] {
        [
            ("QONTINUI_CONFIG_DIR", self.config_dir()),
            ("QONTINUI_SECURE_STORAGE_DIR", self.secure_storage_dir()),
            ("QONTINUI_HOME", self.home_dir()),
            ("QONTINUI_RUNNER_LOG_DIR", self.logs_dir()),
            ("QONTINUI_PANIC_LOG_DIR", self.logs_dir()),
            ("QONTINUI_EMBEDDED_PG_DIR", self.embedded_pg_dir()),
            ("WEBVIEW2_USER_DATA_FOLDER", self.webview_dir()),
        ]
    }
}

/// The secure-storage dir of the runner `runner_id` — where its pairing is read
/// from. Every supervisor writer of paired state resolves its destination here,
/// so a writer can never land a pairing somewhere the child does not read.
pub fn instance_secure_storage_dir(runner_id: &str) -> Option<PathBuf> {
    InstanceRoot::for_runner(runner_id).map(|r| r.secure_storage_dir())
}

/// The harness's own `~/.qontinui` — the provisioning SOURCE: `$QONTINUI_HOME`
/// when set non-blank (the runner's `ambient::qontinui_dir` rule), else
/// `<home>/.qontinui`.
pub fn harness_qontinui_dir() -> Option<PathBuf> {
    harness_qontinui_dir_from(std::env::var_os("QONTINUI_HOME"), dirs::home_dir())
}

/// Env-free core of [`harness_qontinui_dir`].
pub fn harness_qontinui_dir_from(
    override_dir: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    match override_dir {
        Some(dir) if !dir.to_string_lossy().trim().is_empty() => Some(PathBuf::from(dir)),
        _ => home.map(|h| h.join(".qontinui")),
    }
}

/// Copy each of [`PROVISIONED_HOME_FILES`] that exists in `source` into
/// `dest_home` (created if absent), skipping any the destination already holds
/// — a persistent named runner keeps its own copy (it may have switched its
/// active tenant), and a fresh temp root has none.
///
/// Returns the basenames copied. A missing source (`None`, or a file absent
/// there) is not an error: the subject then runs without that file, exactly as
/// a fresh box would. `Err` names the failed copy.
pub fn provision_instance_home(
    source: Option<&Path>,
    dest_home: &Path,
) -> Result<Vec<&'static str>, String> {
    std::fs::create_dir_all(dest_home)
        .map_err(|e| format!("could not create {}: {e}", dest_home.display()))?;
    let Some(source) = source else {
        return Ok(Vec::new());
    };
    let mut copied = Vec::new();
    for file in PROVISIONED_HOME_FILES {
        let src = source.join(file);
        let dst = dest_home.join(file);
        if !src.is_file() || dst.exists() {
            continue;
        }
        std::fs::copy(&src, &dst)
            .map_err(|e| format!("copy {} -> {} failed: {e}", src.display(), dst.display()))?;
        copied.push(file);
    }
    Ok(copied)
}

/// True when `path` lies lexically inside `root` (the root itself counts). A
/// path carrying `..` is never inside. Lexical on purpose — the runner's check
/// is lexical too, and this answer only decides whether to WARN.
pub fn is_inside_root(path: &Path, root: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        && path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confined_exports_cover_only_confined_keys_and_lie_inside_the_root() {
        let root = InstanceRoot::at(PathBuf::from("/r/com.qontinui.runner/instances/test-1"));
        for (key, value) in root.confined_exports() {
            assert!(
                ROOT_CONFINED_ENV_KEYS.contains(&key),
                "{key} is exported as a root location but is not a root-confined key"
            );
            assert!(
                is_inside_root(&value, root.path()),
                "{key}={value:?} must lie inside {:?}",
                root.path()
            );
        }
    }

    #[test]
    fn secure_storage_and_config_are_the_root_itself() {
        let root = InstanceRoot::at(PathBuf::from("/r/instances/named-1"));
        assert_eq!(root.secure_storage_dir(), root.path());
        assert_eq!(root.config_dir(), root.path());
    }

    #[test]
    fn instance_secure_storage_dir_is_the_runner_root() {
        if dirs::config_dir().is_none() {
            return;
        }
        assert_eq!(
            instance_secure_storage_dir("test-9877"),
            super::super::instance_config_dir("test-9877")
        );
        assert_eq!(instance_secure_storage_dir(""), None);
    }

    #[test]
    fn is_inside_root_is_lexical_and_refuses_parent_dirs() {
        let root = Path::new("/a/root");
        assert!(is_inside_root(Path::new("/a/root"), root));
        assert!(is_inside_root(Path::new("/a/root/logs"), root));
        assert!(!is_inside_root(Path::new("/a/rootx"), root));
        assert!(!is_inside_root(Path::new("/a/root/../elsewhere"), root));
        assert!(!is_inside_root(Path::new("relative/logs"), root));
    }

    #[test]
    fn harness_qontinui_dir_prefers_a_non_blank_override() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            harness_qontinui_dir_from(None, home.clone()),
            Some(PathBuf::from("/home/u/.qontinui"))
        );
        assert_eq!(
            harness_qontinui_dir_from(Some("  ".into()), home.clone()),
            Some(PathBuf::from("/home/u/.qontinui"))
        );
        assert_eq!(
            harness_qontinui_dir_from(Some("/elsewhere".into()), home),
            Some(PathBuf::from("/elsewhere"))
        );
    }

    #[test]
    fn provision_copies_only_the_listed_files_and_never_overwrites() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let source = tmp.path().join("harness-home");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("machine.json"), b"{\"device_id\":\"d\"}").unwrap();
        std::fs::write(source.join("profiles.json"), b"{}").unwrap();
        // Secrets and ledgers the subject must NOT inherit.
        std::fs::write(source.join("coord-device-jwt"), b"secret").unwrap();
        std::fs::write(source.join("runner-loopback-key-9876"), b"secret").unwrap();

        let dest = tmp.path().join("root").join("home");
        let copied = provision_instance_home(Some(&source), &dest).expect("provision");
        assert_eq!(copied, vec!["machine.json", "profiles.json"]);
        assert!(!dest.join("coord-device-jwt").exists());
        assert!(!dest.join("runner-loopback-key-9876").exists());

        // A subject that rewrote its copy keeps it on the next spawn.
        std::fs::write(dest.join("machine.json"), b"{\"device_id\":\"mine\"}").unwrap();
        let again = provision_instance_home(Some(&source), &dest).expect("provision");
        assert!(again.is_empty());
        assert_eq!(
            std::fs::read(dest.join("machine.json")).unwrap(),
            b"{\"device_id\":\"mine\"}"
        );
    }

    #[test]
    fn provision_without_a_source_still_creates_the_home() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dest = tmp.path().join("home");
        assert!(provision_instance_home(None, &dest).unwrap().is_empty());
        assert!(dest.is_dir());
    }
}
