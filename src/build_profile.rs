//! Which cargo profile a runner binary is built in, and how that profile is
//! chosen for the PRIMARY runner.
//!
//! Plan `2026-10-19-primary-runner-is-a-debug-build-and-nothing-shows-it`,
//! Phase 5. The primary serves every live session on the box, so it is built
//! in the optimized `release-checked` profile (`inherits = "release"`, with
//! `debug-assertions` and `overflow-checks` kept ON, so every
//! `cfg(debug_assertions)` door and the std UB tripwire behave exactly as in a
//! dev build — the ONLY variable is optimization). Temp, named and slot builds
//! are the iteration loop and stay on the dev profile, byte for byte.
//!
//! **Why one enum, threaded everywhere, instead of a `"release"` literal.** The
//! supervisor resolved every artifact through `.join("debug")`, and the one time
//! a path was pointed at another profile by hand (`runner_exe_candidates`) a
//! `spawn-test {rebuild:true}` rebuilt debug and then silently launched a stale
//! release binary. A profile is therefore a VALUE that the build args, the
//! artifact directory, the sidecar placement, the LKG location and the exe copy
//! path are all derived from — so they cannot disagree.
//!
//! **Layout.** The primary is built into its OWN target dir,
//! `target-pool/primary/` ([`PRIMARY_TARGET_DIR_NAME`]), never into a pool slot,
//! so no slot ever holds two profiles and every slot routine
//! (`pick_slot_for_resolution`, slot freshness, fingerprints, the sidecar
//! verdict) keeps its one-profile `debug/` assumption.

use std::path::{Path, PathBuf};

/// Directory under `target-pool/` that the primary's optimized builds use as
/// their `CARGO_TARGET_DIR`. A sibling of `slot-N/` and `lkg/`, deliberately
/// NOT named `slot-*`: the slot scans (`build_sizing::built_slot_ids`,
/// `slot_dirs_beyond_pool`) key on that prefix, and this dir is not a slot.
pub const PRIMARY_TARGET_DIR_NAME: &str = "primary";

/// The id the primary build dir carries where the build pipeline needs a slot
/// id (log lines, `BuildAttempt`, the LKG record's `source_slot`).
///
/// `u8::MAX` so it survives the `u8` narrowing `StaleBinary.slot_id` applies,
/// and far outside any real pool: the derived pool is capped at
/// [`crate::build_sizing::DERIVED_POOL_SIZE_CAP`] slots. It is never an index
/// into `BuildPool::slots`, and a build on it never becomes
/// `last_successful_slot`.
pub const PRIMARY_BUILD_SLOT_ID: usize = u8::MAX as usize;

/// Env var equivalent of `--primary-build-profile`. The flag wins.
pub const PRIMARY_BUILD_PROFILE_ENV: &str = "QONTINUI_SUPERVISOR_PRIMARY_BUILD_PROFILE";

/// A cargo profile the supervisor builds the runner in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RunnerBuildProfile {
    /// cargo's `dev` profile (opt-level 0). Output dir `debug/`.
    #[serde(rename = "dev")]
    Dev,
    /// The runner workspace's `[profile.release-checked]`: release
    /// optimization with debug assertions on. Output dir `release-checked/`.
    #[serde(rename = "release-checked")]
    ReleaseChecked,
}

impl RunnerBuildProfile {
    /// Every profile, for callers that must recognise an artifact of ANY
    /// profile (the orphan scan's root set, a primary's identity paths).
    pub const ALL: [RunnerBuildProfile; 2] =
        [RunnerBuildProfile::Dev, RunnerBuildProfile::ReleaseChecked];

