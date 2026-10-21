//! Machine-derived sizing for the supervisor's build pool: how many slots,
//! how many cargo jobs each slot runs, and the free-disk floor a build must
//! clear (plan
//! `2026-10-02-cargo-jobs-supervisor-build-pool-and-physical-disk-floors-follow-the-machine`,
//! Phases 2 and 4).
//!
//! **Why derived.** Before this module the pool was a constant 3 slots, every
//! slot's cargo ran with cargo's default of one job per CPU (so a full pool
//! asked for 3 × cores rustc jobs), and the disk floor was a constant 30 GB
//! whether a slot's target dir was 2 GB or 40. All three now follow the
//! machine, from ONE formula shared with the runner's CI lane
//! ([`qontinui_types::host_sizing`]).
//!
//! Everything here is pure except [`read_one_target_estimate`] /
//! [`write_one_target_estimate`] (a small JSON sidecar) — the host and disk
//! readings are passed in, so every branch is unit-testable.

use qontinui_types::host_sizing::{self, HostCapacity};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::config::DEFAULT_MIN_FREE_DISK_GB;

/// One gibibyte. The supervisor's floors are declared in "GB" and compared
/// against `n * 1024^3` bytes (see `build_monitor::GIB`); same unit here.
const GIB: u64 = 1024 * 1024 * 1024;

/// Env var that pins the pool size explicitly (unchanged from before this
/// module; it now overrides a DERIVED value instead of the constant 3).
pub const BUILD_POOL_SIZE_ENV: &str = "QONTINUI_SUPERVISOR_BUILD_POOL_SIZE";

/// Cargo's own job-count variable. When the SUPERVISOR's environment already
/// carries it, the operator chose a value and the supervisor leaves it alone
/// (its children inherit it).
pub const CARGO_BUILD_JOBS_ENV: &str = "CARGO_BUILD_JOBS";

/// **Deliberate cap on the DERIVED pool size: the pre-derivation default.**
///
/// The plan's formula is `max(1, min(cpu+memory term, disk term))`. Uncapped,
/// merytshost (48 cores / 368 GiB) derives 12 slots — 12 pre-warmed full
/// runner target dirs (~14 GiB each measured) for a lane that ran ZERO
/// builds across the plan's Phase 0 hour. Served policy `production-and-cost`
/// `spawns-outrank-ci-and-builds` (2026-10-21, after the plan was vetted)
/// puts background builds LAST on a saturated box, so the derivation may
/// LOWER the pool on a small machine but never raise it above what shipped.
/// The plan says the slot occupancy this phase records (`GET /health`
/// `build.pool.slots_busy`) is the evidence for a later raise; raising this
/// cap is that later, evidenced change. `QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`
/// still sets any value explicitly.
pub const DERIVED_POOL_SIZE_CAP: u32 = 3;

/// The disk floor never exceeds this fraction (1/N) of the volume's total
/// size, so a very large one-target estimate cannot demand an absurd floor.
pub const DISK_FLOOR_TOTAL_DIVISOR: u64 = 10;

/// File name (under the build pool root, `target-pool/`) of the persisted
/// one-target size estimate. Written by the footprint refresh; read at
/// startup so the pool derivation's disk term is not UNKNOWN on every boot.
pub const ONE_TARGET_ESTIMATE_FILE: &str = "one-target-estimate.json";

/// Where the pool size came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolSizeSource {
    /// Derived from this machine by [`derive_pool_size`].
    Derived,
    /// Set explicitly (`QONTINUI_SUPERVISOR_BUILD_POOL_SIZE`, or a test).
    Override,
}

/// The inputs and intermediate terms of a derived pool size, so `/health` can
/// show WHY the pool is the size it is. `None` on a term means UNKNOWN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PoolSizeDerivation {
    pub host_cpus: u32,
    pub host_mem_bytes: Option<u64>,
    /// `host_sizing::suggested_concurrent_builds(host)`. Unreadable memory
    /// makes this 1 (the shared rule's conservative fallback).
    pub cpu_memory_term: u32,
    /// `(free_disk − disk_floor) / one_target_bytes`, or `None` when either
    /// reading is UNKNOWN — an UNKNOWN disk term is ignored, never a reason
    /// to shrink the pool.
    pub disk_term: Option<u32>,
    pub disk_free_bytes: Option<u64>,
    pub one_target_bytes: Option<u64>,
    /// [`DERIVED_POOL_SIZE_CAP`].
    pub cap: u32,
}

