//! Build-pool "slot territory": who owns a cargo/rustc process, and what the
//! slot cleanup actually did about it.
//!
//! This module is deliberately **cross-platform** even though its only
//! production caller is the Windows slot reaper. Two reasons:
//!
//! 1. **CI runs on `ubuntu-latest` only.** While the kill predicate lived in
//!    `process::windows` (a `#[cfg(target_os = "windows")]` module) its unit
//!    tests were compiled out of every CI run and had never actually executed
//!    there. The predicate decides whether a peer agent's mid-compile build
//!    gets killed — that is exactly the logic that must be covered by the gate
//!    that blocks merges.
//! 2. **`diagnostics` is cross-platform** and needs [`SlotMatch`] /
//!    [`KillMethod`] in its event payloads. A platform-gated home for those
//!    types would not compile on Linux.
//!
//! The same reasoning is why the sweep's **accounting** lives here too
//! ([`classify_sweep`] / [`triage_kill`]): `process::windows` stays a thin
//! sysinfo-driven adapter that produces [`SweepCandidate`]s, and every
//! killed/failed/spared bucketing decision is a pure function CI actually runs.
//!
//! Nothing here may import from `process::windows` or from any platform-gated
//! module — everything is pure `std` + `serde` (plus the equally cross-platform
//! `crate::pii_scrub`).

// JUSTIFIED ALLOW — this is the flip side of being cross-platform on purpose.
// The only PRODUCTION caller of everything below is `process::windows`, which is
// `#[cfg(target_os = "windows")]`. On Linux these items compile (that is the
// point — CI must type-check and TEST them) but have no non-test caller, and in
// the BINARY crate `pub` does not exempt an item from `dead_code`: `main.rs`
// declares a private `mod process`, so `cargo clippy -- -D warnings` on
// `ubuntu-latest` would fail on every function and type in this file. The lib
// target is unaffected (pub-reachable from the crate root), which is exactly why
// the asymmetry is invisible when checking on Windows.
//
// This suppresses only the platform-asymmetry noise: the module's unit tests
// still compile and RUN on Linux, so a genuine regression here still fails the
// gate. Same pattern, same reason as `process::orphan_scan`'s module-level
// `cfg_attr(not(target_os = "windows"), allow(unused))`.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use crate::pii_scrub::{key_is_sensitive, sanitize_value, REDACTED_VALUE};
use serde::Serialize;
use std::ffi::OsString;
use std::path::Path;

/// Max characters retained from a process command line before truncation.
const CMD_SNIPPET_MAX_CHARS: usize = 160;

/// Max pids rendered inline in a cleanup summary line, and max pids carried in
/// the `killed_pids` / `spared_pids` vectors of the `BuildCleanupSummary`
/// diagnostics event.
///
/// ONE cap for both surfaces on purpose: the log line and the event are read
/// side by side, and two different caps would make them silently disagree about
/// which pids "the first few" are.
///
/// Deliberately DIFFERENT from `build_monitor::MAX_KILL_EVENTS_PER_PASS` (10),
/// which is read right next to it: that one bounds a **per-process event
/// stream** (one addressable `BuildProcessKilled` / `BuildKillFailed` per
/// process, where per-pid detail is the whole point), while this one bounds a
/// **summary sample** carried alongside a true count on a single event. Unifying
/// the two would either flood the ring or throw away kill detail; only the two
/// surfaces that render the SAME sample (the log line and the summary event's
/// pid vectors) must share a cap, and they do.
const PID_LIST_CAP: usize = 5;

/// Argv flags whose FOLLOWING token (or `=`-RHS) is a live credential.
///
/// Matched after stripping leading dashes and lowercasing, so `--token` and
/// `-token` both hit. Deliberately whole-token (not substring) on the flag
/// side: `--target` must not match `--token`.
const SENSITIVE_FLAG_NAMES: &[&str] = &[
    "token",
    "password",
    "passwd",
    "secret",
    "credential",
    "credentials",
    "api-key",
    "api_key",
    "apikey",
    "registry-token",
];

/// Which evidence attributed a process to a build-pool slot.
///
/// Recorded on every kill so the telemetry can distinguish the strong signal
/// (`Env` — an exact `CARGO_TARGET_DIR` match) from the weaker fallback
/// (`Argv` — a slot path found in the command line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotMatch {
    /// The process environment sets `CARGO_TARGET_DIR` to the slot dir.
    Env,
    /// An argv token names the slot dir (e.g. rustc's `--out-dir`).
    Argv,
}

/// How a process was actually terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KillMethod {
    /// sysinfo's native `Process::kill()`.
    Sysinfo,
    /// `taskkill` fallback, used when the native kill was refused.
    Taskkill,
}

/// Why the cleanup left a cargo/rustc process alone.
///
/// Recording the spared set is only auditable if the record can tell the two
/// cases apart. `NotOurs` is the boring, safe case (a peer's build with a
/// different `CARGO_TARGET_DIR`); `Unclassifiable` is the SAFE-DEGRADE case,
/// where sysinfo could read neither the environ nor the argv — typically an
/// elevated or other-user process. Only the second one is a signal that the
/// reaper is flying blind, and it is the one an operator greps for when a build
/// keeps failing on a locked artifact nobody appears to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SparedReason {
    /// Attributed to somebody else — a peer worktree build, a sibling pool
    /// slot, an operator's manual cargo.
    NotOurs,
    /// Neither environ nor argv was readable, so ownership could not be decided
    /// at all. Not a kill (unknown is never a kill), but not "provably not
    /// ours" either.
    Unclassifiable,
}

impl SparedReason {
    /// Decide the reason from what sysinfo could actually read.
    ///
    /// Both empty ⇒ the predicate had nothing to work with ⇒
    /// [`SparedReason::Unclassifiable`]. Anything readable ⇒ the predicate ran
    /// over the evidence it had and said no ⇒ [`SparedReason::NotOurs`].
    ///
    /// PRECISION NOTE: `NotOurs` means "at least one net ran and rejected it",
    /// which is NOT the same strength of claim in every case.
    /// [`process_targets_slot`] has two nets — the strong `CARGO_TARGET_DIR`
    /// environ compare and the weak argv-path scan — and this function cannot
    /// see which of them was available. So an "argv readable, environ
    /// unreadable" process is labelled `NotOurs` on the strength of the **weak
    /// argv net alone**: nothing proved it is not ours, only that no argv token
    /// named the slot. That is still the correct disposition (unknown is never a
    /// kill, and sparing is the safe side), but do not read `not_ours` off the
    /// wire as "proven foreign" — read it as "not attributed to this slot by the
    /// evidence that was readable".
    pub fn classify(environ: &[OsString], cmd: &[OsString]) -> Self {
        if environ.is_empty() && cmd.is_empty() {
            Self::Unclassifiable
        } else {
            Self::NotOurs
        }
    }
}

/// One process the slot cleanup terminated.
#[derive(Debug, Clone, Serialize)]
pub struct KilledProcess {
    pub pid: u32,
    pub process_name: String,
    pub cmd_snippet: String,
    pub matched_by: SlotMatch,
    pub method: KillMethod,
}

/// One process the cleanup attributed to this slot, tried to kill, and FAILED
/// to kill on every path.
///
/// This is the third bucket. Without it such a process landed in neither
/// `killed` nor `spared`, [`SlotCleanupReport::is_empty`] stayed true, and the
/// pass emitted no log line and no diagnostics event whatsoever — the build
/// then failed on a locked artifact with zero telemetry, which is precisely the
/// diagnosis-blind case this telemetry exists to eliminate.
#[derive(Debug, Clone, Serialize)]
pub struct FailedKill {
    pub pid: u32,
    pub process_name: String,
    pub matched_by: SlotMatch,
    /// Operator-facing reason: which path refused and what it said.
    pub detail: String,
}