    /// The profile's cargo name — the value `--profile` takes and the one
    /// spelling of the opt-in (`dev`) at every layer.
    pub fn cargo_name(self) -> &'static str {
        match self {
            RunnerBuildProfile::Dev => "dev",
            RunnerBuildProfile::ReleaseChecked => "release-checked",
        }
    }

    /// The directory cargo writes this profile's artifacts to, under a target
    /// dir. cargo spells the dev profile's directory `debug`, not `dev`.
    pub fn dir_name(self) -> &'static str {
        match self {
            RunnerBuildProfile::Dev => "debug",
            RunnerBuildProfile::ReleaseChecked => "release-checked",
        }
    }

    /// The args that select this profile on a `cargo build` command line.
    /// Empty for `dev`, which is cargo's default for `build`.
    pub fn cargo_args(self) -> &'static [&'static str] {
        match self {
            RunnerBuildProfile::Dev => &[],
            RunnerBuildProfile::ReleaseChecked => &["--profile", "release-checked"],
        }
    }

    /// Parse the operator-facing spelling (`dev` or `release-checked`,
    /// case-insensitive, surrounding whitespace ignored). `None` for anything
    /// else — including `debug`: the opt-in is named ONCE, as `dev`.
    pub fn parse(value: &str) -> Option<RunnerBuildProfile> {
        match value.trim().to_ascii_lowercase().as_str() {
            "dev" => Some(RunnerBuildProfile::Dev),
            "release-checked" => Some(RunnerBuildProfile::ReleaseChecked),
            _ => None,
        }
    }

    /// This profile's artifact directory under `target_dir`.
    pub fn artifact_dir(self, target_dir: &Path) -> PathBuf {
        target_dir.join(self.dir_name())
    }

    /// The runner exe this profile produces under `target_dir`.
    pub fn runner_exe(self, target_dir: &Path) -> PathBuf {
        self.artifact_dir(target_dir)
            .join(crate::config::RUNNER_BIN_NAME)
    }
}

impl std::fmt::Display for RunnerBuildProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.cargo_name())
    }
}

/// clap value parser for `--primary-build-profile`. An unrecognised value is a
/// usage error at startup — the operator typed it just now.
pub fn parse_profile_arg(value: &str) -> Result<RunnerBuildProfile, String> {
    RunnerBuildProfile::parse(value).ok_or_else(|| {
        format!("unrecognised build profile {value:?} (accepted: dev, release-checked)")
    })
}

/// Where the configured primary profile came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryProfileSource {
    /// Nothing chose it: the `release-checked` default.
    Default,
    /// `--primary-build-profile`.
    Flag,
    /// [`PRIMARY_BUILD_PROFILE_ENV`].
    Env,
}

impl PrimaryProfileSource {
    /// The operator-facing name of the knob, for announcement lines.
    pub fn knob(self) -> &'static str {
        match self {
            PrimaryProfileSource::Default => "default",
            PrimaryProfileSource::Flag => "--primary-build-profile",
            PrimaryProfileSource::Env => PRIMARY_BUILD_PROFILE_ENV,
        }
    }
}

/// The CONFIGURED build profile of the primary runner, and why.
///
/// Configured, not observed: what the running primary was actually built in is
/// read from the runner's own `/health` `buildProfile` and served beside this
/// value, so the two can visibly disagree (an unattended start that fell back
/// to a dev LKG).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PrimaryBuildProfile {
    pub profile: RunnerBuildProfile,
    pub source: PrimaryProfileSource,
    /// An env value that was set but is not a profile name, and was therefore
    /// IGNORED (the default applies). Kept so the startup announcement can say
    /// so rather than silently discarding the operator's intent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored_env_value: Option<String>,
}

impl Default for PrimaryBuildProfile {
    fn default() -> Self {
        PrimaryBuildProfile {
            profile: RunnerBuildProfile::ReleaseChecked,
            source: PrimaryProfileSource::Default,
            ignored_env_value: None,
        }
    }
}

impl PrimaryBuildProfile {
    /// Resolve from the parsed flag and the raw env value. The flag wins; a
    /// blank env value counts as unset.
    ///
    /// An UNPARSEABLE env value does not stop the supervisor: it is recorded in
    /// [`Self::ignored_env_value`] and the default applies, because a typo in an
    /// exported variable must not leave a box with no supervisor and so no
    /// primary. The announcement names what was ignored.
    pub fn resolve(flag: Option<RunnerBuildProfile>, env: Option<String>) -> PrimaryBuildProfile {
        if let Some(profile) = flag {
            return PrimaryBuildProfile {
                profile,
                source: PrimaryProfileSource::Flag,
                ignored_env_value: None,
            };
        }
        let env = env.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        match env {
            None => PrimaryBuildProfile::default(),
            Some(raw) => match RunnerBuildProfile::parse(&raw) {
                Some(profile) => PrimaryBuildProfile {
                    profile,
                    source: PrimaryProfileSource::Env,
                    ignored_env_value: None,
                },
                None => PrimaryBuildProfile {
                    ignored_env_value: Some(raw),
                    ..PrimaryBuildProfile::default()
                },
            },
        }
    }