/// Derive the pool size: `max(1, min(cpu+memory term, disk term, CAP))`.
///
/// The disk term subtracts the floor a pool of the capped candidate size
/// would demand ([`derived_disk_floor_gb`]) before dividing by one target's
/// size, so a pool is only as large as the disk can hold ABOVE its floor.
pub fn derive_pool_size(
    host: HostCapacity,
    disk_free_bytes: Option<u64>,
    disk_total_bytes: Option<u64>,
    one_target_bytes: Option<u64>,
) -> (u32, PoolSizeDerivation) {
    let cpu_memory_term = host_sizing::suggested_concurrent_builds(host);
    let candidate = cpu_memory_term.clamp(1, DERIVED_POOL_SIZE_CAP);
    let one_target = one_target_bytes.filter(|b| *b > 0);
    let disk_term = match (disk_free_bytes, one_target) {
        (Some(free), Some(target)) => {
            let floor = derived_disk_floor_gb(candidate, Some(target), disk_total_bytes)
                .saturating_mul(GIB);
            Some((free.saturating_sub(floor) / target).min(u32::MAX as u64) as u32)
        }
        _ => None,
    };
    let pool = candidate.min(disk_term.unwrap_or(u32::MAX)).max(1);
    (
        pool,
        PoolSizeDerivation {
            host_cpus: host.cpus,
            host_mem_bytes: host.mem_bytes,
            cpu_memory_term,
            disk_term,
            disk_free_bytes,
            one_target_bytes: one_target,
            cap: DERIVED_POOL_SIZE_CAP,
        },
    )
}

/// `CARGO_BUILD_JOBS` for ONE slot when `pool_size` slots may compile at once:
/// the host split evenly across the pool (`host_sizing::build_jobs(host,
/// pool_size − 1)`, i.e. `derive(share(host, pool_size)).cargo_build_jobs`).
/// A full pool's jobs therefore sum to at most the host's cores (plan D1,
/// option C — concurrency is exactly `pool_size` by construction).
pub fn per_slot_jobs(host: HostCapacity, pool_size: u32) -> u32 {
    host_sizing::build_jobs(host, pool_size.max(1) - 1)
}

/// Where a slot build's `CARGO_BUILD_JOBS` comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobsSource {
    /// The supervisor sets [`SlotJobs::per_slot`] on every cargo it spawns.
    Derived,
    /// The supervisor's own environment carries `CARGO_BUILD_JOBS`; children
    /// inherit it and the supervisor sets nothing.
    Inherited,
}

/// The per-slot cargo job setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SlotJobs {
    /// The derived per-slot value (always computed, so `/health` shows it even
    /// when an inherited value wins).
    pub per_slot: u32,
    pub source: JobsSource,
    /// The inherited `CARGO_BUILD_JOBS` text when `source == inherited`.
    pub inherited_value: Option<String>,
}

impl SlotJobs {
    /// Resolve against the supervisor's own `CARGO_BUILD_JOBS` (`env_value`).
    /// A blank value counts as unset.
    pub fn resolve(host: HostCapacity, pool_size: u32, env_value: Option<String>) -> Self {
        let per_slot = per_slot_jobs(host, pool_size);
        match env_value.filter(|v| !v.trim().is_empty()) {
            Some(v) => SlotJobs {
                per_slot,
                source: JobsSource::Inherited,
                inherited_value: Some(v),
            },
            None => SlotJobs {
                per_slot,
                source: JobsSource::Derived,
                inherited_value: None,
            },
        }
    }

    /// The value the supervisor must SET on a cargo child, or `None` when the
    /// child should inherit the supervisor's own `CARGO_BUILD_JOBS`.
    pub fn to_set(&self) -> Option<u32> {
        match self.source {
            JobsSource::Derived => Some(self.per_slot),
            JobsSource::Inherited => None,
        }
    }
}

/// Phase 4's disk rule: `max(30, min(pool_size × one_target, total / 10))`,
/// in GB (GiB, as every supervisor floor).
///
/// - UNKNOWN one-target size or disk size → exactly [`DEFAULT_MIN_FREE_DISK_GB`].
/// - Never below 30 (the shipped constant is the floor of the floor).
/// - Capped by a tenth of the volume, so a huge target cannot demand an absurd
///   floor — but the cap cannot push it under 30 either.
/// - Identity: when `pool_size × one_target ≤ 30 GB` the answer is 30.
///
/// The pool term is rounded UP to whole GB (a floor that is too small by a
/// fraction of a GB is the failure direction); the 10% cap is rounded down.
pub fn derived_disk_floor_gb(
    pool_size: u32,
    one_target_bytes: Option<u64>,
    disk_total_bytes: Option<u64>,
) -> u64 {
    let (Some(target), Some(total)) = (
        one_target_bytes.filter(|b| *b > 0),
        disk_total_bytes.filter(|b| *b > 0),
    ) else {
        return DEFAULT_MIN_FREE_DISK_GB;
    };
    let pool_bytes = pool_size.max(1) as u128 * target as u128;
    let pool_gb = pool_bytes.div_ceil(GIB as u128).min(u64::MAX as u128) as u64;
    let cap_gb = total / DISK_FLOOR_TOTAL_DIVISOR / GIB;
    DEFAULT_MIN_FREE_DISK_GB.max(pool_gb.min(cap_gb))
}