/// One cargo/rustc process the slot cleanup deliberately left alone because
/// [`process_targets_slot`] did not attribute it to this slot.
///
/// This is the half of the picture that used to be invisible: a "quiet" pass
/// that killed nothing looked identical to a pass that spared a peer agent's
/// in-flight build. Recording the spared set — WITH the [`SparedReason`] that
/// separates "provably not ours" from "could not be classified" — makes the
/// safe-degrade path auditable — see [`SparedReason::classify`] for exactly how
/// strong a claim `not_ours` is (it is "no readable evidence attributed it to
/// this slot", not "proven foreign").
#[derive(Debug, Clone, Serialize)]
pub struct SparedProcess {
    pub pid: u32,
    pub cmd_snippet: String,
    pub reason: SparedReason,
}

/// What a single `cleanup_orphaned_slot_processes` pass did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SlotCleanupReport {
    pub killed: Vec<KilledProcess>,
    pub spared: Vec<SparedProcess>,
    pub failed: Vec<FailedKill>,
}

/// The outcome of the kill sequence for one in-territory process, normalized so
/// the accounting can be a pure function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    /// The process is gone, via this mechanism.
    Killed(KillMethod),
    /// Every kill path was tried and the process is (as far as we know) still
    /// alive. The string is the operator-facing detail.
    Failed(String),
}

/// What the `taskkill` fallback reported, once the platform specifics are
/// stripped off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackOutcome {
    /// taskkill reported success.
    Killed,
    /// taskkill ran and refused (e.g. "Access is denied").
    Refused,
    /// taskkill could not be run at all; the string is the error.
    Unavailable(String),
}

/// Fold the native-kill result and the optional fallback result into a single
/// [`KillOutcome`].
///
/// The `Ok(true)` / `Ok(false)` / `Err` triage used to live inline in the
/// Windows reaper, where CI could never execute it — and its `Ok(false)` and
/// `Err` arms dropped the process on the floor. Pure and platform-free here so
/// `ubuntu-latest` covers it.
pub fn triage_kill(sysinfo_killed: bool, fallback: Option<FallbackOutcome>) -> KillOutcome {
    if sysinfo_killed {
        return KillOutcome::Killed(KillMethod::Sysinfo);
    }
    match fallback {
        Some(FallbackOutcome::Killed) => KillOutcome::Killed(KillMethod::Taskkill),
        Some(FallbackOutcome::Refused) => KillOutcome::Failed(
            "sysinfo kill returned false and the taskkill fallback refused the kill \
             (typically an elevated or other-user process: \"Access is denied\")"
                .to_string(),
        ),
        Some(FallbackOutcome::Unavailable(e)) => KillOutcome::Failed(format!(
            "sysinfo kill returned false and the taskkill fallback could not be run: {e}"
        )),
        None => KillOutcome::Failed(
            "sysinfo kill returned false and no taskkill fallback was attempted".to_string(),
        ),
    }
}

/// What the sweep decided about one enumerated cargo/rustc process.
#[derive(Debug, Clone)]
pub enum SweepDisposition {
    /// Attributed to this slot; the adapter ran the kill sequence and got
    /// `outcome`.
    Targeted {
        matched_by: SlotMatch,
        outcome: KillOutcome,
    },
    /// Not reaped, for `reason`.
    Spared { reason: SparedReason },
}

/// One enumerated cargo/rustc process, already classified (and, when in
/// territory, already put through the kill sequence) by the platform adapter.
#[derive(Debug, Clone)]
pub struct SweepCandidate {
    pub pid: u32,
    pub process_name: String,
    pub cmd_snippet: String,
    pub disposition: SweepDisposition,
}

/// Bucket every candidate into the report's three vectors.
///
/// The whole point of extracting this: a process that failed BOTH kill paths
/// must land in `failed` rather than vanishing from the report. `windows.rs`
/// stays a thin adapter over sysinfo + taskkill and owns no accounting.
pub fn classify_sweep<I>(candidates: I) -> SlotCleanupReport
where
    I: IntoIterator<Item = SweepCandidate>,
{
    let mut report = SlotCleanupReport::default();
    for c in candidates {
        match c.disposition {
            SweepDisposition::Spared { reason } => report.spared.push(SparedProcess {
                pid: c.pid,
                cmd_snippet: c.cmd_snippet,
                reason,
            }),
            SweepDisposition::Targeted {
                matched_by,
                outcome: KillOutcome::Killed(method),
            } => report.killed.push(KilledProcess {
                pid: c.pid,
                process_name: c.process_name,
                cmd_snippet: c.cmd_snippet,
                matched_by,
                method,
            }),
            SweepDisposition::Targeted {
                matched_by,
                outcome: KillOutcome::Failed(detail),
            } => report.failed.push(FailedKill {
                pid: c.pid,
                process_name: c.process_name,
                matched_by,
                detail,
            }),
        }
    }
    report
}

/// Decide whether a cargo/rustc process is building into `slot_dir` — i.e. it
/// belongs to THIS supervisor's build pool and is ours to reap.
///
/// The supervisor spawns every pool build with
/// `.env("CARGO_TARGET_DIR", &slot.target_dir)` (see
/// `build_monitor::run_build_inner`), and the rustc children cargo spawns
/// inherit that env AND receive an explicit `--out-dir <slot_dir>/debug/deps`
/// in argv. So a process targets the slot iff its environment sets
/// `CARGO_TARGET_DIR` to `slot_dir` OR any argv token contains the `slot_dir`
/// path (case-insensitive, matching Windows path semantics).
///
/// The returned [`SlotMatch`] names WHICH signal fired; `None` means "no
/// readable evidence attributed it to this slot" — see
/// [`SparedReason::classify`] for how the caller then tells that from
/// "unclassifiable", and for why the two nets make `not_ours` a weaker claim
/// than its name suggests.
///
/// SAFE DEGRADE: if BOTH `environ` and `cmd` are empty (sysinfo could not read
/// them — common for another user's or an elevated process), returns `None`.
/// Unknown is NOT a kill: the previous "kill every cargo/rustc" behavior was the
/// unsafe degrade and it killed peer agents' worktree builds fleet-wide. A truly
/// stuck orphan we cannot classify is left for `free_slot_exe`'s exe-lock path,
/// which is already slot-scoped.
pub fn process_targets_slot(
    environ: &[OsString],
    cmd: &[OsString],
    slot_dir: &Path,
) -> Option<SlotMatch> {
    // Trim trailing separators so `slot-1` and `slot-1\` compare equal, and so
    // the boundary check below is exact.
    let needle = slot_dir
        .to_string_lossy()
        .to_ascii_lowercase()
        .trim_end_matches(['/', '\\'])
        .to_string();
    if needle.is_empty() {
        return None;
    }

    // CARGO_TARGET_DIR=<slot_dir> in the environment. cargo sets this for every
    // pool build and rustc children inherit it, so this branch fires for both
    // cargo.exe and rustc.exe and is an EXACT (not substring) compare.
    for kv in environ {
        let kv = kv.to_string_lossy();
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("CARGO_TARGET_DIR")
                && v.to_ascii_lowercase().trim_end_matches(['/', '\\']) == needle
            {
                return Some(SlotMatch::Env);
            }
        }
    }

    // Secondary net: a rustc argv token under the slot dir (e.g.
    // `--out-dir <slot>/debug/deps`), for the rare case sysinfo could not read
    // the environ. Boundary-aware so `slot-1` never matches `slot-10` — the
    // slot path must be the whole token or be followed by a path separator.
    let sep_back = format!("{needle}\\");
    let sep_fwd = format!("{needle}/");
    for arg in cmd {
        let a = arg.to_string_lossy().to_ascii_lowercase();
        if a == needle || a.contains(&sep_back) || a.contains(&sep_fwd) {
            return Some(SlotMatch::Argv);
        }
    }

    None
}