    /// Is the primary configured as a debug build — the opt-in that permits a
    /// primary to start from a `debug/` artifact?
    pub fn dev_opted_in(&self) -> bool {
        self.profile == RunnerBuildProfile::Dev
    }

    /// The one INFO line the supervisor prints at start:
    /// `primary build profile: release-checked (default)` or
    /// `primary build profile: dev (opt-in via <source>)`.
    pub fn announce_line(&self) -> String {
        let mut line = match (self.profile, self.source) {
            (RunnerBuildProfile::ReleaseChecked, PrimaryProfileSource::Default) => {
                "primary build profile: release-checked (default)".to_string()
            }
            (RunnerBuildProfile::Dev, source) => {
                format!("primary build profile: dev (opt-in via {})", source.knob())
            }
            (RunnerBuildProfile::ReleaseChecked, source) => format!(
                "primary build profile: release-checked (via {})",
                source.knob()
            ),
        };
        if let Some(raw) = &self.ignored_env_value {
            line.push_str(&format!(
                "; IGNORED unrecognised {PRIMARY_BUILD_PROFILE_ENV}={raw:?} (accepted: dev, \
                 release-checked)"
            ));
        }
        line
    }

    /// The WARN that accompanies a dev opt-in, naming its cost. `None` for an
    /// optimized primary.
    pub fn opt_in_warning(&self) -> Option<String> {
        self.dev_opted_in().then(|| {
            format!(
                "DEBUG PRIMARY (opt-in via {}): the primary runner is built and started at \
                 opt-level 0, so every terminal byte it serves runs unoptimized",
                self.source.knob()
            )
        })
    }
}

/// The profile the primary runner is built and resolved in.
pub fn primary_profile(config: &crate::config::SupervisorConfig) -> RunnerBuildProfile {
    config.primary_build_profile.profile
}