/// Where the effective disk floor came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskFloorSource {
    /// `QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB` (a `0` disables the guard).
    Override,
    /// [`derived_disk_floor_gb`] with every input known.
    Derived,
    /// An input is UNKNOWN (no one-target measurement yet, or the volume size
    /// is unreadable), so the floor is the shipped constant.
    DefaultUnknown,
}

/// The effective disk floor and the inputs it was computed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskFloor {
    pub gb: u64,
    pub source: DiskFloorSource,
    pub pool_size: u32,
    /// Largest already-built slot target dir (bytes) in the cached footprint.
    pub one_target_bytes: Option<u64>,
    pub one_target_slot: Option<usize>,
    pub disk_total_bytes: Option<u64>,
}

/// The largest non-empty slot target dir in a footprint's slot list, as
/// `(slot id, bytes)` — the cheapest honest one-target size: a measurement of
/// a real build on this machine, taken by the existing footprint walk (never
/// on a build gate). `None` when no slot has been built yet.
pub fn largest_slot(slots: &[crate::footprint::SlotFootprint]) -> Option<(usize, u64)> {
    slots
        .iter()
        .filter(|s| s.bytes > 0)
        .max_by_key(|s| s.bytes)
        .map(|s| (s.id, s.bytes))
}

/// Resolve the effective disk floor. `override_gb` is the parsed
/// `QONTINUI_SUPERVISOR_MIN_FREE_DISK_GB` and always wins; otherwise the floor
/// is derived from the cached footprint (UNKNOWN → 30).
pub fn effective_disk_floor(
    override_gb: Option<u64>,
    pool_size: u32,
    footprint: Option<&crate::footprint::FootprintSnapshot>,
) -> DiskFloor {
    let one_target = footprint.and_then(|f| largest_slot(&f.slots));
    let disk_total_bytes = footprint.and_then(|f| f.disk_total_bytes);
    let one_target_bytes = one_target.map(|(_, b)| b);
    let (gb, source) = match override_gb {
        Some(gb) => (gb, DiskFloorSource::Override),
        None => {
            let known = one_target_bytes.is_some() && disk_total_bytes.is_some_and(|t| t > 0);
            let gb = derived_disk_floor_gb(pool_size, one_target_bytes, disk_total_bytes);
            let source = if known {
                DiskFloorSource::Derived
            } else {
                DiskFloorSource::DefaultUnknown
            };
            (gb, source)
        }
    };
    DiskFloor {
        gb,
        source,
        pool_size,
        one_target_bytes,
        one_target_slot: one_target.map(|(id, _)| id),
        disk_total_bytes,
    }
}

/// The persisted one-target size estimate (see [`ONE_TARGET_ESTIMATE_FILE`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OneTargetEstimate {
    pub bytes: u64,
    pub slot: usize,
    /// RFC3339 time of the footprint walk that measured it.
    pub measured_at: String,
}

/// Read the persisted estimate from `pool_root`. `None` (UNKNOWN) when absent,
/// unreadable, malformed, or zero.
pub fn read_one_target_estimate(pool_root: &Path) -> Option<OneTargetEstimate> {
    let text = std::fs::read_to_string(pool_root.join(ONE_TARGET_ESTIMATE_FILE)).ok()?;
    serde_json::from_str::<OneTargetEstimate>(&text)
        .ok()
        .filter(|e| e.bytes > 0)
}