/// True when `tok` is a flag whose value is a credential.
fn is_sensitive_flag(tok: &str) -> bool {
    if !tok.starts_with('-') {
        return false;
    }
    let name = tok.trim_start_matches('-').to_ascii_lowercase();
    SENSITIVE_FLAG_NAMES.contains(&name.as_str())
}

/// Cargo GLOBAL options that consume the following token as their value.
///
/// Needed only to find the subcommand position: cargo's usage is
/// `cargo [+toolchain] [OPTIONS] [COMMAND]`, so a global option's VALUE sits in
/// what would otherwise look like the subcommand slot
/// (`cargo --color always login <tok>`), and mistaking that value for the
/// subcommand makes [`cargo_login_subcommand_index`] return `None` — i.e. it
/// leaks. Matched only in the exact separated spelling: `--color=always` and
/// `-Zfoo` carry their own value and are skipped by the plain leading-dash rule.
///
/// Deliberately SHORT and certain, and matched CASE-SENSITIVELY: a false entry
/// here is the dangerous direction (it would step over a real `login` token and
/// leak), and cargo's value-taking short options are capitalized (`-Z`, `-C`)
/// while lowercase `-c` is not a cargo global at all — lowercasing would
/// conflate them and turn a hypothetical boolean `-c` into a leak.
const CARGO_GLOBAL_VALUE_FLAGS: &[&str] = &["--color", "--config", "-Z", "-C"];

/// Index of the `login` SUBCOMMAND token, if this argv is a `cargo login`.
///
/// `cargo login <token>` puts a live crates.io credential in bare argv with no
/// flag to key off, so finding the subcommand is the whole game.
///
/// cargo's usage is `cargo [+toolchain] [OPTIONS] [COMMAND] [ARGS]`, so the walk
/// skips, in order: the toolchain selector (`+nightly`), every flag (any token
/// with a leading `-`), and the separate VALUE of the global options in
/// [`CARGO_GLOBAL_VALUE_FLAGS`]. The first surviving token is the subcommand.
///
/// Skipping flags is load-bearing, not cosmetic: with a `+`-only skip,
/// `cargo -q login <tok>` / `cargo --offline login <tok>` /
/// `cargo --config net.retry=3 login <tok>` all resolved the subcommand slot to
/// the FLAG, returned `None`, and let the credential through verbatim — the same
/// leak this function exists to close, one token further left.
///
/// RESIDUAL (documented, not fixed): a global option that takes a value and is
/// NOT in `CARGO_GLOBAL_VALUE_FLAGS` still shifts the subcommand slot onto its
/// value and yields `None`. Closing that completely means vendoring cargo's flag
/// table and re-syncing it every release, which trades one stale-data leak for
/// another; the short certain list plus the leading-dash skip covers the
/// spellings that actually appear.
///
/// This deliberately returns the SUBCOMMAND index rather than the secret's
/// index: see [`cmd_snippet`] for why every positional after it is redacted
/// instead of one computed position.
fn cargo_login_subcommand_index(tokens: &[String]) -> Option<usize> {
    let mut i = 1;
    while let Some(tok) = tokens.get(i) {
        if tok.starts_with('+') {
            i += 1;
            continue;
        }
        if tok.starts_with('-') {
            // A global option's separate value occupies the next slot, so step
            // over both. `--color=always` / `-Zfoo` carry their value inline and
            // are handled by the plain flag skip.
            i += if CARGO_GLOBAL_VALUE_FLAGS.contains(&tok.as_str()) {
                2
            } else {
                1
            };
            continue;
        }
        return tok.eq_ignore_ascii_case("login").then_some(i);
    }
    None
}

/// Redact `scheme://user:pass@host` down to `scheme://<redacted>@host`.
///
/// Covers `cargo install --git https://user:ghp_xxx@github.com/…`, where the
/// credential is inside a single argv token.
fn redact_url_userinfo(tok: &str) -> String {
    let Some(scheme_end) = tok.find("://") else {
        return tok.to_string();
    };
    let rest_start = scheme_end + 3;
    let rest = &tok[rest_start..];
    // Userinfo ends at the first '@' and cannot span past the authority, so any
    // '/', '?' or '#' before it means there is no userinfo.
    let Some(at) = rest.find('@') else {
        return tok.to_string();
    };
    let userinfo = &rest[..at];
    if userinfo.is_empty() || userinfo.contains(['/', '?', '#']) {
        return tok.to_string();
    }
    format!("{}{REDACTED_VALUE}{}", &tok[..rest_start], &rest[at..])
}

/// Redact the RHS of `--flag=value` / `key.path.token="value"` when the key
/// side is sensitive — at ANY `=` depth.
///
/// The key check is [`crate::pii_scrub::key_is_sensitive`] (substring-based),
/// so it also catches cargo's `--config 'registries.x.token="…"'` shape where
/// the sensitive name is buried in a dotted key.
///
/// **Why it walks every `=` and not just the first.** clap accepts the
/// `=`-joined spelling of every long flag, so `--config` carries its own
/// `key=value` payload in a single token:
/// `--config=registries.my-reg.token="s3cret"`. Splitting once put `--config`
/// (not sensitive) on the key side and handed the whole credential-bearing
/// payload back untouched — and nothing downstream caught it either
/// ([`crate::pii_scrub::sanitize_value`]'s dotted heuristic needs ≥4-char
/// segments, and `"my"` is 2). The space-separated spelling of the SAME flag was
/// covered and tested, which is what made the gap invisible.
///
/// Iterative rather than recursive on purpose: the depth is the number of `=`
/// in the token, and these tokens come from arbitrary third-party argv.
fn redact_kv(tok: &str) -> String {
    let mut prefix = String::new();
    let mut rest = tok;
    loop {
        let Some((key, value)) = rest.split_once('=') else {
            // No `=` layer had a sensitive key — leave the token verbatim.
            return tok.to_string();
        };
        if key_is_sensitive(key) {
            return format!("{prefix}{key}={REDACTED_VALUE}");
        }
        prefix.push_str(key);
        prefix.push('=');
        rest = value;
    }
}