/// `<npm_dir>/target-pool/primary` — the primary's dedicated target dir.
pub fn primary_target_dir(runner_npm_dir: &Path) -> PathBuf {
    runner_npm_dir
        .join("target-pool")
        .join(PRIMARY_TARGET_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_names_are_the_directories_cargo_writes() {
        assert_eq!(RunnerBuildProfile::Dev.dir_name(), "debug");
        assert_eq!(
            RunnerBuildProfile::ReleaseChecked.dir_name(),
            "release-checked"
        );
    }

    #[test]
    fn cargo_args_select_the_profile_and_dev_adds_nothing() {
        let empty: &[&str] = &[];
        assert_eq!(RunnerBuildProfile::Dev.cargo_args(), empty);
        assert_eq!(
            RunnerBuildProfile::ReleaseChecked.cargo_args(),
            &["--profile", "release-checked"]
        );
    }

    #[test]
    fn cargo_names_and_serde_spell_the_opt_in_dev() {
        assert_eq!(RunnerBuildProfile::Dev.cargo_name(), "dev");
        assert_eq!(
            RunnerBuildProfile::ReleaseChecked.cargo_name(),
            "release-checked"
        );
        assert_eq!(
            serde_json::to_string(&RunnerBuildProfile::Dev).unwrap(),
            "\"dev\""
        );
        assert_eq!(
            serde_json::to_string(&RunnerBuildProfile::ReleaseChecked).unwrap(),
            "\"release-checked\""
        );
        assert_eq!(
            serde_json::from_str::<RunnerBuildProfile>("\"release-checked\"").unwrap(),
            RunnerBuildProfile::ReleaseChecked
        );
    }

    #[test]
    fn parse_accepts_only_the_two_names() {
        assert_eq!(
            RunnerBuildProfile::parse("dev"),
            Some(RunnerBuildProfile::Dev)
        );
        assert_eq!(
            RunnerBuildProfile::parse(" Release-Checked "),
            Some(RunnerBuildProfile::ReleaseChecked)
        );
        // The opt-in is named once: `debug` is the DIRECTORY, not the name.
        assert_eq!(RunnerBuildProfile::parse("debug"), None);
        assert_eq!(RunnerBuildProfile::parse("release"), None);
        assert_eq!(RunnerBuildProfile::parse(""), None);
        assert!(parse_profile_arg("fast").is_err());
    }

    #[test]
    fn runner_exe_lands_in_the_profile_dir() {
        let target = Path::new("/ws/qontinui-runner/target-pool/primary");
        let exe = RunnerBuildProfile::ReleaseChecked.runner_exe(target);
        assert!(
            exe.ends_with(format!(
                "target-pool/primary/release-checked/{}",
                crate::config::RUNNER_BIN_NAME
            )),
            "{exe:?}"
        );
        let slot = Path::new("/ws/qontinui-runner/target-pool/slot-0");
        assert!(RunnerBuildProfile::Dev
            .runner_exe(slot)
            .ends_with(format!("slot-0/debug/{}", crate::config::RUNNER_BIN_NAME)));
    }

    #[test]
    fn primary_target_dir_is_target_pool_primary() {
        assert_eq!(
            primary_target_dir(Path::new("/ws/qontinui-runner")),
            Path::new("/ws/qontinui-runner/target-pool/primary")
        );
    }

    #[test]
    fn primary_build_slot_id_fits_a_u8_and_is_outside_the_pool() {
        assert_eq!(PRIMARY_BUILD_SLOT_ID, 255);
        assert!(PRIMARY_BUILD_SLOT_ID > crate::build_sizing::DERIVED_POOL_SIZE_CAP as usize);
    }

    #[test]
    fn default_is_release_checked() {
        let p = PrimaryBuildProfile::resolve(None, None);
        assert_eq!(p.profile, RunnerBuildProfile::ReleaseChecked);
        assert_eq!(p.source, PrimaryProfileSource::Default);
        assert!(!p.dev_opted_in());
        assert_eq!(
            p.announce_line(),
            "primary build profile: release-checked (default)"
        );
        assert_eq!(p.opt_in_warning(), None);
    }

    #[test]
    fn flag_dev_is_an_announced_opt_in_and_beats_the_env() {
        let p = PrimaryBuildProfile::resolve(
            Some(RunnerBuildProfile::Dev),
            Some("release-checked".into()),
        );
        assert_eq!(p.profile, RunnerBuildProfile::Dev);
        assert_eq!(p.source, PrimaryProfileSource::Flag);
        assert!(p.dev_opted_in());
        assert_eq!(
            p.announce_line(),
            "primary build profile: dev (opt-in via --primary-build-profile)"
        );
        assert!(p
            .opt_in_warning()
            .unwrap()
            .starts_with("DEBUG PRIMARY (opt-in via --primary-build-profile)"));
    }

    #[test]
    fn env_dev_selects_dev() {
        let p = PrimaryBuildProfile::resolve(None, Some("dev".into()));
        assert_eq!(p.profile, RunnerBuildProfile::Dev);
        assert_eq!(p.source, PrimaryProfileSource::Env);
        assert_eq!(
            p.announce_line(),
            "primary build profile: dev (opt-in via QONTINUI_SUPERVISOR_PRIMARY_BUILD_PROFILE)"
        );
    }

    #[test]
    fn blank_env_is_unset() {
        assert_eq!(
            PrimaryBuildProfile::resolve(None, Some("   ".into())),
            PrimaryBuildProfile::default()
        );
    }

    #[test]
    fn unrecognised_env_is_ignored_loudly_not_fatal() {
        let p = PrimaryBuildProfile::resolve(None, Some("debug".into()));
        assert_eq!(p.profile, RunnerBuildProfile::ReleaseChecked);
        assert_eq!(p.source, PrimaryProfileSource::Default);
        assert_eq!(p.ignored_env_value.as_deref(), Some("debug"));
        assert_eq!(
            p.announce_line(),
            "primary build profile: release-checked (default); IGNORED unrecognised \
             QONTINUI_SUPERVISOR_PRIMARY_BUILD_PROFILE=\"debug\" (accepted: dev, release-checked)"
        );
    }

    #[test]
    fn explicit_release_checked_names_its_source() {
        let p = PrimaryBuildProfile::resolve(None, Some("release-checked".into()));
        assert_eq!(
            p.announce_line(),
            "primary build profile: release-checked (via QONTINUI_SUPERVISOR_PRIMARY_BUILD_PROFILE)"
        );
    }
}