/// Persist the estimate under `pool_root` (best-effort; atomic replace). A
/// missing pool root is not created — no pool, nothing measured.
pub fn write_one_target_estimate(pool_root: &Path, estimate: &OneTargetEstimate) {
    if !pool_root.is_dir() {
        return;
    }
    if let Ok(json) = serde_json::to_vec_pretty(estimate) {
        if let Err(e) =
            crate::fs_atomic::atomic_write(&pool_root.join(ONE_TARGET_ESTIMATE_FILE), &json)
        {
            tracing::debug!("one-target estimate not persisted: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footprint::SlotFootprint;

    fn host(gib: u64, cpus: u32) -> HostCapacity {
        HostCapacity {
            mem_bytes: Some(gib * GIB),
            cpus,
        }
    }

    // ---- Phase 2: pool size -------------------------------------------------

    /// merytshost (48c / 368 GiB): the shared rule suggests 12; the cap holds
    /// it at the shipped 3.
    #[test]
    fn big_host_is_capped_at_the_shipped_default() {
        let (pool, d) = derive_pool_size(host(368, 48), None, None, None);
        assert_eq!(d.cpu_memory_term, 12);
        assert_eq!(pool, DERIVED_POOL_SIZE_CAP);
        assert_eq!(d.disk_term, None);
    }

    /// A small host derives fewer slots than the cap.
    #[test]
    fn small_host_derives_below_the_cap() {
        assert_eq!(derive_pool_size(host(31, 8), None, None, None).0, 2);
        assert_eq!(derive_pool_size(host(15, 8), None, None, None).0, 1);
    }

    /// Unreadable memory → the shared rule's 1, kept.
    #[test]
    fn unreadable_memory_gives_one_slot() {
        let h = HostCapacity {
            mem_bytes: None,
            cpus: 48,
        };
        assert_eq!(
            derive_pool_size(h, Some(500 * GIB), Some(2000 * GIB), Some(14 * GIB)).0,
            1
        );
    }

    /// An UNKNOWN disk term is ignored, never a reason to shrink.
    #[test]
    fn unknown_disk_term_is_ignored() {
        for (free, target) in [
            (None, Some(14 * GIB)),
            (Some(500 * GIB), None),
            (Some(500 * GIB), Some(0)),
        ] {
            let (pool, d) = derive_pool_size(host(368, 48), free, Some(2000 * GIB), target);
            assert_eq!(pool, 3);
            assert_eq!(d.disk_term, None);
        }
    }

    /// A tight disk lowers the pool; a full one still leaves 1.
    #[test]
    fn disk_term_bounds_the_pool() {
        // floor for 3 × 14 GiB on a 2000 GiB disk = 42; free 80 → (80-42)/14 = 2.
        let (pool, d) = derive_pool_size(
            host(368, 48),
            Some(80 * GIB),
            Some(2000 * GIB),
            Some(14 * GIB),
        );
        assert_eq!(d.disk_term, Some(2));
        assert_eq!(pool, 2);
        // Below the floor entirely → disk term 0 → pool 1, never 0.
        let (pool, d) = derive_pool_size(
            host(368, 48),
            Some(10 * GIB),
            Some(2000 * GIB),
            Some(14 * GIB),
        );
        assert_eq!(d.disk_term, Some(0));
        assert_eq!(pool, 1);
    }

    // ---- Phase 2: per-slot jobs --------------------------------------------

    /// `pool_size` slots' jobs sum to no more than the cores, over a sweep of
    /// host shapes, for every pool size up to the core count.
    #[test]
    fn full_pool_jobs_never_exceed_cores() {
        for cpus in [1u32, 2, 4, 8, 16, 32, 48, 64] {
            for gib in [4u64, 8, 16, 31, 64, 125, 368] {
                for pool in 1..=cpus.min(16) {
                    let j = per_slot_jobs(host(gib, cpus), pool);
                    assert!(j >= 1);
                    assert!(
                        j * pool <= cpus,
                        "{pool} slots × {j} jobs > {cpus} cores ({gib} GiB)"
                    );
                }
            }
        }
    }

    /// merytshost: 3 slots × 16 jobs = 48.
    #[test]
    fn merytshost_slot_jobs() {
        assert_eq!(per_slot_jobs(host(368, 48), 3), 16);
        assert_eq!(per_slot_jobs(host(368, 48), 1), 48);
    }

    /// The supervisor's own CARGO_BUILD_JOBS wins: nothing is set.
    #[test]
    fn inherited_cargo_build_jobs_wins() {
        let j = SlotJobs::resolve(host(368, 48), 3, Some("7".into()));
        assert_eq!(j.source, JobsSource::Inherited);
        assert_eq!(j.to_set(), None);
        assert_eq!(j.per_slot, 16);
        let j = SlotJobs::resolve(host(368, 48), 3, Some("  ".into()));
        assert_eq!(j.source, JobsSource::Derived);
        assert_eq!(j.to_set(), Some(16));
    }

    // ---- Phase 4: disk floor -----------------------------------------------

    #[test]
    fn unknown_inputs_return_exactly_the_constant() {
        for (t, d) in [
            (None, Some(2000 * GIB)),
            (Some(14 * GIB), None),
            (None, None),
            (Some(0), Some(2000 * GIB)),
            (Some(14 * GIB), Some(0)),
        ] {
            assert_eq!(derived_disk_floor_gb(3, t, d), DEFAULT_MIN_FREE_DISK_GB);
        }
    }

    /// Property sweep: never below 30; never above max(30, total/10); identity
    /// at 30 whenever pool × target ≤ 30 GB.
    #[test]
    fn floor_properties_hold_over_a_sweep() {
        for pool in 1u32..=12 {
            for target_gib in [0u64, 1, 2, 5, 9, 10, 14, 15, 40, 400] {
                for total_gib in [50u64, 100, 300, 1000, 2000, 20000] {
                    let t = Some(target_gib * GIB);
                    let total = Some(total_gib * GIB);
                    let f = derived_disk_floor_gb(pool, t, total);
                    assert!(f >= DEFAULT_MIN_FREE_DISK_GB);
                    assert!(f <= DEFAULT_MIN_FREE_DISK_GB.max(total_gib / 10));
                    if pool as u64 * target_gib <= DEFAULT_MIN_FREE_DISK_GB {
                        assert_eq!(f, DEFAULT_MIN_FREE_DISK_GB, "identity: {pool}×{target_gib}");
                    }
                }
            }
        }
    }

    /// merytshost measured 2026-10-22: slot-0 15_213_074_718 bytes on a
    /// 2_005_795_418_112-byte volume, pool 3 → ceil(42.5) = 43 GB.
    #[test]
    fn merytshost_floor() {
        assert_eq!(
            derived_disk_floor_gb(3, Some(15_213_074_718), Some(2_005_795_418_112)),
            43
        );
        // Capped by a tenth of a small disk, but never under 30.
        assert_eq!(
            derived_disk_floor_gb(3, Some(40 * GIB), Some(500 * GIB)),
            50
        );
        assert_eq!(
            derived_disk_floor_gb(3, Some(40 * GIB), Some(200 * GIB)),
            30
        );
    }

    fn snapshot(slots: &[u64], total: Option<u64>) -> crate::footprint::FootprintSnapshot {
        crate::footprint::FootprintSnapshot {
            computed_at: String::new(),
            disk_free_bytes: None,
            disk_total_bytes: total,
            disk_mount: None,
            memory: Default::default(),
            build_pool: Default::default(),
            slots: slots
                .iter()
                .enumerate()
                .map(|(id, &bytes)| SlotFootprint { id, bytes })
                .collect(),
            lkg_bytes: 0,
            spawn_containers: Default::default(),
            exe_copies: Default::default(),
        }
    }

    #[test]
    fn effective_floor_sources() {
        // No footprint yet → UNKNOWN → 30.
        let f = effective_disk_floor(None, 3, None);
        assert_eq!((f.gb, f.source), (30, DiskFloorSource::DefaultUnknown));
        // Unbuilt slots → UNKNOWN → 30.
        let s = snapshot(&[0, 0, 0], Some(2000 * GIB));
        let f = effective_disk_floor(None, 3, Some(&s));
        assert_eq!((f.gb, f.source), (30, DiskFloorSource::DefaultUnknown));
        // Largest built slot is the one-target estimate.
        let s = snapshot(&[15 * GIB, 2 * GIB, 5 * GIB], Some(2000 * GIB));
        let f = effective_disk_floor(None, 3, Some(&s));
        assert_eq!((f.gb, f.source), (45, DiskFloorSource::Derived));
        assert_eq!(f.one_target_slot, Some(0));
        // The env override always wins, including 0 (guard disabled).
        let f = effective_disk_floor(Some(0), 3, Some(&s));
        assert_eq!((f.gb, f.source), (0, DiskFloorSource::Override));
    }

    #[test]
    fn estimate_round_trips_and_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_one_target_estimate(dir.path()), None);
        let e = OneTargetEstimate {
            bytes: 14 * GIB,
            slot: 0,
            measured_at: "2026-10-22T00:00:00Z".into(),
        };
        write_one_target_estimate(dir.path(), &e);
        assert_eq!(read_one_target_estimate(dir.path()), Some(e));
        std::fs::write(dir.path().join(ONE_TARGET_ESTIMATE_FILE), "{").unwrap();
        assert_eq!(read_one_target_estimate(dir.path()), None);
        // A missing pool root is not created.
        let missing = dir.path().join("nope");
        write_one_target_estimate(
            &missing,
            &OneTargetEstimate {
                bytes: 1,
                slot: 0,
                measured_at: String::new(),
            },
        );
        assert!(!missing.exists());
    }
}