/// Render a process command line as a bounded, log-safe, **redacted** snippet.
///
/// Argv tokens are joined with spaces and truncated to
/// [`CMD_SNIPPET_MAX_CHARS`] **characters** (not bytes) with a `…` marker.
/// Character-based truncation is load-bearing: cargo command lines routinely
/// carry non-ASCII paths, and slicing a `String` by byte index would panic
/// mid-codepoint inside the build path — the worst possible place to panic.
///
/// **Redaction runs BEFORE truncation**, deliberately: truncation must never be
/// what saves us: a secret that happens to sit past character 160 today moves
/// left tomorrow when a path shortens.
///
/// Why this matters at all: the `spared` path snapshots the argv of ARBITRARY
/// third-party cargo/rustc on a shared multi-agent box, and
/// [`SlotCleanupReport::summary_line`] writes several of them into
/// `supervisor.log`, which is append-only with no rotation. Live credentials
/// really do appear in cargo argv (`cargo publish --token …`, `cargo login …`,
/// `--config 'registries.x.token="…"'`, `--git https://user:ghp_x@github.com/…`).
///
/// Four layers, in order:
/// 1. the token FOLLOWING a sensitive flag (`--token`, `--password`, `--secret`, …);
/// 2. EVERY positional argument of a `cargo login`;
/// 3. URL userinfo, then the RHS of a sensitive `key=value` at any `=` depth;
/// 4. every surviving token through [`crate::pii_scrub::sanitize_value`] for
///    the `Bearer …` / JWT shapes.
///
/// **Why layer 2 redacts every positional and not "the token after `login`".**
/// The secret's position is not computable from argv alone. `cargo login --`
/// `<tok>` (the modern documented spelling) puts the end-of-flags separator
/// where the naive `login + 1` index points, so the separator got redacted and
/// the credential sailed through every other layer (no leading `-`, no `=`, not
/// a bearer/JWT shape) into the unrotated log. "First token with no leading
/// `-`" does not fix it either: in `cargo login --registry my-reg <tok>` that
/// rule names the registry NAME and still leaks the credential, because
/// deciding which flags consume a following value would mean hard-coding
/// cargo's flag table and re-breaking on every cargo release. So the rule is
/// positional-blind: after a `login` subcommand, any token that is not a flag
/// (does not start with `-` — which also leaves a bare `--` visible) is
/// redacted. The over-redaction costs at most a registry name; the alternative
/// costs a live crates.io credential.
pub fn cmd_snippet(cmd: &[OsString]) -> String {
    let raw: Vec<String> = cmd
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let login_idx = cargo_login_subcommand_index(&raw);

    let mut redact_next = false;
    let mut tokens: Vec<String> = Vec::with_capacity(raw.len());
    for (i, tok) in raw.iter().enumerate() {
        let is_login_positional =
            login_idx.is_some_and(|sub| i > sub) && !tok.starts_with('-') && !tok.is_empty();
        if redact_next || is_login_positional {
            redact_next = false;
            tokens.push(REDACTED_VALUE.to_string());
            continue;
        }
        if is_sensitive_flag(tok) {
            redact_next = true;
            tokens.push(tok.clone());
            continue;
        }
        let cleaned = redact_kv(&redact_url_userinfo(tok));
        tokens.push(sanitize_value(&cleaned));
    }

    let joined = tokens.join(" ");
    let mut truncated: String = joined.chars().take(CMD_SNIPPET_MAX_CHARS).collect();
    if truncated.chars().count() < joined.chars().count() {
        truncated.push('…');
    }
    truncated
}

impl SlotCleanupReport {
    /// True when the pass neither killed, spared, nor failed to kill anything —
    /// the common, genuinely quiet case, which callers use to stay silent.
    ///
    /// `failed` is part of this by construction: a pass that TRIED and failed is
    /// the single most important pass to report, and leaving it out of the gate
    /// silenced it completely.
    pub fn is_empty(&self) -> bool {
        self.killed.is_empty() && self.spared.is_empty() && self.failed.is_empty()
    }

    /// How many spared processes could not be classified at all (the
    /// safe-degrade count). Surfaced on the summary event so an operator can
    /// grep for a reaper that is flying blind.
    pub fn unclassifiable_count(&self) -> usize {
        self.spared
            .iter()
            .filter(|s| s.reason == SparedReason::Unclassifiable)
            .count()
    }

    /// First [`PID_LIST_CAP`] killed pids, for the summary event's attributable
    /// kill evidence. The true count travels alongside as `killed`.
    pub fn killed_pids(&self) -> Vec<u32> {
        self.killed
            .iter()
            .take(PID_LIST_CAP)
            .map(|k| k.pid)
            .collect()
    }

    /// First [`PID_LIST_CAP`] spared pids. A COUNT alone cannot prove a
    /// specific foreign build survived — unrelated sibling-slot rustc processes
    /// satisfy it — so the verification harness needs the pids themselves.
    pub fn spared_pids(&self) -> Vec<u32> {
        self.spared
            .iter()
            .take(PID_LIST_CAP)
            .map(|s| s.pid)
            .collect()
    }

    /// The one-line operator-facing summary of a cleanup pass.
    ///
    /// Reports ALL THREE counts (a pass that killed nothing but spared three
    /// peer builds is a very different event from a pass that saw nothing at
    /// all, and a pass that FAILED to kill an in-territory process is different
    /// again), names the unclassifiable subset, and lists the spared and failed
    /// processes capped at [`PID_LIST_CAP`] with an explicit overflow count so a
    /// busy box cannot flood the log.
    ///
    /// Pure so it is unit-testable — that is why it lives here rather than
    /// inline in the Windows reaper.
    pub fn summary_line(&self, slot_id: usize, territory: &Path) -> String {
        let mut line = format!(
            "slot {} cleanup ({}): killed {}, spared {} ({} unclassifiable), failed {}",
            slot_id,
            territory.display(),
            self.killed.len(),
            self.spared.len(),
            self.unclassifiable_count(),
            self.failed.len(),
        );

        if !self.spared.is_empty() {
            let shown = self
                .spared
                .iter()
                .take(PID_LIST_CAP)
                .map(render_spared)
                .collect::<Vec<_>>()
                .join(", ");
            line = format!("{line} out-of-territory cargo/rustc process(es): [{shown}]");
            let overflow = self.spared.len().saturating_sub(PID_LIST_CAP);
            if overflow > 0 {
                line = format!("{line} … and {overflow} more");
            }
        }

        if !self.failed.is_empty() {
            let shown = self
                .failed
                .iter()
                .take(PID_LIST_CAP)
                .map(|f| format!("pid {} {} ({})", f.pid, f.process_name, f.detail))
                .collect::<Vec<_>>()
                .join(", ");
            line = format!("{line} UNREAPED in-territory process(es): [{shown}]");
            let overflow = self.failed.len().saturating_sub(PID_LIST_CAP);
            if overflow > 0 {
                line = format!("{line} … and {overflow} more");
            }
        }

        line
    }
}

/// Render one spared entry for the summary line.
///
/// An unclassifiable process has no readable argv, so `cmd_snippet` is empty —
/// rendering it as `pid 1234 ` (bare pid, trailing space, nothing after it) told
/// the operator nothing and looked like a truncation bug. Name the reason
/// instead, and never emit a dangling separator.
fn render_spared(s: &SparedProcess) -> String {
    match (s.reason, s.cmd_snippet.trim().is_empty()) {
        (SparedReason::Unclassifiable, _) => format!("pid {} <unclassifiable>", s.pid),
        (SparedReason::NotOurs, true) => format!("pid {} <no cmdline>", s.pid),
        (SparedReason::NotOurs, false) => format!("pid {} {}", s.pid, s.cmd_snippet),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_sweep, cmd_snippet, process_targets_slot, triage_kill, FailedKill,
        FallbackOutcome, KillMethod, KillOutcome, KilledProcess, SlotCleanupReport, SlotMatch,
        SparedProcess, SparedReason, SweepCandidate, SweepDisposition, REDACTED_VALUE,
    };
    use std::ffi::OsString;
    use std::path::Path;

    fn os(v: &str) -> OsString {
        OsString::from(v)
    }

    fn spared(pid: u32) -> SparedProcess {
        SparedProcess {
            pid,
            cmd_snippet: format!("cargo build --pid-{pid}"),
            reason: SparedReason::NotOurs,
        }
    }

    fn killed(pid: u32) -> KilledProcess {
        KilledProcess {
            pid,
            process_name: "cargo.exe".to_string(),
            cmd_snippet: "cargo build".to_string(),
            matched_by: SlotMatch::Env,
            method: KillMethod::Sysinfo,
        }
    }

    fn failed(pid: u32) -> FailedKill {
        FailedKill {
            pid,
            process_name: "rustc.exe".to_string(),
            matched_by: SlotMatch::Argv,
            detail: "access denied".to_string(),
        }
    }

    fn candidate(pid: u32, disposition: SweepDisposition) -> SweepCandidate {
        SweepCandidate {
            pid,
            process_name: "cargo.exe".to_string(),
            cmd_snippet: format!("cargo build --pid-{pid}"),
            disposition,
        }
    }

    /// A cargo process whose env sets `CARGO_TARGET_DIR` to the slot dir is ours.
    #[test]
    fn targets_slot_via_cargo_target_dir_env() {
        let slot = Path::new(r"D:\qontinui-root\qontinui-runner\target-pool\slot-0");
        let environ = vec![
            os("PATH=C:\\bin"),
            os(r"CARGO_TARGET_DIR=D:\qontinui-root\qontinui-runner\target-pool\slot-0"),
        ];
        assert_eq!(
            process_targets_slot(&environ, &[], slot),
            Some(SlotMatch::Env)
        );
    }

    /// Case-insensitive path match + a trailing separator must not defeat it.
    #[test]
    fn targets_slot_case_and_trailing_sep_insensitive() {
        let slot = Path::new(r"D:\qontinui-root\qontinui-runner\target-pool\slot-1");
        let environ = vec![os(
            r"cargo_target_dir=d:\QONTINUI-ROOT\qontinui-runner\target-pool\slot-1\",
        )];
        assert_eq!(
            process_targets_slot(&environ, &[], slot),
            Some(SlotMatch::Env)
        );
    }

    /// A rustc argv `--out-dir <slot>/debug/deps` is ours even without env, and
    /// is attributed to the WEAKER `Argv` signal.
    #[test]
    fn targets_slot_via_rustc_out_dir_argv() {
        let slot = Path::new(r"D:\qontinui-root\qontinui-runner\target-pool\slot-2");
        let cmd = vec![
            os("rustc.exe"),
            os("--out-dir"),
            os(r"D:\qontinui-root\qontinui-runner\target-pool\slot-2\debug\deps"),
        ];
        assert_eq!(process_targets_slot(&[], &cmd, slot), Some(SlotMatch::Argv));
    }

    /// A PEER agent's worktree build (different target dir) is NOT ours — this
    /// is the whole point of the fix; the old code killed it.
    #[test]
    fn does_not_target_peer_worktree_build() {
        let slot = Path::new(r"D:\qontinui-root\qontinui-runner\target-pool\slot-0");
        let environ = vec![os(
            r"CARGO_TARGET_DIR=D:\qontinui-root\qontinui-runner-utf8\target",
        )];
        let cmd = vec![
            os("rustc.exe"),
            os("--out-dir"),
            os(r"D:\qontinui-root\qontinui-runner-utf8\target\debug\deps"),
        ];
        assert_eq!(process_targets_slot(&environ, &cmd, slot), None);
    }

    /// Another pool slot's build is NOT reaped by this slot's cleanup (fixes the
    /// concurrent-slot self-sabotage).
    #[test]
    fn does_not_target_sibling_pool_slot() {
        let slot = Path::new(r"D:\qontinui-root\qontinui-runner\target-pool\slot-0");
        let environ = vec![os(
            r"CARGO_TARGET_DIR=D:\qontinui-root\qontinui-runner\target-pool\slot-1",
        )];
        assert_eq!(process_targets_slot(&environ, &[], slot), None);
    }

    /// Boundary: a cleanup for `slot-1` must NOT match `slot-10`'s argv (the
    /// `slot-1` string is a prefix of `slot-10`). Guards the pool-size ≥ 11 case.
    #[test]
    fn slot_1_does_not_match_slot_10_argv() {
        let slot = Path::new(r"D:\qontinui-root\qontinui-runner\target-pool\slot-1");
        let cmd = vec![
            os("rustc.exe"),
            os("--out-dir"),
            os(r"D:\qontinui-root\qontinui-runner\target-pool\slot-10\debug\deps"),
        ];
        assert_eq!(process_targets_slot(&[], &cmd, slot), None);
    }

    /// SAFE DEGRADE: unreadable env AND cmd ⇒ not ours ⇒ do not kill. This is
    /// the inversion of the old "kill everything" default and the reason the fix
    /// can never again take out a peer build it failed to classify.
    #[test]
    fn empty_environ_and_cmd_is_not_a_kill() {
        let slot = Path::new(r"D:\qontinui-root\qontinui-runner\target-pool\slot-0");
        assert_eq!(process_targets_slot(&[], &[], slot), None);
    }

    /// The safe-degrade case is `Unclassifiable`; anything readable that the
    /// predicate rejected is `NotOurs`. Conflating them is what made the spared
    /// record unauditable.
    #[test]
    fn spared_reason_separates_unclassifiable_from_not_ours() {
        assert_eq!(
            SparedReason::classify(&[], &[]),
            SparedReason::Unclassifiable
        );
        assert_eq!(
            SparedReason::classify(&[os("PATH=/bin")], &[]),
            SparedReason::NotOurs
        );
        assert_eq!(
            SparedReason::classify(&[], &[os("cargo"), os("build")]),
            SparedReason::NotOurs
        );
    }

    /// Short command lines pass through untouched — no spurious ellipsis.
    #[test]
    fn cmd_snippet_leaves_short_command_lines_intact() {
        let cmd = vec![os("cargo.exe"), os("build"), os("--release")];
        assert_eq!(cmd_snippet(&cmd), "cargo.exe build --release");
        assert!(cmd_snippet(&[]).is_empty());
    }

    /// Long command lines are cut at 160 CHARACTERS with an explicit marker.
    #[test]
    fn cmd_snippet_truncates_at_160_chars() {
        let long = "a".repeat(400);
        let snippet = cmd_snippet(&[os(&long)]);
        assert_eq!(
            snippet.chars().count(),
            161,
            "160 retained chars plus the ellipsis marker"
        );
        assert!(snippet.ends_with('…'));
    }

    /// Multi-byte paths must not panic and must still cut at a char boundary.
    /// Byte-index slicing here is a panic in the middle of a build cleanup.
    #[test]
    fn cmd_snippet_is_char_boundary_safe_on_multibyte_input() {
        // 'é' is 2 bytes; 400 of them is 800 bytes, so a naive `&s[..160]`
        // would land mid-codepoint and panic.
        let multibyte = "é".repeat(400);
        let snippet = cmd_snippet(&[os(&multibyte)]);
        assert_eq!(snippet.chars().count(), 161);
        assert!(snippet.starts_with('é'));
        assert!(snippet.ends_with('…'));
    }

    // ---------------------------------------------------------------------
    // Credential redaction. The `spared` path snapshots THIRD-PARTY argv into
    // an unrotated log file, so these are the real leak cases.
    // ---------------------------------------------------------------------

    /// `cargo publish --token <crates.io-token>` — the flagged-value form.
    #[test]
    fn cmd_snippet_redacts_value_after_a_sensitive_flag() {
        let snippet = cmd_snippet(&[os("cargo"), os("publish"), os("--token"), os("abc123")]);
        assert_eq!(snippet, "cargo publish --token <redacted>");
        assert!(!snippet.contains("abc123"));
    }

    /// The `--token=abc` form must be redacted too — same secret, different
    /// spelling, and the flagged-value walk alone would miss it.
    #[test]
    fn cmd_snippet_redacts_the_equals_form() {
        let snippet = cmd_snippet(&[os("cargo"), os("publish"), os("--token=abc123")]);
        assert_eq!(snippet, "cargo publish --token=<redacted>");
        assert!(!snippet.contains("abc123"));
    }

    /// URL userinfo (`cargo install --git https://user:ghp_x@github.com/…`).
    /// The host must survive — it is the useful half — and the credential must
    /// not.
    #[test]
    fn cmd_snippet_redacts_url_userinfo() {
        let snippet = cmd_snippet(&[
            os("cargo"),
            os("install"),
            os("--git"),
            os("https://u:p@host"),
        ]);
        assert_eq!(snippet, "cargo install --git https://<redacted>@host");

        // A URL with no userinfo is untouched.
        assert_eq!(
            cmd_snippet(&[os("https://github.com/x/y")]),
            "https://github.com/x/y"
        );
    }

    /// `cargo login <tok>` has no flag to key off — the secret is positional.
    /// A later `login` (e.g. `--bin login`) is NOT the subcommand and must not
    /// swallow the following token.
    #[test]
    fn cmd_snippet_redacts_cargo_login_positional_only_at_the_subcommand() {
        assert_eq!(
            cmd_snippet(&[os("cargo"), os("login"), os("sekrit")]),
            "cargo login <redacted>"
        );
        assert_eq!(
            cmd_snippet(&[os("cargo"), os("build"), os("--bin"), os("login"), os("x")]),
            "cargo build --bin login x"
        );
    }

    /// REGRESSION (leak): `cargo login -- <tok>` is the modern documented
    /// spelling, and the old `login + 1` index landed on the `--` separator —
    /// redacting the SEPARATOR while the live credential one token later fell
    /// through every remaining layer (no leading `-`, no `=`, not a bearer/JWT
    /// shape) verbatim into the unrotated log.
    #[test]
    fn cmd_snippet_redacts_cargo_login_secret_after_a_double_dash_separator() {
        let snippet = cmd_snippet(&[os("cargo"), os("login"), os("--"), os("ghp_liveToken")]);
        assert!(!snippet.contains("ghp_liveToken"), "{snippet}");
        assert_eq!(snippet, "cargo login -- <redacted>");
    }

    /// REGRESSION (leak): the same hole with a flag in front of the secret.
    ///
    /// This is also why the rule is "every positional" rather than "the first
    /// token with no leading `-`": that rule names `my-reg` here and leaves the
    /// credential untouched.
    #[test]
    fn cmd_snippet_redacts_cargo_login_secret_after_a_registry_flag() {
        let snippet = cmd_snippet(&[
            os("cargo"),
            os("login"),
            os("--registry"),
            os("my-reg"),
            os("ghp_liveToken"),
        ]);
        assert!(!snippet.contains("ghp_liveToken"), "{snippet}");
        assert_eq!(snippet, "cargo login --registry <redacted> <redacted>");

        // The `=`-joined flag spelling keeps the flag readable and still
        // redacts the trailing credential.
        let joined = cmd_snippet(&[
            os("cargo"),
            os("login"),
            os("--registry=my-reg"),
            os("ghp_liveToken"),
        ]);
        assert!(!joined.contains("ghp_liveToken"), "{joined}");
        assert_eq!(joined, "cargo login --registry=my-reg <redacted>");
    }

    /// REGRESSION (leak): cargo's usage is `cargo [+toolchain] [OPTIONS]
    /// [COMMAND]`, so GLOBAL options legally precede the subcommand. A
    /// subcommand scan that skipped only `+toolchain` resolved the subcommand
    /// slot to the FLAG, concluded "not a login", and let the credential through
    /// verbatim — the same leak as the `--`/`--registry` cases, one token
    /// further left.
    #[test]
    fn cmd_snippet_redacts_cargo_login_behind_global_flags() {
        for argv in [
            vec!["cargo", "-q", "login", "ghp_liveToken"],
            vec!["cargo", "--offline", "login", "ghp_liveToken"],
            vec!["cargo", "--frozen", "--offline", "login", "ghp_liveToken"],
            // A global option with a SEPARATE value: the value occupies what
            // would otherwise be the subcommand slot, so both must be stepped
            // over.
            vec!["cargo", "--config", "net.retry=3", "login", "ghp_liveToken"],
            vec!["cargo", "--color", "always", "login", "ghp_liveToken"],
            // The `=`-joined spelling carries its value inline — the plain flag
            // skip handles it.
            vec!["cargo", "--config=net.retry=3", "login", "ghp_liveToken"],
            // Toolchain selector AND global flags together.
            vec![
                "cargo",
                "+nightly",
                "--offline",
                "login",
                "--",
                "ghp_liveToken",
            ],
        ] {
            let owned: Vec<OsString> = argv.iter().map(|s| os(s)).collect();
            let snippet = cmd_snippet(&owned);
            assert!(
                !snippet.contains("ghp_liveToken"),
                "credential survived for argv {argv:?}: {snippet}"
            );
            assert!(
                snippet.contains(REDACTED_VALUE),
                "nothing was redacted for argv {argv:?}: {snippet}"
            );
        }
    }

    /// The flag-skipping subcommand scan must not turn a NON-login cargo
    /// invocation into one. `cargo --config <k=v> build` is the shape the dotted
    /// -config redaction test uses, and `cargo build --bin login x` is the
    /// original false-positive guard — neither may start redacting positionals.
    #[test]
    fn cmd_snippet_flag_skip_does_not_invent_a_login_subcommand() {
        assert_eq!(
            cmd_snippet(&[
                os("cargo"),
                os("--offline"),
                os("build"),
                os("--bin"),
                os("login"),
                os("x")
            ]),
            "cargo --offline build --bin login x"
        );
        assert_eq!(
            cmd_snippet(&[
                os("cargo"),
                os("--color"),
                os("always"),
                os("build"),
                os("release-artifact")
            ]),
            "cargo --color always build release-artifact"
        );
    }

    /// The plain `cargo login <tok>` form (which always worked) must keep
    /// working after the positional rule changed.
    #[test]
    fn cmd_snippet_still_redacts_the_bare_cargo_login_form() {
        assert_eq!(
            cmd_snippet(&[os("cargo"), os("login"), os("ghp_liveToken")]),
            "cargo login <redacted>"
        );
        // A toolchain selector before the subcommand does not shift the rule.
        assert_eq!(
            cmd_snippet(&[
                os("cargo"),
                os("+nightly"),
                os("login"),
                os("--"),
                os("ghp_liveToken")
            ]),
            "cargo +nightly login -- <redacted>"
        );
    }

    /// cargo's `--config 'registries.x.token="…"'` buries the sensitive name in
    /// a dotted key; the substring key check catches it.
    #[test]
    fn cmd_snippet_redacts_dotted_config_token_key() {
        let snippet = cmd_snippet(&[
            os("cargo"),
            os("--config"),
            os("registries.my-reg.token=\"s3cret\""),
            os("build"),
        ]);
        assert!(!snippet.contains("s3cret"), "{snippet}");
        assert!(
            snippet.contains("registries.my-reg.token=<redacted>"),
            "{snippet}"
        );
    }

    /// REGRESSION (leak): the `=`-JOINED spelling of the same flag.
    ///
    /// clap accepts `--config=<k>=<v>` exactly as it accepts `--config <k>=<v>`,
    /// but the old single-split `redact_kv` put `--config` (not sensitive) on
    /// the key side and returned the credential-bearing payload verbatim.
    /// `sanitize_value` does not save it either: its dotted-token heuristic
    /// requires ≥4-char segments and `"my"` is two. The space-separated form was
    /// covered and tested, which is exactly what hid this.
    #[test]
    fn cmd_snippet_redacts_equals_joined_config_token_key() {
        let snippet = cmd_snippet(&[
            os("cargo"),
            os("--config=registries.my-reg.token=\"s3cret\""),
            os("build"),
        ]);
        assert!(!snippet.contains("s3cret"), "{snippet}");
        assert_eq!(
            snippet,
            "cargo --config=registries.my-reg.token=<redacted> build"
        );
    }

    /// The `=`-walk must not turn every multi-`=` token into a redaction: a
    /// token whose keys are all boring survives verbatim.
    #[test]
    fn cmd_snippet_leaves_non_sensitive_multi_equals_tokens_intact() {
        assert_eq!(
            cmd_snippet(&[os("--config=build.jobs=8"), os("PATH=/bin:/usr/bin")]),
            "--config=build.jobs=8 PATH=/bin:/usr/bin"
        );
    }

    /// Bearer / JWT shapes anywhere in argv go through `pii_scrub`.
    #[test]
    fn cmd_snippet_redacts_jwt_shaped_tokens() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signaturepart";
        let snippet = cmd_snippet(&[os("some-tool"), os(jwt)]);
        assert_eq!(snippet, "some-tool <redacted>");
    }

    /// Redaction must happen BEFORE truncation: a secret that sits past the
    /// 160-char cut today moves left tomorrow, and "the truncator ate it" is not
    /// a security control.
    #[test]
    fn cmd_snippet_redacts_before_truncating() {
        let padding = "x".repeat(300);
        let snippet = cmd_snippet(&[os("cargo"), os(&padding), os("--token"), os("sekrit-value")]);
        assert!(!snippet.contains("sekrit-value"), "{snippet}");
        // And the redaction is genuinely doing the work: the secret is past the
        // truncation point, so the un-redacted string would also have hidden it
        // — assert on the pre-truncation behaviour with a short pad instead.
        let short = cmd_snippet(&[os("cargo"), os("--token"), os("sekrit-value"), os(&padding)]);
        assert!(short.contains("--token <redacted>"), "{short}");
        assert!(!short.contains("sekrit-value"), "{short}");
    }

    // ---------------------------------------------------------------------
    // Kill accounting — the triage that used to live (untested) inside the
    // Windows-only reaper.
    // ---------------------------------------------------------------------

    /// The native kill wins outright; no fallback is consulted.
    #[test]
    fn triage_sysinfo_success_is_a_sysinfo_kill() {
        assert_eq!(
            triage_kill(true, None),
            KillOutcome::Killed(KillMethod::Sysinfo)
        );
    }

    /// sysinfo refused, taskkill worked ⇒ killed, attributed to taskkill.
    #[test]
    fn triage_fallback_success_is_a_taskkill_kill() {
        assert_eq!(
            triage_kill(false, Some(FallbackOutcome::Killed)),
            KillOutcome::Killed(KillMethod::Taskkill)
        );
    }

    /// BOTH paths failed ⇒ `Failed`, never silently dropped. `Refused`
    /// (taskkill's "Access is denied", i.e. `Ok(false)`) and `Unavailable`
    /// (taskkill could not run at all, i.e. `Err`) are distinct details.
    #[test]
    fn triage_both_paths_failed_is_a_failure_with_detail() {
        match triage_kill(false, Some(FallbackOutcome::Refused)) {
            KillOutcome::Failed(d) => assert!(d.contains("refused"), "{d}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        match triage_kill(
            false,
            Some(FallbackOutcome::Unavailable("spawn ENOENT".into())),
        ) {
            KillOutcome::Failed(d) => assert!(d.contains("spawn ENOENT"), "{d}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        match triage_kill(false, None) {
            KillOutcome::Failed(d) => assert!(d.contains("no taskkill fallback"), "{d}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// The pure sweep classifier buckets all four dispositions, and — the
    /// regression this whole extraction exists for — a both-paths-failed process
    /// lands in `failed` instead of vanishing from the report.
    #[test]
    fn classify_sweep_buckets_all_four_dispositions() {
        let report = classify_sweep(vec![
            candidate(
                1,
                SweepDisposition::Targeted {
                    matched_by: SlotMatch::Env,
                    outcome: KillOutcome::Killed(KillMethod::Sysinfo),
                },
            ),
            candidate(
                2,
                SweepDisposition::Targeted {
                    matched_by: SlotMatch::Argv,
                    outcome: KillOutcome::Killed(KillMethod::Taskkill),
                },
            ),
            candidate(
                3,
                SweepDisposition::Targeted {
                    matched_by: SlotMatch::Env,
                    outcome: KillOutcome::Failed("access denied".to_string()),
                },
            ),
            candidate(
                4,
                SweepDisposition::Spared {
                    reason: SparedReason::NotOurs,
                },
            ),
            candidate(
                5,
                SweepDisposition::Spared {
                    reason: SparedReason::Unclassifiable,
                },
            ),
        ]);

        assert_eq!(report.killed.len(), 2);
        assert_eq!(report.killed[0].pid, 1);
        assert_eq!(report.killed[0].method, KillMethod::Sysinfo);
        assert_eq!(report.killed[1].pid, 2);
        assert_eq!(report.killed[1].method, KillMethod::Taskkill);
        assert_eq!(report.killed[1].matched_by, SlotMatch::Argv);

        assert_eq!(
            report.failed.len(),
            1,
            "a both-paths-failed kill is REPORTED"
        );
        assert_eq!(report.failed[0].pid, 3);
        assert_eq!(report.failed[0].matched_by, SlotMatch::Env);
        assert_eq!(report.failed[0].detail, "access denied");

        assert_eq!(report.spared.len(), 2);
        assert_eq!(report.spared[0].reason, SparedReason::NotOurs);
        assert_eq!(report.spared[1].reason, SparedReason::Unclassifiable);
        assert_eq!(report.unclassifiable_count(), 1);

        assert!(!report.is_empty());
        assert_eq!(report.killed_pids(), vec![1, 2]);
        assert_eq!(report.spared_pids(), vec![4, 5]);
    }

    /// A pass whose ONLY content is a failed kill must still be reported — the
    /// exact case that previously produced no log line and no diagnostics event
    /// while the build went on to fail on the locked artifact.
    #[test]
    fn a_failed_only_pass_is_not_quiet() {
        let report = classify_sweep(vec![candidate(
            9,
            SweepDisposition::Targeted {
                matched_by: SlotMatch::Env,
                outcome: KillOutcome::Failed("access denied".to_string()),
            },
        )]);
        assert!(!report.is_empty(), "a failed-kill pass is NOT a quiet pass");
        let line = report.summary_line(1, Path::new(r"D:\pool\slot-1"));
        assert!(line.contains("failed 1"), "{line}");
        assert!(line.contains("UNREAPED"), "{line}");
        assert!(line.contains("pid 9"), "{line}");
    }

    /// A pass that did nothing is empty; anything in any bucket is not.
    #[test]
    fn is_empty_is_true_only_when_every_vec_is_empty() {
        assert!(SlotCleanupReport::default().is_empty());

        assert!(!SlotCleanupReport {
            killed: vec![killed(1)],
            ..Default::default()
        }
        .is_empty());

        assert!(!SlotCleanupReport {
            spared: vec![spared(2)],
            ..Default::default()
        }
        .is_empty());

        assert!(!SlotCleanupReport {
            failed: vec![failed(3)],
            ..Default::default()
        }
        .is_empty());
    }

    /// The summary reports every count and the slot id — a pass that killed
    /// nothing but spared peers is a distinct, reportable event.
    #[test]
    fn summary_line_reports_every_count_and_slot_id() {
        let report = SlotCleanupReport {
            killed: vec![killed(11), killed(12)],
            spared: vec![spared(21), spared(22), spared(23)],
            failed: vec![],
        };
        let line = report.summary_line(7, Path::new(r"D:\pool\slot-7"));
        assert!(line.contains("slot 7"), "{line}");
        assert!(line.contains("killed 2"), "{line}");
        assert!(line.contains("spared 3"), "{line}");
        assert!(line.contains("0 unclassifiable"), "{line}");
        assert!(line.contains("failed 0"), "{line}");
        assert!(line.contains(r"D:\pool\slot-7"), "{line}");
        assert!(line.contains("pid 21"), "{line}");
        assert!(!line.contains("more"), "no overflow marker below the cap");
    }

    /// An unclassifiable spared process has no argv, so it must render its
    /// REASON rather than a bare `pid 1234 ` with a dangling trailing space.
    #[test]
    fn summary_line_names_unclassifiable_entries_without_a_dangling_separator() {
        let report = SlotCleanupReport {
            spared: vec![SparedProcess {
                pid: 1234,
                cmd_snippet: String::new(),
                reason: SparedReason::Unclassifiable,
            }],
            ..Default::default()
        };
        let line = report.summary_line(0, Path::new(r"D:\pool\slot-0"));
        assert!(line.contains("1 unclassifiable"), "{line}");
        assert!(line.contains("[pid 1234 <unclassifiable>]"), "{line}");
        assert!(
            !line.contains("pid 1234 ]"),
            "no empty trailing space before the list terminator: {line}"
        );
    }

    /// The spared list is capped at 5 and the overflow count is NAMED, so a
    /// busy box cannot turn one cleanup pass into a wall of log.
    #[test]
    fn summary_line_caps_spared_list_and_names_overflow() {
        let report = SlotCleanupReport {
            spared: (1..=8).map(spared).collect(),
            ..Default::default()
        };
        let line = report.summary_line(0, Path::new(r"D:\pool\slot-0"));
        for pid in 1..=5 {
            assert!(line.contains(&format!("pid {pid} ")), "{line}");
        }
        for pid in 6..=8 {
            assert!(
                !line.contains(&format!("pid {pid} ")),
                "pid {pid} is past the cap and must not be rendered: {line}"
            );
        }
        assert!(line.contains("… and 3 more"), "{line}");
        assert!(
            line.contains("spared 8"),
            "the true count is still reported"
        );
        assert_eq!(
            report.spared_pids(),
            vec![1, 2, 3, 4, 5],
            "the event's pid vector uses the SAME cap as the log line"
        );
    }

    // ---------------------------------------------------------------------
    // Wire shape the Phase 3 verification harness depends on.
    //
    // `scripts/verify-scoped-cleanup.ps1` is a Windows-only harness and CI is
    // `ubuntu-latest` only, so it can NEVER run on the merge gate. These tests
    // are the CI-enforced half for the types that live HERE: they pin the exact
    // serialized strings the harness string-matches on, so a rename fails the
    // build instead of silently turning the harness into a no-op that always
    // passes. The harness also reads `slot_id` / `territory` and the
    // `{timestamp, kind, data}` envelope, which belong to
    // `DiagnosticEventKind` — those are pinned by the sibling tests in
    // `crate::diagnostics`, not here.
    // ---------------------------------------------------------------------

    /// `matched_by` must serialize as exactly `"env"` / `"argv"`.
    ///
    /// The V2 harness assertion is a literal string compare against `"env"` on
    /// the `build_process_killed` payload — renaming these variants would leave
    /// the harness green while asserting nothing.
    #[test]
    fn slot_match_serializes_as_env_and_argv() {
        assert_eq!(
            serde_json::to_value(SlotMatch::Env).unwrap(),
            serde_json::json!("env")
        );
        assert_eq!(
            serde_json::to_value(SlotMatch::Argv).unwrap(),
            serde_json::json!("argv")
        );
    }

    /// `method` must serialize as exactly `"sysinfo"` / `"taskkill"` — the
    /// operator-facing distinction between "the native kill worked" and "we had
    /// to escalate", read straight off the diagnostics event.
    #[test]
    fn kill_method_serializes_as_sysinfo_and_taskkill() {
        assert_eq!(
            serde_json::to_value(KillMethod::Sysinfo).unwrap(),
            serde_json::json!("sysinfo")
        );
        assert_eq!(
            serde_json::to_value(KillMethod::Taskkill).unwrap(),
            serde_json::json!("taskkill")
        );
    }

    /// `reason` must serialize as exactly `"not_ours"` / `"unclassifiable"`.
    #[test]
    fn spared_reason_serializes_as_not_ours_and_unclassifiable() {
        assert_eq!(
            serde_json::to_value(SparedReason::NotOurs).unwrap(),
            serde_json::json!("not_ours")
        );
        assert_eq!(
            serde_json::to_value(SparedReason::Unclassifiable).unwrap(),
            serde_json::json!("unclassifiable")
        );
    }

    /// A killed entry must serialize with the exact field names the harness
    /// reads out of `build_process_killed.data`. The harness matches on `pid`,
    /// `matched_by` and (via the event) `territory`; a silent rename of any of
    /// them turns every V2 assertion into an unfindable-pid failure that reads
    /// like a product regression.
    #[test]
    fn killed_entry_serializes_with_the_field_names_the_harness_reads() {
        let value = serde_json::to_value(killed(4242)).unwrap();
        let obj = value.as_object().expect("KilledProcess is a JSON object");

        for key in ["pid", "process_name", "cmd_snippet", "matched_by", "method"] {
            assert!(obj.contains_key(key), "missing `{key}` in {value}");
        }
        assert_eq!(obj["pid"], serde_json::json!(4242));
        assert_eq!(obj["matched_by"], serde_json::json!("env"));
        assert_eq!(obj["method"], serde_json::json!("sysinfo"));
    }

    /// The spared + failed entries carry the field names the harness reads.
    #[test]
    fn spared_and_failed_entries_serialize_with_stable_field_names() {
        let spared_value = serde_json::to_value(spared(7)).unwrap();
        let spared_obj = spared_value.as_object().unwrap();
        for key in ["pid", "cmd_snippet", "reason"] {
            assert!(
                spared_obj.contains_key(key),
                "missing `{key}` in {spared_value}"
            );
        }
        assert_eq!(spared_obj["reason"], serde_json::json!("not_ours"));

        let failed_value = serde_json::to_value(failed(8)).unwrap();
        let failed_obj = failed_value.as_object().unwrap();
        for key in ["pid", "process_name", "matched_by", "detail"] {
            assert!(
                failed_obj.contains_key(key),
                "missing `{key}` in {failed_value}"
            );
        }
        assert_eq!(failed_obj["matched_by"], serde_json::json!("argv"));
    }

    /// Killed-but-nothing-spared renders the killed count and NO spared list.
    ///
    /// The new coverage here is the absent `out-of-territory` clause: the
    /// spared-list renderer must not emit an empty `[]` tail. The `!is_empty()`
    /// half restates `is_empty_is_true_only_when_every_vec_is_empty` above and
    /// is kept only to make the quiet-pass gate explicit at this call shape —
    /// that gate is `is_empty()`, NOT `spared == 0`, and a pass that reaped two
    /// orphans while seeing no out-of-territory cargo is the single most
    /// important pass to log.
    #[test]
    fn summary_line_reports_killed_count_with_zero_spared() {
        let report = SlotCleanupReport {
            killed: vec![killed(31), killed(32)],
            ..Default::default()
        };
        assert!(!report.is_empty(), "killed-only is NOT a quiet pass");

        let line = report.summary_line(2, Path::new(r"D:\pool\slot-2"));
        assert!(line.contains("killed 2"), "{line}");
        assert!(line.contains("spared 0"), "{line}");
        assert!(
            !line.contains("out-of-territory"),
            "no spared list when nothing was spared: {line}"
        );
        assert!(
            !line.contains("UNREAPED"),
            "no failed list when nothing failed: {line}"
        );
    }

    /// The slot dir reaches the operator-facing line verbatim.
    ///
    /// `summary_line` is the LOG surface; the harness reads the `territory`
    /// FIELD of the diagnostics event, not this text. What is new here over
    /// `summary_line_reports_every_count_and_slot_id` (which already asserts a
    /// backslash path survives) is the forward-slash case — what a non-Windows
    /// `Path::display()` produces, and therefore the only form CI itself ever
    /// exercises. It must not be separator-rewritten or shortened to a
    /// basename.
    #[test]
    fn summary_line_embeds_the_slot_territory_path_verbatim() {
        let territory = Path::new("/srv/qontinui-runner/target-pool/slot-2");
        let report = SlotCleanupReport {
            killed: vec![killed(41)],
            ..Default::default()
        };
        let line = report.summary_line(2, territory);
        assert!(
            line.contains("/srv/qontinui-runner/target-pool/slot-2"),
            "{line}"
        );
        assert!(
            line.contains("slot-2"),
            "the `slot-<id>` suffix the harness matches on: {line}"
        );
    }
}
